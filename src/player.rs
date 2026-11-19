//! The seam between the core and whatever plays audio.
//!
//! The core only needs the handful of operations below, so it takes a
//! `&dyn Player` instead of the mpv child process. Desktop passes
//! `mpv::Mpv`; a mobile front-end can bring its own engine; tests use
//! `FakePlayer`. Method names and semantics match the `Mpv` methods
//! they wrap (see src/mpv.rs for the measured behaviour of each).
//!
//! `dyn` rather than a generic: `CoreCtx` is a struct of borrowed
//! handles built per op, and a type parameter would spread through
//! every core signature and the shellsong host for no gain; one
//! virtual call per player op is invisible next to an IPC round trip.

use std::collections::{HashMap, HashSet};
use std::sync::Mutex;

use anyhow::{anyhow, Result};
use serde_json::{json, Value};

use crate::mpv::Mpv;

pub trait Player {
    // playlist
    fn load_file(&self, path_or_url: &str) -> Result<Option<u64>>;
    fn playlist_append(&self, path_or_url: &str) -> Result<Option<u64>>;
    fn playlist_append_batch(&self, paths: &[String]) -> Vec<Result<Option<u64>>>;
    fn playlist_remove(&self, index: usize) -> Result<()>;
    fn playlist_move(&self, from: usize, to: usize) -> Result<()>;
    fn playlist_clear(&self) -> Result<()>;
    fn playlist_next(&self) -> Result<()>;
    /// The raw playlist: an array of `{id, current, ...}` entries.
    fn playlist(&self) -> Result<Value>;
    /// 0-based index to jump to.
    fn set_playlist_pos(&self, index: usize) -> Result<()>;

    // transport
    fn toggle_pause(&self) -> Result<()>;
    fn set_pause(&self, paused: bool) -> Result<()>;
    fn set_keep_open(&self, hold: bool) -> Result<()>;
    fn seek(&self, seconds: f64, relative: bool) -> Result<()>;
    fn set_loop_file(&self, mode: &str) -> Result<()>;
    fn set_speed(&self, speed: f64) -> Result<()>;

    // audio
    fn set_volume(&self, vol: f64) -> Result<()>;
    fn set_mute(&self, muted: bool) -> Result<()>;
    fn set_af(&self, filter: &str) -> Result<()>;

    // video / titling
    /// mpv's `vid` property: `"auto"` or `"no"`.
    fn set_video_track(&self, mode: &str) -> Result<()>;
    fn set_media_title(&self, title: &str) -> Result<()>;
    fn request_video_window(&self) -> Result<()>;

    // reads
    fn time_pos(&self) -> Option<f64>;
    fn volume(&self) -> Option<f64>;
    fn duration(&self) -> Option<f64>;

    /// Ask the engine to push the properties the render path reads.
    fn observe_playback(&self) -> Result<()>;

    // host loop
    /// `set_volume` without waiting for the engine's answer; for ramps
    /// that run every frame.
    fn set_volume_nowait(&self, vol: f64) -> Result<()>;
    fn sub_add(&self, path: &str) -> Result<()>;
    /// The next pending engine event, if any; never blocks.
    fn poll_event(&self) -> Option<crate::mpv::Event>;
    /// Why the engine stopped on its own, once it has.
    fn exited(&self) -> Option<String>;
    /// Whether a background command batch is still running.
    fn background_active(&self) -> bool;
}

