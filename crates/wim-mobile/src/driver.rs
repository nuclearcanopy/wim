//! The per-tick loop shellswim's `Host` runs, minus the terminal, mpv
//! and the editors that need a screen. Lines go in one per message;
//! console output comes out as bubbles attributed to the message that
//! was running when it was written.

use std::collections::VecDeque;
use std::sync::Arc;

use wim::core::{self, CoreCtx, Drive, Intent};
use wim::player::Player;
use wim::shellsong::{LineKind, Op};
use wim::swimsession::SwimSession;
use wim::ui::results_list::label;

use crate::{Bubble, BubbleKind};

/// Programs and drains run per tick, so a burst of queued messages
/// cannot starve the UI thread.
const MAX_STEPS: usize = 16;
/// Console width for echoed search results: a phone bubble.
const WIDTH: usize = 40;

pub struct Driver {
    pub session: SwimSession,
    pub player: Arc<dyn Player + Send + Sync>,
    pending: VecDeque<(u64, String)>,
    active: Option<u64>,
    next_id: u64,
}

macro_rules! core_ctx {
    ($d:expr, $now:expr, $np:expr) => {
        CoreCtx {
            cfg: &$d.session.cfg,
            player: &*$d.player,
            app: &mut $d.session.app,
            notifications: &mut $d.session.notifications,
            queue: &mut $d.session.play_queue,
            previous_track: &mut $d.session.previous_track,
            cache: &$d.session.cache_index,
            download_tx: &$d.session.download_tx,
            downloading: &mut $d.session.downloading,
            scrobble: &mut $d.session.scrobble,
            subtitle_tx: &$d.session.subtitle_tx,
            console: &mut $d.session.console,
            now: $now,
            now_playing: &$np,
            favorites: &mut $d.session.favorites,
            remote_starred: &mut $d.session.remote_starred,
            lyrics_client: &$d.session.lyrics_client,
            lyrics_loader: &mut $d.session.lyrics_loader,
            star_tx: &$d.session.star_tx,
            track_resolver: &mut $d.session.track_resolver,
            base_volume: &mut $d.session.base_volume,
            muted: $d.session.playback.muted,
            crossfade_enabled: &mut $d.session.crossfade_enabled,
            overridden: &mut $d.session.overridden,
            undo: &mut $d.session.undo,
            between_tracks: &mut $d.session.between_tracks,
            fade_in_started_at: &mut $d.session.fade_in_started_at,
            fade: &mut $d.session.fade,
            sleep_due: &mut $d.session.sleep_due,
            resume_due: &mut $d.session.resume_due,
            scan_rx: &mut $d.session.scan_rx,
            remote_search_tx: &$d.session.remote_search_tx,
            settle: &mut $d.session.settle,
            saved_queues: &mut $d.session.saved_queues,
            vars: &mut $d.session.vars,
            bindings: &mut $d.session.bindings,
        }
    };
}

/// Ops that need mpv features, a terminal or an account the app does
/// not have. The reason is the bubble text.
fn declined(op: &Op) -> Option<&'static str> {
    Some(match op {
        Op::Mute(_) => "mute isn't available on mobile; use volume(0)",
        Op::SetEqPreset(_) => "the equalizer isn't available on mobile",
        Op::SetSpeed(_) | Op::SpeedUp | Op::SpeedDown => "speed isn't available on mobile",
        Op::OpenVideoPlayer => "video isn't available on mobile",
        Op::Quit => "there is nothing to quit on mobile",
        Op::TorBoxSearch(_)
        | Op::TorBoxStatus
        | Op::TorBoxList(_)
        | Op::TorBoxAdd { .. }
        | Op::TorBoxPlay { .. }
        | Op::TorBoxQueue { .. }
        | Op::TorBoxControl { .. } => "TorBox isn't available on mobile",
        _ => return None,
    })
}

impl Driver {
    pub fn new(session: SwimSession, player: Arc<dyn Player + Send + Sync>) -> Driver {
        Driver {
            session,
            player,
            pending: VecDeque::new(),
            active: None,
            next_id: 0,
        }
    }

