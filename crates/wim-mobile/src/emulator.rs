//! A `Player` for phones: a playlist model on top of one native engine
//! that plays a single url at a time.
//!
//! Scope is the mobile MVP: the queue advancing on `on_ended`,
//! single-file loop, transport, volume. mpv-only features (filters,
//! mute, speed, video, subtitles) decline with a message the session
//! prints as an error bubble.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{anyhow, Result};
use serde_json::{json, Value};
use wim::mpv::PROP_TIME_POS;
use wim::mpv::{Event, PROP_DURATION, PROP_IDLE_ACTIVE, PROP_PAUSE, PROP_PLAYLIST_POS};
use wim::player::Player;

use crate::PlatformPlayer;

struct Entry {
    id: u64,
    url: String,
}

/// A call for the native engine, made after the state lock is
/// released so a callback that re-enters `on_time` cannot deadlock.
enum Act {
    Load(String),
    Play,
    Pause,
    Seek(f64),
    Volume(f64),
    Stop,
}

/// A generated silence has no url to hand the engine: it is a timer
/// (mpv's lavfi source would need ffmpeg filters the phone lacks).
struct Silence {
    secs: f64,
    elapsed: f64,
    /// The last position pushed as an event; see `run_clock`.
    reported: f64,
}

struct State {
    silence: Option<Silence>,
    last_tick: Instant,
    entries: Vec<Entry>,
    pos: Option<usize>,
    next_id: u64,
    paused: bool,
    /// mpv's scale, 0 to 100.
    volume: f64,
    loop_file: bool,
    time: Option<f64>,
    duration: Option<f64>,
    events: VecDeque<Event>,
    acts: Vec<Act>,
}

impl State {
    fn prop(&mut self, name: &str, value: Value) {
        self.events.push_back(Event::PropertyChange {
            name: name.to_string(),
            value,
        });
    }

    fn push(&mut self, url: &str) -> u64 {
        self.next_id += 1;
        let id = self.next_id;
        self.entries.push(Entry {
            id,
            url: url.to_string(),
        });
        id
    }

    /// Begins entry `i` from the top.
    fn start(&mut self, i: usize) {
        let (id, url) = (self.entries[i].id, self.entries[i].url.clone());
        self.pos = Some(i);
        self.paused = false;
        self.time = Some(0.0);
        self.duration = None;
        self.silence = silence_secs(&url).map(|secs| Silence {
            secs,
            elapsed: 0.0,
            reported: 0.0,
        });
        if let Some(silence) = &self.silence {
            self.duration = Some(silence.secs);
            self.acts.push(Act::Stop);
        } else {
            self.acts.push(Act::Load(url));
        }
        self.events
            .push_back(Event::StartFile { entry_id: Some(id) });
        self.prop(PROP_PLAYLIST_POS, json!(i));
        self.prop(PROP_IDLE_ACTIVE, json!(false));
        self.prop(PROP_PAUSE, json!(false));
    }

    /// What follows the current entry: itself under loop-file when
    /// `natural`, else the next one, else idle.
    fn advance(&mut self, natural: bool) {
        match self.pos {
            Some(i) if natural && self.loop_file => self.start(i),
            Some(i) if i + 1 < self.entries.len() => self.start(i + 1),
            _ => self.go_idle(),
        }
    }

    /// The playlist ran out: mpv reports an idle, empty player.
    fn go_idle(&mut self) {
        self.entries.clear();
        self.silence = None;
        self.pos = None;
        self.time = None;
        self.duration = None;
        self.acts.push(Act::Stop);
        self.prop(PROP_TIME_POS, Value::Null);
        self.prop(PROP_IDLE_ACTIVE, json!(true));
    }
}

/// The seconds in `wim::core::silence_url`'s output.
fn silence_secs(url: &str) -> Option<f64> {
    url.strip_prefix("av://lavfi:anullsrc=d=")?.parse().ok()
}

pub struct PlaylistEmulator {
    native: Arc<dyn PlatformPlayer>,
    state: Mutex<State>,
}

fn decline(what: &str) -> anyhow::Error {
    anyhow!("{what} isn't available on mobile")
}