impl Player for Mpv {
    fn load_file(&self, path_or_url: &str) -> Result<Option<u64>> {
        Mpv::load_file(self, path_or_url)
    }
    fn playlist_append(&self, path_or_url: &str) -> Result<Option<u64>> {
        Mpv::playlist_append(self, path_or_url)
    }
    fn playlist_append_batch(&self, paths: &[String]) -> Vec<Result<Option<u64>>> {
        Mpv::playlist_append_batch(self, paths)
    }
    fn playlist_remove(&self, index: usize) -> Result<()> {
        Mpv::playlist_remove(self, index)
    }
    fn playlist_move(&self, from: usize, to: usize) -> Result<()> {
        Mpv::playlist_move(self, from, to)
    }
    fn playlist_clear(&self) -> Result<()> {
        Mpv::playlist_clear(self)
    }
    fn playlist_next(&self) -> Result<()> {
        Mpv::playlist_next(self)
    }
    fn playlist(&self) -> Result<Value> {
        self.get_property("playlist")
    }
    fn set_playlist_pos(&self, index: usize) -> Result<()> {
        self.set_property("playlist-pos", json!(index))
    }
    fn toggle_pause(&self) -> Result<()> {
        Mpv::toggle_pause(self)
    }
    fn set_pause(&self, paused: bool) -> Result<()> {
        Mpv::set_pause(self, paused)
    }
    fn set_keep_open(&self, hold: bool) -> Result<()> {
        Mpv::set_keep_open(self, hold)
    }
    fn seek(&self, seconds: f64, relative: bool) -> Result<()> {
        Mpv::seek(self, seconds, relative)
    }
    fn set_loop_file(&self, mode: &str) -> Result<()> {
        Mpv::set_loop_file(self, mode)
    }
    fn set_speed(&self, speed: f64) -> Result<()> {
        Mpv::set_speed(self, speed)
    }
    fn set_volume(&self, vol: f64) -> Result<()> {
        Mpv::set_volume(self, vol)
    }
    fn set_mute(&self, muted: bool) -> Result<()> {
        self.set_property("mute", json!(muted))
    }
    fn set_af(&self, filter: &str) -> Result<()> {
        Mpv::set_af(self, filter)
    }
    fn set_video_track(&self, mode: &str) -> Result<()> {
        self.set_property("vid", json!(mode))
    }
    fn set_media_title(&self, title: &str) -> Result<()> {
        self.set_property("force-media-title", json!(title))
    }
    fn request_video_window(&self) -> Result<()> {
        self.set_property("force-window", json!(true))
    }
    fn time_pos(&self) -> Option<f64> {
        self.get_property("time-pos").ok().and_then(|v| v.as_f64())
    }
    fn volume(&self) -> Option<f64> {
        self.get_property("volume").ok().and_then(|v| v.as_f64())
    }
    fn duration(&self) -> Option<f64> {
        self.get_property(crate::mpv::PROP_DURATION)
            .ok()
            .and_then(|v| v.as_f64())
    }
    fn observe_playback(&self) -> Result<()> {
        Mpv::observe_playback(self)
    }
    fn set_volume_nowait(&self, vol: f64) -> Result<()> {
        Mpv::set_volume_nowait(self, vol)
    }
    fn sub_add(&self, path: &str) -> Result<()> {
        Mpv::sub_add(self, path)
    }
    fn poll_event(&self) -> Option<crate::mpv::Event> {
        self.events().try_recv().ok()
    }
    fn exited(&self) -> Option<String> {
        Mpv::exited(self)
    }
    fn background_active(&self) -> bool {
        Mpv::background_active(self)
    }
}

/// A `Player` that records every call as a string (`"set_volume(50)"`)
/// and answers from scripted values. Not an mpv model: it does not
/// track a playlist or a clock, only what it was told to return.
#[derive(Default)]
pub struct FakePlayer {
    calls: Mutex<Vec<String>>,
    failing: Mutex<HashSet<String>>,
    reads: Mutex<HashMap<&'static str, Value>>,
    playlist: Mutex<Option<Value>>,
    next_id: Mutex<u64>,
}

impl FakePlayer {
    pub fn new() -> Self {
        Self::default()
    }

    /// Every call so far, in order.
    pub fn calls(&self) -> Vec<String> {
        self.calls.lock().unwrap().clone()
    }

    /// Make every call to `method` (e.g. `"playlist_next"`) return Err.
    pub fn fail(&self, method: &str) {
        self.failing.lock().unwrap().insert(method.to_string());
    }

    /// Script `time_pos`, `volume` or `duration`.
    pub fn script_read(&self, name: &'static str, value: f64) {
        self.reads.lock().unwrap().insert(name, json!(value));
    }

    /// Script what `playlist()` returns.
    pub fn script_playlist(&self, value: Value) {
        *self.playlist.lock().unwrap() = Some(value);
    }

    fn rec(&self, method: &str, args: String) -> Result<()> {
        self.calls.lock().unwrap().push(format!("{method}({args})"));
        if self.failing.lock().unwrap().contains(method) {
            Err(anyhow!("fake player: {method} failed"))
        } else {
            Ok(())
        }
    }