    /// Blocks until the playback cache, the warmed catalog and the
    /// local scan have landed, as the one-shot shellswim does. Tests
    /// and the example need it; an app has a human's typing delay.
    pub fn wait_ready(&mut self) {
        let _ = self.session.wait_startup_cache();
        self.session.wait_startup_catalog();
        let end = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while self.session.app.scanning_library && std::time::Instant::now() < end {
            self.session.pump_library();
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
    }

    /// Whether a submitted command is still running or waiting.
    pub fn busy(&self) -> bool {
        self.active.is_some() || !self.pending.is_empty() || self.session.has_program_work()
    }

    /// Queues one command; ids start at 1 (0 means "nobody asked").
    pub fn submit(&mut self, text: &str) -> u64 {
        self.next_id += 1;
        self.pending
            .push_back((self.next_id, text.trim().to_string()));
        self.next_id
    }

    pub fn tick(&mut self) -> Vec<Bubble> {
        let mut out = Vec::new();
        self.pump_playback();
        self.drain(0, &mut out);
        for _ in 0..MAX_STEPS {
            if self.active.is_none() {
                if let Some((id, line)) = self.pending.pop_front() {
                    self.active = Some(id);
                    self.begin(id, &line);
                }
            }
            self.pump_background();
            let drive = if self.session.has_program_work() {
                self.step()
            } else {
                Drive::Done
            };
            self.drain(self.active.unwrap_or(0), &mut out);
            if drive == Drive::Await {
                break;
            }
            if !self.session.has_program_work() {
                self.active = None;
                if self.pending.is_empty() {
                    break;
                }
            }
        }
        out
    }

    fn begin(&mut self, id: u64, line: &str) {
        if line.is_empty() || line.contains('\n') {
            self.session.console.fail("send one command per message");
            self.active = Some(id);
            return;
        }
        self.session.begin(line);
    }

    fn step(&mut self) -> Drive {
        self.session.take_next_program();
        let Some(program) = self.session.program.as_mut() else {
            return Drive::Done;
        };
        let now = self.session.start.elapsed();
        let now_playing = self.session.play_queue.current().cloned();
        let mut surf = self.session.swim.borrowed();
        let mut core = core_ctx!(self, now, now_playing);
        let outcome = core::drive_with(
            program,
            &mut core,
            &mut surf,
            |c| c.settle.settled(c.now),
            |op, core, surf| {
                if let Some(why) = declined(&op) {
                    core.console.fail(why);
                    return core::OpDrive::Continue;
                }
                match core::apply_op(op, core, surf) {
                    core::Dispatch::Continue => core::OpDrive::Continue,
                    core::Dispatch::Quit => core::OpDrive::Quit,
                }
            },
        );
        if outcome != Drive::Await {
            self.session.program = None;
        }
        outcome
    }

    /// Engine events first: a track change is the player talking, not
    /// the answer to whatever command is running.
    fn pump_playback(&mut self) {
        self.session.pump_player_health(&*self.player);
        let advanced = self.session.pump_player_events(&*self.player);
        if let Some(track) = advanced {
            if self.session.play_queue.current().is_some() {
                self.session
                    .console
                    .note(format!("now playing: {}", label(&track)));
            }
            let now = self.session.start.elapsed();
            let np = Some(track.clone());
            let started = {
                let mut core = core_ctx!(self, now, np);
                core::track_started(&track, &mut core)
            };
            if !started.ops.is_empty() {
                self.session.swim.queued = Some(Box::new(wim::shellsong::Program::of(started.ops)));
            }
            if let Some(bound) = started.bound {
                self.session.console.note(bound);
            }
        }
    }

    fn pump_background(&mut self) {
        self.session.pump_failures();
        self.session.pump_echo(WIDTH);
        self.session.pump_timers(&*self.player);
        self.session.pump_library();
        self.session.pump_replies(&*self.player, WIDTH);
        self.pump_resolver();
        self.reject_screens();
        for note in self.session.notifications.drain_text() {
            self.session.console.note(note);
        }
    }

    /// An album or artist fan-out finished.
    fn pump_resolver(&mut self) {
        while let Some((into, name, tracks)) = self.session.track_resolver.poll() {
            let now = self.session.start.elapsed();
            let now_playing = self.session.play_queue.current().cloned();
            let mut surf = self.session.swim.borrowed();
            let mut core = core_ctx!(self, now, now_playing);
            match tracks {
                Err(why) => core.console.note(why),
                Ok(t) if t.is_empty() => core
                    .console
                    .note(format!("nothing to {} in {name}", into.verb())),
                Ok(t) => match into {
                    Intent::Play => match core::start_session(t, false, &mut core, &mut surf) {
                        Ok(status) => core.console.note(status),
                        Err(why) => core.console.fail(why),
                    },
                    Intent::Back => core::append_resolved(&t, &mut core),
                    Intent::Front => match core::front_resolved(&t, &mut core) {
                        Ok(n) => core.console.note(format!("queued {n} tracks next")),
                        Err(why) => core.console.note(why),
                    },
                    Intent::Browse => t.iter().for_each(|i| core.console.note(label(i))),
                    Intent::Download => core.console.fail("downloads aren't available on mobile"),
                },
            }
        }
    }

    /// Full-screen editors have no chat equivalent.
    fn reject_screens(&mut self) {
        let swim = &mut self.session.swim;
        let screens = [
            (&mut swim.keybinds_requested, "the keybinds editor"),
            (&mut swim.eq_requested, "the equalizer"),
            (&mut swim.settings_requested, "the settings editor"),
            (&mut swim.undo_requested, "the undo tree"),
        ];
        for (flag, name) in screens {
            if std::mem::take(flag) {
                self.session
                    .console
                    .fail(format!("{name} isn't available on mobile"));
            }
        }
        self.session.swim.list_requested = None;
        self.session.swim.lyrics_requested = false;
    }

    /// Moves what the console and the notification area hold into
    /// bubbles. `id` 0 marks the player speaking unprompted.
    fn drain(&mut self, id: u64, out: &mut Vec<Bubble>) {
        let console = &mut self.session.console;
        let mut push = |kind, text: String| {
            out.push(Bubble {
                reply_to: id,
                kind: if id == 0 && kind == BubbleKind::Reply {
                    BubbleKind::System
                } else {
                    kind
                },
                text,
            })
        };
        for line in console.lines.drain(..) {
            match line.kind {
                LineKind::Command => {}
                LineKind::Note => push(BubbleKind::Reply, line.text),
                LineKind::Error => push(BubbleKind::Error, line.text),
            }
        }
        if let Some(error) = console.error.take() {
            push(BubbleKind::Error, error);
        }
    }
}