impl PlaylistEmulator {
    pub fn new(native: Arc<dyn PlatformPlayer>) -> Self {
        PlaylistEmulator {
            native,
            state: Mutex::new(State {
                silence: None,
                last_tick: Instant::now(),
                entries: Vec::new(),
                pos: None,
                next_id: 0,
                paused: false,
                volume: 100.0,
                loop_file: false,
                time: None,
                duration: None,
                events: VecDeque::new(),
                acts: Vec::new(),
            }),
        }
    }

    fn with<R>(&self, f: impl FnOnce(&mut State) -> R) -> R {
        let (out, acts) = {
            let mut st = self.state.lock().unwrap();
            let out = f(&mut st);
            (out, std::mem::take(&mut st.acts))
        };
        for act in acts {
            match act {
                Act::Load(url) => self.native.load(url),
                Act::Play => self.native.play(),
                Act::Pause => self.native.pause(),
                Act::Seek(secs) => self.native.seek(secs),
                Act::Volume(level) => self.native.set_volume(level),
                Act::Stop => self.native.stop(),
            }
        }
        out
    }

    /// The engine's position report.
    pub fn on_time(&self, secs: f64, duration: f64) {
        self.with(|s| {
            if s.silence.is_some() {
                return;
            }
            s.time = Some(secs);
            s.prop(PROP_TIME_POS, json!(secs));
            if duration > 0.0 {
                s.duration = Some(duration);
                s.prop(PROP_DURATION, json!(duration));
            }
        });
    }

    /// Advances a silence by `dt` of playing time; it ends by itself.
    pub fn run_clock(&self, dt: Duration) {
        self.with(|s| {
            let Some(silence) = s.silence.as_mut().filter(|_| !s.paused) else {
                return;
            };
            silence.elapsed = (silence.elapsed + dt.as_secs_f64()).min(silence.secs);
            let (elapsed, over) = (silence.elapsed, silence.elapsed >= silence.secs);
            // quarter-second steps: `poll_event` runs this on every
            // call, and an event per call would never drain.
            if !over && elapsed - silence.reported < 0.25 {
                return;
            }
            silence.reported = elapsed;
            s.time = Some(elapsed);
            s.prop(PROP_TIME_POS, json!(elapsed));
            if over {
                s.advance(true);
            }
        });
    }

    /// The engine finished the current url.
    pub fn on_ended(&self) {
        self.with(|s| s.advance(true));
    }
}

impl Player for PlaylistEmulator {
    fn load_file(&self, url: &str) -> Result<Option<u64>> {
        self.with(|s| {
            s.entries.clear();
            let id = s.push(url);
            s.start(0);
            Ok(Some(id))
        })
    }
    fn playlist_append(&self, url: &str) -> Result<Option<u64>> {
        self.with(|s| Ok(Some(s.push(url))))
    }
    fn playlist_append_batch(&self, paths: &[String]) -> Vec<Result<Option<u64>>> {
        paths.iter().map(|p| self.playlist_append(p)).collect()
    }
    fn playlist_remove(&self, index: usize) -> Result<()> {
        self.with(|s| {
            if index >= s.entries.len() {
                return Err(anyhow!("no playlist entry {index}"));
            }
            s.entries.remove(index);
            match s.pos {
                Some(p) if p == index && index < s.entries.len() => s.start(index),
                Some(p) if p == index => s.go_idle(),
                Some(p) if index < p => s.pos = Some(p - 1),
                _ => {}
            }
            Ok(())
        })
    }
    fn playlist_move(&self, from: usize, to: usize) -> Result<()> {
        self.with(|s| {
            if from >= s.entries.len() || to >= s.entries.len() {
                return Err(anyhow!("playlist move out of range"));
            }
            let playing = s.pos.map(|p| s.entries[p].id);
            let entry = s.entries.remove(from);
            s.entries.insert(to, entry);
            s.pos = playing.and_then(|id| s.entries.iter().position(|e| e.id == id));
            Ok(())
        })
    }
    fn playlist_clear(&self) -> Result<()> {
        // mpv's playlist-clear keeps what is playing.
        self.with(|s| {
            if let Some(p) = s.pos {
                let keep = s.entries.swap_remove(p);
                s.entries = vec![keep];
                s.pos = Some(0);
            } else {
                s.entries.clear();
            }
            Ok(())
        })
    }
    fn playlist_next(&self) -> Result<()> {
        self.with(|s| {
            s.advance(false);
            Ok(())
        })
    }
    fn playlist(&self) -> Result<Value> {
        self.with(|s| {
            Ok(Value::Array(
                s.entries
                    .iter()
                    .enumerate()
                    .map(|(i, e)| {
                        json!({"id": e.id, "filename": e.url, "current": s.pos == Some(i)})
                    })
                    .collect(),
            ))
        })
    }
    fn set_playlist_pos(&self, index: usize) -> Result<()> {
        self.with(|s| {
            if index >= s.entries.len() {
                return Err(anyhow!("no playlist entry {index}"));
            }
            s.start(index);
            Ok(())
        })
    }