    fn id(&self) -> Option<u64> {
        let mut n = self.next_id.lock().unwrap();
        *n += 1;
        Some(*n)
    }

    fn read(&self, method: &'static str) -> Option<f64> {
        self.calls.lock().unwrap().push(format!("{method}()"));
        self.reads
            .lock()
            .unwrap()
            .get(method)
            .and_then(Value::as_f64)
    }
}

impl Player for FakePlayer {
    fn load_file(&self, p: &str) -> Result<Option<u64>> {
        self.rec("load_file", p.to_string()).map(|()| self.id())
    }
    fn playlist_append(&self, p: &str) -> Result<Option<u64>> {
        self.rec("playlist_append", p.to_string())
            .map(|()| self.id())
    }
    fn playlist_append_batch(&self, paths: &[String]) -> Vec<Result<Option<u64>>> {
        paths.iter().map(|p| self.playlist_append(p)).collect()
    }
    fn playlist_remove(&self, i: usize) -> Result<()> {
        self.rec("playlist_remove", i.to_string())
    }
    fn playlist_move(&self, from: usize, to: usize) -> Result<()> {
        self.rec("playlist_move", format!("{from}, {to}"))
    }
    fn playlist_clear(&self) -> Result<()> {
        self.rec("playlist_clear", String::new())
    }
    fn playlist_next(&self) -> Result<()> {
        self.rec("playlist_next", String::new())
    }
    fn playlist(&self) -> Result<Value> {
        self.rec("playlist", String::new())?;
        self.playlist
            .lock()
            .unwrap()
            .clone()
            .ok_or_else(|| anyhow!("fake player: no playlist scripted"))
    }
    fn set_playlist_pos(&self, i: usize) -> Result<()> {
        self.rec("set_playlist_pos", i.to_string())
    }
    fn toggle_pause(&self) -> Result<()> {
        self.rec("toggle_pause", String::new())
    }
    fn set_pause(&self, paused: bool) -> Result<()> {
        self.rec("set_pause", paused.to_string())
    }
    fn set_keep_open(&self, hold: bool) -> Result<()> {
        self.rec("set_keep_open", hold.to_string())
    }
    fn seek(&self, seconds: f64, relative: bool) -> Result<()> {
        self.rec("seek", format!("{seconds}, {relative}"))
    }
    fn set_loop_file(&self, mode: &str) -> Result<()> {
        self.rec("set_loop_file", mode.to_string())
    }
    fn set_speed(&self, speed: f64) -> Result<()> {
        self.rec("set_speed", speed.to_string())
    }
    fn set_volume(&self, vol: f64) -> Result<()> {
        self.rec("set_volume", vol.to_string())
    }
    fn set_mute(&self, muted: bool) -> Result<()> {
        self.rec("set_mute", muted.to_string())
    }
    fn set_af(&self, filter: &str) -> Result<()> {
        self.rec("set_af", filter.to_string())
    }
    fn set_video_track(&self, mode: &str) -> Result<()> {
        self.rec("set_video_track", mode.to_string())
    }
    fn set_media_title(&self, title: &str) -> Result<()> {
        self.rec("set_media_title", title.to_string())
    }
    fn request_video_window(&self) -> Result<()> {
        self.rec("request_video_window", String::new())
    }
    fn time_pos(&self) -> Option<f64> {
        self.read("time_pos")
    }
    fn volume(&self) -> Option<f64> {
        self.read("volume")
    }
    fn duration(&self) -> Option<f64> {
        self.read("duration")
    }
    fn observe_playback(&self) -> Result<()> {
        self.rec("observe_playback", String::new())
    }
    fn set_volume_nowait(&self, vol: f64) -> Result<()> {
        self.rec("set_volume_nowait", vol.to_string())
    }
    fn sub_add(&self, path: &str) -> Result<()> {
        self.rec("sub_add", path.to_string())
    }
    fn poll_event(&self) -> Option<crate::mpv::Event> {
        None
    }
    fn exited(&self) -> Option<String> {
        None
    }
    fn background_active(&self) -> bool {
        false
    }
}