    fn toggle_pause(&self) -> Result<()> {
        let paused = self.with(|s| s.paused);
        self.set_pause(!paused)
    }
    fn set_pause(&self, paused: bool) -> Result<()> {
        self.with(|s| {
            if s.pos.is_some() && s.paused != paused {
                s.paused = paused;
                if s.silence.is_none() {
                    s.acts.push(if paused { Act::Pause } else { Act::Play });
                }
                s.prop(PROP_PAUSE, json!(paused));
            }
            Ok(())
        })
    }
    fn set_keep_open(&self, _hold: bool) -> Result<()> {
        Ok(())
    }
    fn seek(&self, seconds: f64, relative: bool) -> Result<()> {
        self.with(|s| {
            if s.pos.is_none() {
                return Err(anyhow!("nothing is playing"));
            }
            let base = if relative { s.time.unwrap_or(0.0) } else { 0.0 };
            let target = (base + seconds).max(0.0);
            s.time = Some(target);
            match s.silence.as_mut() {
                Some(silence) => silence.elapsed = target.min(silence.secs),
                None => s.acts.push(Act::Seek(target)),
            }
            s.prop(PROP_TIME_POS, json!(target));
            Ok(())
        })
    }
    fn set_loop_file(&self, mode: &str) -> Result<()> {
        self.with(|s| {
            s.loop_file = match mode {
                "inf" | "yes" => true,
                "no" => false,
                other => return Err(anyhow!("loop mode '{other}' isn't available on mobile")),
            };
            Ok(())
        })
    }
    fn set_speed(&self, speed: f64) -> Result<()> {
        if (speed - 1.0).abs() < f64::EPSILON {
            Ok(())
        } else {
            Err(decline("changing speed"))
        }
    }

    fn set_volume(&self, vol: f64) -> Result<()> {
        self.with(|s| {
            s.volume = vol.clamp(0.0, 100.0);
            s.acts.push(Act::Volume(s.volume / 100.0));
            Ok(())
        })
    }
    fn set_mute(&self, _muted: bool) -> Result<()> {
        Err(decline("mute"))
    }
    fn set_af(&self, _filter: &str) -> Result<()> {
        Err(decline("the equalizer"))
    }

    fn set_video_track(&self, mode: &str) -> Result<()> {
        match mode {
            "no" => Ok(()),
            _ => Err(decline("video")),
        }
    }
    fn set_media_title(&self, _title: &str) -> Result<()> {
        Ok(())
    }
    fn request_video_window(&self) -> Result<()> {
        Err(decline("video"))
    }

    fn time_pos(&self) -> Option<f64> {
        self.with(|s| s.time)
    }
    fn volume(&self) -> Option<f64> {
        self.with(|s| Some(s.volume))
    }
    fn duration(&self) -> Option<f64> {
        self.with(|s| s.duration)
    }
    fn observe_playback(&self) -> Result<()> {
        Ok(())
    }

    fn set_volume_nowait(&self, vol: f64) -> Result<()> {
        self.set_volume(vol)
    }
    fn sub_add(&self, _path: &str) -> Result<()> {
        Err(decline("subtitles"))
    }
    fn poll_event(&self) -> Option<Event> {
        let dt = self.with(|s| std::mem::replace(&mut s.last_tick, Instant::now()).elapsed());
        self.run_clock(dt);
        self.with(|s| s.events.pop_front())
    }
    fn exited(&self) -> Option<String> {
        None
    }
    fn background_active(&self) -> bool {
        false
    }
}
