use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use serde_json::{json, Value};

/// `observe_property` registration ids. mpv echoes the id back with
/// every change, but the handler matches on the property name, so
/// these only have to be distinct from each other.
const OBS_PLAYLIST_POS: u64 = 1;
const OBS_IDLE_ACTIVE: u64 = 2;
const OBS_TIME_POS: u64 = 3;
const OBS_DURATION: u64 = 4;
const OBS_PAUSE: u64 = 5;
const OBS_LEVEL_RMS: u64 = 6;
const OBS_EOF_REACHED: u64 = 7;

/// How long any one blocking command waits for mpv's reply.
const REPLY_TIMEOUT: Duration = Duration::from_secs(10);

/// Current index into mpv's own playlist, or -1 when nothing is
/// loaded; the single source of truth for which track is playing.
pub const PROP_PLAYLIST_POS: &str = "playlist-pos";
/// True once mpv has run out of playlist and gone back to idling,
/// i.e. the queue actually drained. Unambiguous in a way end-of-file
/// never was: `loadfile replace` emits an end-file for the track it
/// evicts, but never leaves mpv idle.
pub const PROP_IDLE_ACTIVE: &str = "idle-active";
/// Where playback is, how long the file is, and whether it's paused.
/// Observed rather than polled: the render path reads these every
/// frame, and `get_property` is a blocking request/reply round trip
/// over the ipc socket each time.
pub const PROP_TIME_POS: &str = "time-pos";
pub const PROP_DURATION: &str = "duration";
pub const PROP_PAUSE: &str = "pause";
/// True while mpv is sitting at the end of a file it has decided not
/// to close, which only happens under `keep-open`. That is the hook
/// the between-track gap hangs off: mpv stops *before* loading the
/// next file, so the pause lands in the join rather than a few
/// milliseconds into the next track.
///
/// Measured, because two things about it are not obvious.
/// `keep-open=always` makes mpv pause itself and hold (`pause` goes
/// true), and **`pause` stays true after `playlist-next`**, so
/// whoever advances has to unpause as well. And under
/// `keep-open=always` mpv **never goes idle at all**, so
/// `PROP_IDLE_ACTIVE` stops firing and the end-of-playlist and
/// loop-all path never runs; `set_keep_open` is therefore toggled per
/// track rather than pinned on.
pub const PROP_EOF_REACHED: &str = "eof-reached";
/// RMS output level in dBFS, from the labeled `astats` tap
/// `eq::build_af_filter` puts at the end of the af chain; drives the
/// now-playing level meter. Spelled out rather than built with
/// `format!` so it can be matched in `Playback::apply` for free; a
/// test ties it back to `eq::LEVEL_METADATA_LABEL`.
///
/// The label has to be mpv's *own* filter label (`af=@wimlevel:lavfi=
/// [...]`), not a label inside the lavfi graph: `af-metadata/<label>`
/// resolves nothing at all for the latter, which is what left this
/// meter dead while still paying a round trip per frame for it.
pub const PROP_LEVEL_RMS: &str = "af-metadata/wimlevel/lavfi.astats.Overall.RMS_level";

/// How long the last pushed level reading is believed. mpv pushes
/// ~20/s while audio flows and simply stops pushing when it doesn't
/// (pause), so without a horizon a paused meter would sit frozen at
/// whatever the last frame measured. Worst-case staleness is therefore
/// this value; af being cleared or playback stopping both push an
/// explicit "no value" that clears it immediately.
const LEVEL_FRESH_FOR: Duration = Duration::from_millis(300);

/// Property-change and other async events pushed by mpv over the IPC socket.
#[derive(Debug, Clone)]
pub enum Event {
    PropertyChange {
        name: String,
        value: Value,
    },
    #[allow(dead_code)]
    Other(Value),
}

/// Last known playback position, folded from observed property
/// changes rather than asked for.
///
/// This used to be three `get_property` calls per frame
/// (`playback_info`), so every frame paid three blocking round trips
/// for values mpv will happily push. The scrubber, the mini bar, the
/// full player and the scrobble threshold all read it, which is why it
/// was already hoisted to one call per frame; now it costs none.
#[derive(Debug, Clone, Copy, Default)]
pub struct Playback {
    pub elapsed_secs: Option<f64>,
    pub duration_secs: Option<f64>,
    pub paused: bool,
    /// mpv is holding at the end of a file under `keep-open`; see
    /// `PROP_EOF_REACHED`.
    pub eof_reached: bool,
    /// Last pushed `astats` RMS reading and when it landed; private
    /// because the pair is only meaningful together, see `level`.
    level_db: Option<f64>,
    level_at: Option<Instant>,
}

impl Playback {
    /// Folds one observed change in, returning whether it was one of
    /// ours. mpv sends `null` for a property that currently has no
    /// value (no file loaded), which is `None` rather than stale.
    pub fn apply(&mut self, name: &str, value: &Value) -> bool {
        match name {
            PROP_TIME_POS => self.elapsed_secs = value.as_f64(),
            PROP_DURATION => self.duration_secs = value.as_f64(),
            // an absent `pause` means nothing is loaded, which reads
            // as not-paused everywhere downstream.
            PROP_PAUSE => self.paused = value.as_bool().unwrap_or(false),
            // absent reads as "not at a held end", same as `pause`.
            PROP_EOF_REACHED => self.eof_reached = value.as_bool().unwrap_or(false),
            PROP_LEVEL_RMS => {
                // mpv hands af-metadata values through as the strings
                // the filter wrote; `as_f64` alone reads every one of
                // them as absent. a number is accepted too in case a
                // later mpv types them.
                self.level_db = value
                    .as_f64()
                    .or_else(|| value.as_str().and_then(|s| s.parse().ok()));
                self.level_at = self.level_db.map(|_| Instant::now());
            }
            _ => return false,
        }
        true
    }

    /// Output level 0..1 for the level meter, or `None` when no fresh
    /// reading is in hand (paused, stopped, or an af chain without the
    /// tap); callers fall back to their own idle animation. Costs no
    /// IPC: this is the last value mpv pushed.
    pub fn level(&self) -> Option<f32> {
        self.level_as_of(Instant::now())
    }

    fn level_as_of(&self, now: Instant) -> Option<f32> {
        let db = self.level_db?;
        if now.duration_since(self.level_at?) > LEVEL_FRESH_FOR {
            return None;
        }
        // RMS level is dBFS, roughly -60 (near silent) to 0 (full
        // scale); map that range rather than guessing a narrower one
        // that'd clip real audio's dynamic range. digital silence
        // reads -inf, which clamps to 0.
        let scaled = (db as f32 + 60.0) / 60.0;
        Some(if scaled.is_nan() {
            0.0
        } else {
            scaled.clamp(0.0, 1.0)
        })
    }
}

struct Pending {
    reply_tx: Sender<Result<Value, String>>,
}

/// A running mpv instance controlled over its JSON IPC socket, rather than
/// linking libmpv directly; keeps the build free of C FFI/pkg-config and
/// matches how mpv is normally driven by external controllers.
pub struct Mpv {
    child: Child,
    sock_path: PathBuf,
    sock: Mutex<UnixStream>,
    next_id: AtomicU64,
    pending: Arc<Mutex<HashMap<u64, Pending>>>,
    events: Receiver<Event>,
}

/// Asks the kernel to SIGTERM mpv if this process dies without
/// getting to clean up. `Drop` already sends `quit` on an orderly
/// exit, but a SIGKILL or an abort skips destructors entirely and left
/// mpv running headless forever, holding the audio device and still
/// answering on a socket nothing owns.
///
/// Linux only (`prctl` is not portable, and the BSD equivalent differs
/// enough to be its own job). Deliberately set from `pre_exec` in the
/// child rather than anywhere in the parent: the flag belongs to the
/// child process.
///
/// One real footgun worth stating: `PR_SET_PDEATHSIG` fires when the
/// parent *thread* exits, not the parent process. `Mpv::spawn` is
/// called from `main` on the main thread, which lives as long as the
/// program, so this is correct here; moving the spawn onto a worker
/// thread would kill mpv the moment that worker finished.
#[cfg(target_os = "linux")]
fn reap_with_parent(cmd: &mut Command) {
    use std::os::unix::process::CommandExt;
    // safety: `pre_exec` runs between fork and exec, so the closure
    // must be async-signal-safe. one `prctl` syscall and nothing else
    // qualifies; it allocates nothing and takes no locks.
    unsafe {
        cmd.pre_exec(|| {
            libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM);
            Ok(())
        });
    }
}

#[cfg(not(target_os = "linux"))]
fn reap_with_parent(_cmd: &mut Command) {}

/// Distinguishes two `Mpv`s spawned by the same process. The app only
/// ever spawns one, but naming the socket by pid alone meant a second
/// spawn deleted the first one's socket and both mpvs then answered on
/// the same path, which is two hours of a test suite hanging on replies
/// from the wrong process.
static SPAWN_SEQ: AtomicU64 = AtomicU64::new(0);

impl Mpv {
    pub fn spawn() -> Result<Self> {
        let sock_path = std::env::temp_dir().join(format!(
            "wim-mpv-{}-{}.sock",
            std::process::id(),
            SPAWN_SEQ.fetch_add(1, Ordering::SeqCst)
        ));
        let _ = std::fs::remove_file(&sock_path);

        let mut cmd = Command::new("mpv");
        cmd.arg("--idle=yes")
            .arg("--no-terminal")
            // mpv is a backend here, not the user's player: a
            // ~/.config/mpv/mpv.conf would silently apply its own af
            // chain, volume or replaygain on top of everything this
            // app sets, and the result is indistinguishable from a bug
            // in wim. a "why does this sound wrong" investigation
            // already burned hours ruling that out, so pin it.
            .arg("--no-config")
            // this is the actual fix for mpv opening its own native
            // window: `vid` (which track to decode, toggled per-item
            // in set_vid_for_item) and `vo` (where a decoded frame
            // gets displayed) are independent. Never setting `vo`
            // left mpv on its own default (a real gpu/x11/wayland
            // window) regardless of vid, so video played with a
            // window popping up right alongside this one even though
            // nothing ever asked for that; closing that window quits
            // mpv's whole process, breaking the ipc socket this app
            // depends on. `--vo=null` discards the frame instead of
            // displaying it anywhere; `screenshot-raw` (the embedded
            // video pane's own frame source) still works, since a
            // screenshot is taken from the decoded/filtered frame,
            // not from whatever vo would've shown it in. `vo` is
            // settable at runtime over ipc, which is how fullscreen
            // video flips to a terminal output and back; see
            // `set_video_output` and docs/video.md.
            .arg("--vo=null")
            .arg(format!("--input-ipc-server={}", sock_path.display()))
            // stdin stays closed: `--no-terminal` already implies
            // `--no-input-terminal`, and mpv must never compete with the
            // tui for keystrokes. stdout is this terminal, because mpv's
            // `tct`/`kitty` outputs draw a fullscreen video by writing
            // escapes to it; with `--vo=null` it writes nothing at all
            // (measured: zero bytes across a load, a seek and a volume
            // change), so ordinary music playback is unaffected. stderr
            // stays closed since `--no-terminal` implies
            // `--really-quiet` but a loader warning would still land on
            // top of the ui.
            .stdin(Stdio::null())
            .stdout(Stdio::inherit())
            .stderr(Stdio::null());
        reap_with_parent(&mut cmd);
        let child = cmd
            .spawn()
            .context("spawning mpv; is it installed and on PATH?")?;

        // mpv creates the socket asynchronously after startup, usually
        // 30-80ms in. a flat 20ms sleep per attempt spent most of that
        // asleep past the socket actually being ready, so back off
        // instead: 1ms doubling to 20ms, same ~1s total budget.
        const CONNECT_BUDGET: Duration = Duration::from_secs(1);
        const MAX_BACKOFF: Duration = Duration::from_millis(20);
        let sock = {
            let mut waited = Duration::ZERO;
            let mut backoff = Duration::from_millis(1);
            loop {
                match UnixStream::connect(&sock_path) {
                    Ok(s) => break s,
                    Err(_) if waited < CONNECT_BUDGET => {
                        thread::sleep(backoff);
                        waited += backoff;
                        backoff = (backoff * 2).min(MAX_BACKOFF);
                    }
                    Err(e) => return Err(e).context("connecting to mpv ipc socket"),
                }
            }
        };

        let pending: Arc<Mutex<HashMap<u64, Pending>>> = Arc::new(Mutex::new(HashMap::new()));
        let (event_tx, event_rx) = mpsc::channel();

        let reader_sock = sock.try_clone().context("cloning mpv ipc socket")?;
        let reader_pending = pending.clone();
        thread::spawn(move || {
            let mut lines = BufReader::new(reader_sock).lines();
            while let Some(Ok(line)) = lines.next() {
                if line.trim().is_empty() {
                    continue;
                }
                let Ok(v) = serde_json::from_str::<Value>(&line) else {
                    continue;
                };
                if let Some(id) = v.get("request_id").and_then(Value::as_u64) {
                    if let Some(p) = reader_pending.lock().unwrap().remove(&id) {
                        let result = match v.get("error").and_then(Value::as_str) {
                            Some("success") | None => {
                                Ok(v.get("data").cloned().unwrap_or(Value::Null))
                            }
                            Some(err) => Err(err.to_string()),
                        };
                        let _ = p.reply_tx.send(result);
                    }
                    continue;
                }
                match v.get("event").and_then(Value::as_str) {
                    Some("property-change") => {
                        // mpv omits `data` entirely (rather than
                        // sending null) when the property has become
                        // unavailable: af cleared, playback stopped.
                        // requiring it dropped exactly the event that
                        // says "this reading is gone", leaving the
                        // last one standing forever.
                        if let Some(name) = v.get("name").and_then(Value::as_str) {
                            let _ = event_tx.send(Event::PropertyChange {
                                name: name.to_string(),
                                value: v.get("data").cloned().unwrap_or(Value::Null),
                            });
                        }
                    }
                    Some(_) => {
                        let _ = event_tx.send(Event::Other(v));
                    }
                    None => {}
                }
            }
        });

        Ok(Mpv {
            child,
            sock_path,
            sock: Mutex::new(sock),
            next_id: AtomicU64::new(1),
            pending,
            events: event_rx,
        })
    }

    /// A second, independent ipc connection that pulls video frames on
    /// its own thread; see `FramePump`. Separate connection on purpose:
    /// a frame reply is megabytes of json, and sharing the control
    /// connection would put that parse in front of the `time-pos`
    /// events the render path reads.
    pub fn frame_pump(&self) -> Result<FramePump> {
        FramePump::spawn(&self.sock_path)
    }

    pub fn events(&self) -> &Receiver<Event> {
        &self.events
    }

    /// Sends a raw mpv IPC command and blocks for its reply.
    pub fn command(&self, args: &[Value]) -> Result<Value> {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let (tx, rx) = mpsc::channel();
        self.pending
            .lock()
            .unwrap()
            .insert(id, Pending { reply_tx: tx });

        let payload = json!({ "command": args, "request_id": id });
        let mut line = serde_json::to_vec(&payload)?;
        line.push(b'\n');
        self.sock
            .lock()
            .unwrap()
            .write_all(&line)
            .context("writing to mpv ipc socket")?;

        match rx.recv_timeout(REPLY_TIMEOUT) {
            Ok(Ok(v)) => Ok(v),
            Ok(Err(e)) => bail!("mpv command error: {e}"),
            Err(_) => {
                self.pending.lock().unwrap().remove(&id);
                bail!("timed out waiting for mpv reply")
            }
        }
    }

    /// Sends a command and does not wait for its reply. For values the
    /// next frame supersedes anyway (the volume ramps), where the
    /// ~2ms round trip is the entire cost. mpv applies one
    /// connection's commands in the order they arrive, so a ramp's
    /// last write still wins; the reply comes back to a request id
    /// with no pending entry and is dropped by the reader.
    pub fn command_nowait(&self, args: &[Value]) -> Result<()> {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let payload = json!({ "command": args, "request_id": id });
        let mut line = serde_json::to_vec(&payload)?;
        line.push(b'\n');
        self.sock
            .lock()
            .unwrap()
            .write_all(&line)
            .context("writing to mpv ipc socket")?;
        Ok(())
    }

    /// Writes every command, then collects every reply: the whole
    /// batch costs one round trip's latency instead of one each, since
    /// mpv reads a connection's commands in order and answers them in
    /// order. One result per input, in the same order; a failure in
    /// one says nothing about the others.
    pub fn command_batch(&self, cmds: &[Vec<Value>]) -> Vec<Result<Value>> {
        let mut ids = Vec::with_capacity(cmds.len());
        let mut rxs = Vec::with_capacity(cmds.len());
        let mut wire = Vec::new();
        {
            let mut pending = self.pending.lock().unwrap();
            for args in cmds {
                let id = self.next_id.fetch_add(1, Ordering::SeqCst);
                let (tx, rx) = mpsc::channel();
                pending.insert(id, Pending { reply_tx: tx });
                ids.push(id);
                rxs.push(rx);
                let payload = json!({ "command": args, "request_id": id });
                let mut line = serde_json::to_vec(&payload).unwrap_or_default();
                line.push(b'\n');
                wire.extend_from_slice(&line);
            }
        }

        if let Err(e) = self.sock.lock().unwrap().write_all(&wire) {
            let mut pending = self.pending.lock().unwrap();
            for id in &ids {
                pending.remove(id);
            }
            return (0..cmds.len())
                .map(|_| Err(anyhow::anyhow!("writing to mpv ipc socket: {e}")))
                .collect();
        }

        ids.into_iter()
            .zip(rxs)
            .map(|(id, rx)| match rx.recv_timeout(REPLY_TIMEOUT) {
                Ok(Ok(v)) => Ok(v),
                Ok(Err(e)) => Err(anyhow::anyhow!("mpv command error: {e}")),
                Err(_) => {
                    self.pending.lock().unwrap().remove(&id);
                    Err(anyhow::anyhow!("timed out waiting for mpv reply"))
                }
            })
            .collect()
    }

    pub fn get_property(&self, name: &str) -> Result<Value> {
        self.command(&[json!("get_property"), json!(name)])
    }

    pub fn set_property(&self, name: &str, value: Value) -> Result<()> {
        self.command(&[json!("set_property"), json!(name), value])?;
        Ok(())
    }

    /// Points mpv's video output at this terminal, or back at `null`.
    ///
    /// Safe to call mid-playback: the switch keeps the playlist, the
    /// position and the pause state, so there is no relaunch and
    /// nothing to seek back to. The vo owns the screen while it draws
    /// (it enters and leaves the alternate screen itself); see
    /// `term::TerminalGuard::reclaim` and docs/video.md.
    pub fn set_video_output(&self, vo: &str) -> Result<()> {
        self.set_property("vo", json!(vo))
    }

    /// How big a drawing vo gets to be, in terminal cells. Has to be
    /// pushed from here and cannot be left at mpv's 0 ("ask the tty"):
    /// under `--no-terminal` mpv never queries the tty and silently
    /// falls back to 80x25, so a video on a larger terminal drew into
    /// the top-left 80x25 of it.
    ///
    /// Only takes effect when the vo next initializes, so a resize has
    /// to bounce `vo` through `null`; both outputs' options are set
    /// together, since neither matters until its own vo is selected.
    pub fn set_video_geometry(&self, cols: u16, rows: u16) -> Result<()> {
        let cmds: Vec<Vec<Value>> = [
            ("vo-tct-width", json!(cols)),
            ("vo-tct-height", json!(rows)),
            ("vo-kitty-cols", json!(cols)),
            ("vo-kitty-rows", json!(rows)),
            // one write per frame instead of per row: a half-block vo
            // painting a row at a time tears visibly at this size.
            ("vo-tct-buffering", json!("frame")),
        ]
        .into_iter()
        .map(|(name, value)| vec![json!("set_property"), json!(name), value])
        .collect();
        for result in self.command_batch(&cmds) {
            result?;
        }
        Ok(())
    }

    /// Asks mpv to push every property the render path reads instead of
    /// us polling for them, which is what lets the current track and
    /// the level meter be resolved from mpv's own state. Registering
    /// fires one change event per property immediately with its current
    /// value, so this has to run *after* any startup playback is
    /// already loaded or the first events describe an mpv that's still
    /// idle. Pipelined: five registrations, one round trip.
    pub fn observe_playback(&self) -> Result<()> {
        let cmds: Vec<Vec<Value>> = [
            (OBS_PLAYLIST_POS, PROP_PLAYLIST_POS),
            (OBS_IDLE_ACTIVE, PROP_IDLE_ACTIVE),
            (OBS_TIME_POS, PROP_TIME_POS),
            (OBS_DURATION, PROP_DURATION),
            (OBS_PAUSE, PROP_PAUSE),
            (OBS_LEVEL_RMS, PROP_LEVEL_RMS),
            (OBS_EOF_REACHED, PROP_EOF_REACHED),
        ]
        .into_iter()
        .map(|(id, name)| vec![json!("observe_property"), json!(id), json!(name)])
        .collect();
        for result in self.command_batch(&cmds) {
            result?;
        }
        Ok(())
    }

    /// Replaces mpv's entire playlist and plays `path_or_url`
    /// immediately; for "play this now" (enter), not queuing.
    pub fn load_file(&self, path_or_url: &str) -> Result<()> {
        self.command(&[json!("loadfile"), json!(path_or_url), json!("replace")])?;
        Ok(())
    }

    /// Appends to mpv's own playlist (playing immediately if nothing is
    /// currently loaded, otherwise queued). mpv auto-advances through
    /// its playlist natively on end-of-file, with none of the IPC
    /// round-trip gap that reactively reloading on our end introduces;
    /// this is what actually makes queue playback gapless.
    pub fn playlist_append(&self, path_or_url: &str) -> Result<()> {
        self.command(&[json!("loadfile"), json!(path_or_url), json!("append-play")])?;
        Ok(())
    }

    /// Clears mpv's playlist except the currently-playing file (mpv's
    /// own semantics for `playlist-clear`), so the upcoming queue can be
    /// rebuilt (e.g. after a shuffle) without interrupting playback.
    /// Drops one entry from mpv's own playlist. Removing the entry
    /// that's playing makes mpv move on to the next one, which is the
    /// sensible reading of "remove this".
    pub fn playlist_remove(&self, index: usize) -> Result<()> {
        self.command(&[json!("playlist-remove"), json!(index)])?;
        Ok(())
    }

    /// Moves the entry at `from` so it ends up at index `to`.
    ///
    /// mpv's own `playlist-move` takes the *target entry* to displace,
    /// not the destination index, and the two differ when moving
    /// downwards: moving 2 to sit at 3 means displacing entry 4. That
    /// off-by-one is mpv's documented behaviour, converted here so
    /// callers can just say where they want it to land. The shuffled
    /// re-insert in `main.rs` only ever moves upwards, where the two
    /// readings coincide, so it is unaffected by the conversion.
    pub fn playlist_move(&self, from: usize, to: usize) -> Result<()> {
        let displaced = if to > from { to + 1 } else { to };
        self.command(&[json!("playlist-move"), json!(from), json!(displaced)])?;
        Ok(())
    }

    pub fn playlist_clear(&self) -> Result<()> {
        self.command(&[json!("playlist-clear")])?;
        Ok(())
    }

    /// `mode`: "no" | "track" | "album". mpv reads replaygain tags
    /// embedded in the file itself (via its own demuxer/decoder) and
    /// applies the matching gain automatically; we don't need to
    /// extract gain values ourselves for local files.
    pub fn set_replaygain(&self, mode: &str) -> Result<()> {
        self.set_property("replaygain", json!(mode))
    }

    /// One round trip: mpv flips the flag itself. Reading `pause` and
    /// writing the negation cost two, and acted on a value that could
    /// already have changed in between.
    pub fn toggle_pause(&self) -> Result<()> {
        self.command(&[json!("cycle"), json!("pause")])?;
        Ok(())
    }

    /// Paused or not, explicitly. `toggle_pause` is the right call
    /// for a user pressing play/pause; this is for the paths that know
    /// which state they want, such as releasing a between-track gap
    /// (mpv stays paused across `playlist-next` once it has held).
    pub fn set_pause(&self, paused: bool) -> Result<()> {
        self.set_property(PROP_PAUSE, json!(paused))
    }

    /// `"weak"`: a no-op (not an error) if there's nothing further
    /// along mpv's own playlist, since it already holds every queued
    /// track (see `playlist_append`) and knows better than we do
    /// whether there's anywhere to skip to.
    pub fn playlist_next(&self) -> Result<()> {
        self.command(&[json!("playlist-next"), json!("weak")])
            .map(|_| ())
    }

    /// Whether mpv should hold at the end of every file instead of
    /// rolling straight into the next one.
    ///
    /// `always`, not `yes`: `yes` holds only on the *last* entry,
    /// which is the opposite of what a between-track gap needs. Only
    /// ever turned on while a gap is actually wanted and there is a
    /// next track, because holding also means mpv never goes idle,
    /// and `PROP_IDLE_ACTIVE` going quiet would take the
    /// end-of-playlist and loop-all handling with it.
    pub fn set_keep_open(&self, hold: bool) -> Result<()> {
        self.set_property("keep-open", json!(if hold { "always" } else { "no" }))
    }

    pub fn set_volume(&self, vol: f64) -> Result<()> {
        self.set_property("volume", json!(vol))
    }

    /// `set_volume` without waiting for mpv's reply, for the crossfade
    /// and shellsong ramps that write it once per frame; see
    /// `command_nowait` for why that is safe for a ramp.
    pub fn set_volume_nowait(&self, vol: f64) -> Result<()> {
        self.command_nowait(&[json!("set_property"), json!("volume"), json!(vol)])
    }

    pub fn set_speed(&self, speed: f64) -> Result<()> {
        self.set_property("speed", json!(speed))
    }

    /// `mode`: "no" | "inf" | a repeat count.
    pub fn set_loop_file(&self, mode: &str) -> Result<()> {
        self.set_property("loop-file", json!(mode))
    }

    /// Sets the `af` (audio filter graph) property directly, e.g. for
    /// the eq filter chain built by `crate::eq::build_af_filter`.
    pub fn set_af(&self, filter: &str) -> Result<()> {
        self.set_property("af", json!(filter))
    }

    /// Applies every `sub-*` style property from config/runtime state
    /// at once; best-effort per property (one unsupported/malformed
    /// value on an older mpv shouldn't block the rest from applying).
    /// Pipelined, because the subtitle menu calls this on every single
    /// keypress and seven sequential round trips was the whole cost.
    pub fn set_subtitle_style(&self, cfg: &crate::config::SubtitleConfig) {
        let cmds = vec![
            vec![
                json!("set_property"),
                json!("sub-delay"),
                json!(cfg.delay_secs),
            ],
            vec![
                json!("set_property"),
                json!("sub-font-size"),
                json!(cfg.font_size),
            ],
            vec![json!("set_property"), json!("sub-pos"), json!(cfg.position)],
            vec![
                json!("set_property"),
                json!("sub-border-size"),
                json!(cfg.outline_size),
            ],
            vec![json!("set_property"), json!("sub-blur"), json!(cfg.blur)],
            vec![
                json!("set_property"),
                json!("sub-color"),
                json!(gray_color(cfg.color_level, 1.0)),
            ],
            // the background box itself always renders near-black
            // regardless of the text's own level; a box that matched a
            // bright text color would defeat its own purpose (readability
            // contrast), and there's no separate background-color config
            // field to let it drift from that.
            vec![
                json!("set_property"),
                json!("sub-back-color"),
                json!(gray_color(0.0, cfg.background_opacity as f32)),
            ],
        ];
        let _ = self.command_batch(&cmds);
    }

    /// Loads an external subtitle file onto the currently playing
    /// video (e.g. one just downloaded from opensubtitles) and
    /// switches to it immediately ("select" flag), rather than just
    /// adding it to the track list unselected.
    pub fn sub_add(&self, path: &str) -> Result<()> {
        self.command(&[json!("sub-add"), json!(path), json!("select")])?;
        Ok(())
    }

    pub fn seek(&self, seconds: f64, relative: bool) -> Result<()> {
        let flag = if relative { "relative" } else { "absolute" };
        self.command(&[json!("seek"), json!(seconds), json!(flag)])?;
        Ok(())
    }
}

/// `bgr0` (4 bytes/pixel, blue/green/red/unused) raw frame as returned
/// by mpv's `screenshot-raw` command; `stride` is the byte width of
/// one row and may exceed `width * 4` (row padding), so it has to be
/// used to find each row's start rather than assuming a tight pack.
pub struct RawFrame {
    pub width: u32,
    pub height: u32,
    pub stride: u32,
    pub data: Vec<u8>,
}

impl RawFrame {
    /// `bgr0` -> `image::RgbImage`, dropping the padding byte and
    /// swapping channel order; `None` if the dimensions mpv reported
    /// don't actually fit the data it sent (never expected, but a
    /// malformed frame should just get skipped, not panic the UI).
    ///
    /// One bounds check per row rather than one per pixel: at 1080p
    /// that is 1080 instead of 2 million, and the output buffer is
    /// sized once instead of growing under three `push`es per pixel.
    pub fn to_image(&self) -> Option<image::RgbImage> {
        let (w, h, stride) = (
            self.width as usize,
            self.height as usize,
            self.stride as usize,
        );
        if w == 0 || h == 0 {
            return None;
        }
        let mut rgb = vec![0u8; w.checked_mul(h)?.checked_mul(3)?];
        for (row, out_row) in rgb.chunks_exact_mut(w * 3).enumerate() {
            let start = row.checked_mul(stride)?;
            let src = self.data.get(start..start.checked_add(w * 4)?)?;
            // fixed-size chunks: the compiler knows each `px`/`out` is
            // in bounds, so the inner loop carries no checks at all.
            let (pixels, _) = src.as_chunks::<4>();
            let (outs, _) = out_row.as_chunks_mut::<3>();
            for (px, out) in pixels.iter().zip(outs.iter_mut()) {
                out[0] = px[2]; // r
                out[1] = px[1]; // g
                out[2] = px[0]; // b
            }
        }
        image::RgbImage::from_raw(self.width, self.height, rgb)
    }
}

/// What the frame pump should be doing. `interval` decouples the
/// capture rate from the display refresh rate; `max_edge` box-scales
/// each frame down to at most that many pixels on its longer side, so
/// a per-cell renderer's sampling cost tracks the cell grid rather than
/// the source resolution. Both are the pump's cost knobs, deliberately
/// parameters rather than constants.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Capture {
    pub on: bool,
    pub interval: Duration,
    pub max_edge: Option<u32>,
}

impl Capture {
    pub fn off() -> Self {
        Capture {
            on: false,
            interval: Duration::from_millis(150),
            max_edge: None,
        }
    }
}

/// The most recent decoded video frame. `image` is shared, so handing
/// it to a renderer is a refcount bump and never a copy.
#[derive(Clone)]
pub struct Frame {
    pub image: Arc<image::DynamicImage>,
    /// Counts published frames. A per-frame renderer can hold the last
    /// value it drew and skip re-rasterizing an unchanged frame; not
    /// read yet, which is why it is allowed to look dead.
    #[allow(dead_code)]
    pub seq: u64,
}

/// Whether the pump has a working frame source. `Unsupported` is not
/// a transient failure: mpv's own manual says `screenshot-raw` "can be
/// used only through the client API", and over json ipc mpv answers it
/// with a byte array it has no json representation for, emitting a
/// reply that does not parse. See docs/architecture.md.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PumpState {
    Idle,
    Capturing,
    Unsupported,
}

/// Pulls video frames from mpv on its own thread and its own ipc
/// connection, keeping exactly the newest decoded frame. The render
/// path only ever reads `latest`, so neither the ipc round trip nor the
/// base64/json decode nor the bgr0 conversion is ever on it, and a
/// decode slower than `interval` drops frames instead of queueing them.
pub struct FramePump {
    latest: Arc<Mutex<Option<Frame>>>,
    state: Arc<Mutex<PumpState>>,
    want: Mutex<Capture>,
    ctl: Sender<Capture>,
    stop: Arc<AtomicBool>,
}

impl FramePump {
    fn spawn(sock_path: &Path) -> Result<Self> {
        let sock = UnixStream::connect(sock_path).context("second mpv ipc connection")?;
        let latest: Arc<Mutex<Option<Frame>>> = Arc::new(Mutex::new(None));
        let state = Arc::new(Mutex::new(PumpState::Idle));
        let stop = Arc::new(AtomicBool::new(false));
        let (ctl, ctl_rx) = mpsc::channel();

        let worker = PumpWorker {
            latest: latest.clone(),
            state: state.clone(),
            stop: stop.clone(),
        };
        thread::spawn(move || worker.run(sock, ctl_rx));

        Ok(FramePump {
            latest,
            state,
            want: Mutex::new(Capture::off()),
            ctl,
            stop,
        })
    }

    /// The newest decoded frame, or `None` if none has landed since
    /// capture was last turned on. Never blocks on mpv or on a decode:
    /// a short lock and an `Arc` clone, nothing else. This is the whole
    /// surface a per-frame renderer needs.
    pub fn latest(&self) -> Option<Frame> {
        self.latest.lock().ok()?.clone()
    }

    /// Turns capture on or off and sets its cost knobs. Cheap and
    /// idempotent, so it is fine to call every frame: a request equal
    /// to the standing one sends nothing. Turning it off clears the
    /// held frame synchronously, so `latest` cannot hand back a frame
    /// from the file that stopped playing.
    pub fn set_capture(&self, want: Capture) {
        let Ok(mut standing) = self.want.lock() else {
            return;
        };
        if *standing == want {
            return;
        }
        *standing = want;
        if !want.on {
            if let Ok(mut held) = self.latest.lock() {
                *held = None;
            }
        }
        let _ = self.ctl.send(want);
    }

    pub fn state(&self) -> PumpState {
        self.state.lock().map(|s| *s).unwrap_or(PumpState::Idle)
    }
}

impl Drop for FramePump {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
    }
}

struct PumpWorker {
    latest: Arc<Mutex<Option<Frame>>>,
    state: Arc<Mutex<PumpState>>,
    stop: Arc<AtomicBool>,
}

/// How long the idle pump blocks on its control channel before
/// re-checking the stop flag.
const PUMP_IDLE_WAIT: Duration = Duration::from_millis(250);

impl PumpWorker {
    fn set_state(&self, state: PumpState) {
        if let Ok(mut s) = self.state.lock() {
            *s = state;
        }
    }

    fn run(self, sock: UnixStream, ctl: Receiver<Capture>) {
        let Ok(write_sock) = sock.try_clone() else {
            return;
        };
        let mut writer = write_sock;
        let mut reader = BufReader::new(sock);
        let mut next_id = 0u64;
        // this connection carries nothing but our own replies, so the
        // reply to a request is simply the next line; that is what
        // lets a reply mpv could not serialize be recognised at all.
        if request(
            &mut writer,
            &mut reader,
            &mut next_id,
            &[json!("disable_event"), json!("all")],
        )
        .is_none()
        {
            return;
        }

        let mut want = Capture::off();
        let mut seq = 0u64;
        loop {
            if self.stop.load(Ordering::SeqCst) {
                return;
            }
            if !want.on {
                match ctl.recv_timeout(PUMP_IDLE_WAIT) {
                    Ok(next) => want = next,
                    Err(RecvTimeoutError::Timeout) => {}
                    Err(RecvTimeoutError::Disconnected) => return,
                }
                continue;
            }

            let started = Instant::now();
            match self.capture(&mut writer, &mut reader, &mut next_id, want.max_edge) {
                Capture_::Published(image) => {
                    seq += 1;
                    if let Ok(mut held) = self.latest.lock() {
                        *held = Some(Frame { image, seq });
                    }
                    self.set_state(PumpState::Capturing);
                }
                // no video track, mpv busy: try again next tick.
                Capture_::Skip => self.set_state(PumpState::Capturing),
                Capture_::Unsupported => {
                    self.set_state(PumpState::Unsupported);
                    want.on = false;
                }
                Capture_::Dead => return,
            }

            let rest = want.interval.saturating_sub(started.elapsed());
            match ctl.recv_timeout(rest) {
                Ok(next) => want = next,
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => return,
            }
            // a backlog of control messages only has one meaning: the
            // last one.
            while let Ok(next) = ctl.try_recv() {
                want = next;
            }
        }
    }

    fn capture(
        &self,
        writer: &mut UnixStream,
        reader: &mut BufReader<UnixStream>,
        next_id: &mut u64,
        max_edge: Option<u32>,
    ) -> Capture_ {
        match request(writer, reader, next_id, &[json!("screenshot-raw")]) {
            None => Capture_::Dead,
            Some(Reply::Unparseable) => Capture_::Unsupported,
            Some(Reply::Error) => Capture_::Skip,
            Some(Reply::Data(v)) => {
                let Some(frame) = parse_raw_frame(&v) else {
                    return Capture_::Skip;
                };
                let Some(rgb) = frame.to_image() else {
                    return Capture_::Skip;
                };
                let mut img = image::DynamicImage::ImageRgb8(rgb);
                if let Some(edge) = max_edge {
                    let (w, h) = (img.width(), img.height());
                    if w.max(h) > edge {
                        img = img.resize(edge, edge, image::imageops::FilterType::Triangle);
                    }
                }
                Capture_::Published(Arc::new(img))
            }
        }
    }
}

/// Outcome of one capture attempt; `Dead` means the ipc connection is
/// gone and the pump should stop.
enum Capture_ {
    Published(Arc<image::DynamicImage>),
    Skip,
    Unsupported,
    Dead,
}

enum Reply {
    Data(Value),
    /// mpv refused the command: no video track yet, or it is busy.
    /// Which error it was doesn't change what the pump does (skip this
    /// tick), so it isn't carried.
    Error,
    /// mpv wrote a line that is not json. For `screenshot-raw` that is
    /// mpv emitting `"data":` with nothing after it, because the value
    /// is a byte array and json ipc has no encoding for one.
    Unparseable,
}

/// One request on the pump's own connection, where the next line is by
/// construction the reply. `None` means the connection is unusable.
fn request(
    writer: &mut UnixStream,
    reader: &mut BufReader<UnixStream>,
    next_id: &mut u64,
    args: &[Value],
) -> Option<Reply> {
    *next_id += 1;
    let payload = json!({ "command": args, "request_id": *next_id });
    let mut line = serde_json::to_vec(&payload).ok()?;
    line.push(b'\n');
    writer.write_all(&line).ok()?;

    let mut buf = String::new();
    if reader.read_line(&mut buf).ok()? == 0 {
        return None;
    }
    let Ok(v) = serde_json::from_str::<Value>(buf.trim()) else {
        return Some(Reply::Unparseable);
    };
    match v.get("error").and_then(Value::as_str) {
        Some("success") | None => Some(Reply::Data(v.get("data").cloned().unwrap_or(Value::Null))),
        Some(_) => Some(Reply::Error),
    }
}

/// `screenshot-raw`'s reply map -> `RawFrame`; `None` if any field mpv
/// is supposed to set is missing or the wrong shape.
fn parse_raw_frame(v: &Value) -> Option<RawFrame> {
    let width = u32::try_from(v.get("w")?.as_u64()?).ok()?;
    let height = u32::try_from(v.get("h")?.as_u64()?).ok()?;
    let stride = u32::try_from(v.get("stride")?.as_u64()?).ok()?;
    let data = base64::Engine::decode(
        &base64::engine::general_purpose::STANDARD,
        v.get("data")?.as_str()?,
    )
    .ok()?;
    Some(RawFrame {
        width,
        height,
        stride,
        data,
    })
}

/// `#AARRGGBB`, mpv's own color option format; `level` (0..1) sets R,
/// G, and B identically so this can never produce a hue, only a shade
/// of gray, per the monochrome rule, no matter what caller passes in.
fn gray_color(level: f32, alpha: f32) -> String {
    let v = (level.clamp(0.0, 1.0) * 255.0).round() as u8;
    let a = (alpha.clamp(0.0, 1.0) * 255.0).round() as u8;
    format!("#{a:02X}{v:02X}{v:02X}{v:02X}")
}

impl Drop for Mpv {
    fn drop(&mut self) {
        let _ = self.command(&[json!("quit")]);
        let _ = self.child.wait();
        // mpv does not unlink it on exit, so every run left one behind.
        let _ = std::fs::remove_file(&self.sock_path);
    }
}

/// Measurements of the paths the render thread walks, not correctness
/// checks; `#[ignore]`d because they spawn a real mpv and take seconds.
/// Run with
/// `cargo test --release -- --ignored --nocapture bench_`.
#[cfg(test)]
mod bench {
    use super::*;
    use std::time::Instant;

    /// A `width x height` bgr0 frame with mpv's own row padding, i.e.
    /// exactly the shape `screenshot-raw` hands back.
    fn synthetic_frame(width: u32, height: u32) -> RawFrame {
        let stride = width * 4 + 64;
        let mut data = vec![0u8; (stride * height) as usize];
        for (i, b) in data.iter_mut().enumerate() {
            *b = (i % 251) as u8;
        }
        RawFrame {
            width,
            height,
            stride,
            data,
        }
    }

    /// The conversion as it was before: one `push` per channel into a
    /// `Vec`, bounds-checked per pixel. Kept here only so the bench
    /// below reports both numbers from one run.
    fn to_image_push_per_pixel(frame: &RawFrame) -> Option<image::RgbImage> {
        let mut rgb = Vec::with_capacity((frame.width * frame.height * 3) as usize);
        for row in 0..frame.height {
            let row_start = (row * frame.stride) as usize;
            for col in 0..frame.width {
                let px = row_start + (col * 4) as usize;
                let pixel = frame.data.get(px..px + 3)?;
                rgb.push(pixel[2]);
                rgb.push(pixel[1]);
                rgb.push(pixel[0]);
            }
        }
        image::RgbImage::from_raw(frame.width, frame.height, rgb)
    }

    #[test]
    #[ignore]
    fn bench_raw_frame_to_image() {
        const N: u32 = 30;
        for (w, h) in [(640u32, 480u32), (1280, 720), (1920, 1080)] {
            let frame = synthetic_frame(w, h);
            let t0 = Instant::now();
            for _ in 0..N {
                assert!(to_image_push_per_pixel(&frame).is_some());
            }
            let before = t0.elapsed() / N;
            let t0 = Instant::now();
            for _ in 0..N {
                assert!(frame.to_image().is_some());
            }
            let after = t0.elapsed() / N;
            // the two must agree pixel for pixel, or this is a
            // benchmark of the wrong thing.
            assert_eq!(
                to_image_push_per_pixel(&frame).unwrap().into_raw(),
                frame.to_image().unwrap().into_raw()
            );
            println!(
                "to_image {w}x{h}: push-per-pixel {:.3}ms, row-wise {:.3}ms",
                before.as_secs_f64() * 1e3,
                after.as_secs_f64() * 1e3
            );
        }
    }

    /// What the level meter used to cost the render path (one blocking
    /// request/reply per frame) against what it costs now (a read of
    /// the folded observed value).
    #[test]
    #[ignore]
    fn bench_level_read_blocking_vs_observed() {
        let mpv = Mpv::spawn().expect("mpv on PATH");
        // these run on a desktop that may have a real wim playing;
        // measure the filter chain, don't put a tone on the speakers.
        let _ = mpv.set_property("ao", json!("null"));
        let _ = mpv.set_volume(0.0);
        mpv.set_af(&crate::eq::build_af_filter(&[0.0; 10]))
            .expect("set af");
        mpv.load_file("av://lavfi:sine=frequency=440:duration=600")
            .expect("load");
        mpv.observe_playback().expect("observe");
        thread::sleep(Duration::from_millis(1500));

        // the mean understates the problem: a frame is only as fast as
        // the slowest round trip it waits on, so report the worst too.
        const N: u32 = 300;
        let mut worst = Duration::ZERO;
        let t0 = Instant::now();
        for _ in 0..N {
            let one = Instant::now();
            let _ = mpv.get_property(PROP_LEVEL_RMS);
            worst = worst.max(one.elapsed());
        }
        let blocking = t0.elapsed() / N;

        let mut playback = Playback::default();
        while let Ok(event) = mpv.events().try_recv() {
            if let Event::PropertyChange { name, value } = event {
                playback.apply(&name, &value);
            }
        }
        const M: u32 = 100_000;
        let t0 = Instant::now();
        for _ in 0..M {
            let _ = playback.level();
        }
        let observed = t0.elapsed() / M;
        println!(
            "level read: blocking get_property {:.0}us mean / {:.0}us worst, \
             Playback::level() {:.4}us (reading {:?})",
            blocking.as_secs_f64() * 1e6,
            worst.as_secs_f64() * 1e6,
            observed.as_secs_f64() * 1e6,
            playback.level()
        );
    }

    /// Seven `sub-*` properties: one round trip each, or one for the
    /// whole pipelined batch.
    #[test]
    #[ignore]
    fn bench_subtitle_style_batch_vs_sequential() {
        let mpv = Mpv::spawn().expect("mpv on PATH");
        let cfg = crate::config::SubtitleConfig::default();
        const N: u32 = 100;

        let t0 = Instant::now();
        for _ in 0..N {
            let _ = mpv.set_property("sub-delay", json!(cfg.delay_secs));
            let _ = mpv.set_property("sub-font-size", json!(cfg.font_size));
            let _ = mpv.set_property("sub-pos", json!(cfg.position));
            let _ = mpv.set_property("sub-border-size", json!(cfg.outline_size));
            let _ = mpv.set_property("sub-blur", json!(cfg.blur));
            let _ = mpv.set_property("sub-color", json!("#FFFFFFFF"));
            let _ = mpv.set_property("sub-back-color", json!("#FF000000"));
        }
        let seq = t0.elapsed() / N;

        let t0 = Instant::now();
        for _ in 0..N {
            mpv.set_subtitle_style(&cfg);
        }
        let batched = t0.elapsed() / N;
        println!(
            "subtitle style: sequential {:.0}us, batched {:.0}us",
            seq.as_secs_f64() * 1e6,
            batched.as_secs_f64() * 1e6
        );
    }

    /// The render path's own cost of asking for a video frame: what
    /// `screenshot_raw()` charged the caller, against what reading the
    /// pump's latest frame charges it.
    #[test]
    #[ignore]
    fn bench_video_frame_on_the_calling_thread() {
        let mpv = Mpv::spawn().expect("mpv on PATH");
        mpv.load_file("av://lavfi:testsrc=size=1920x1080:rate=30")
            .expect("load");
        thread::sleep(Duration::from_millis(1500));

        // what the render path used to do once per VIDEO_FRAME_INTERVAL.
        let t0 = Instant::now();
        let got = mpv.command(&[json!("screenshot-raw")]);
        println!(
            "blocking screenshot-raw on the calling thread: {:.0}ms, ok={}",
            t0.elapsed().as_secs_f64() * 1e3,
            got.is_ok()
        );

        let pump = mpv.frame_pump().expect("second ipc connection");
        pump.set_capture(Capture {
            on: true,
            interval: Duration::from_millis(150),
            max_edge: None,
        });
        thread::sleep(Duration::from_millis(1500));
        const N: u32 = 10_000;
        let t0 = Instant::now();
        for _ in 0..N {
            let _ = pump.latest();
        }
        println!(
            "FramePump::latest(): {:.3}us per call, state={:?}",
            t0.elapsed().as_secs_f64() * 1e6 / f64::from(N),
            pump.state()
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gray_color_keeps_r_g_b_identical_regardless_of_input() {
        assert_eq!(gray_color(1.0, 1.0), "#FFFFFFFF");
        assert_eq!(gray_color(0.0, 0.0), "#00000000");
        assert_eq!(gray_color(0.5, 1.0), "#FF808080");
    }

    #[test]
    fn gray_color_clamps_out_of_range_input() {
        assert_eq!(gray_color(-1.0, 2.0), "#FF000000");
    }

    #[test]
    fn raw_frame_swaps_bgr_to_rgb_and_drops_padding() {
        // 2x1 bgr0, stride padded by 4 extra bytes past width*4.
        let frame = RawFrame {
            width: 2,
            height: 1,
            stride: 12,
            data: vec![
                10, 20, 30, 0, // pixel 0: b=10 g=20 r=30
                40, 50, 60, 0, // pixel 1: b=40 g=50 r=60
                0, 0, 0, 0, // row padding, must be ignored
            ],
        };
        let img = frame.to_image().expect("valid frame");
        assert_eq!(img.get_pixel(0, 0).0, [30, 20, 10]);
        assert_eq!(img.get_pixel(1, 0).0, [60, 50, 40]);
    }

    #[test]
    fn raw_frame_rejects_data_too_short_for_its_own_dimensions() {
        let frame = RawFrame {
            width: 4,
            height: 4,
            stride: 16,
            data: vec![0; 4],
        };
        assert!(frame.to_image().is_none());
    }

    #[test]
    fn playback_folds_the_three_observed_properties() {
        let mut pb = Playback::default();
        assert!(pb.apply(PROP_TIME_POS, &json!(12.5)));
        assert!(pb.apply(PROP_DURATION, &json!(200.0)));
        assert!(pb.apply(PROP_PAUSE, &json!(true)));
        assert_eq!(pb.elapsed_secs, Some(12.5));
        assert_eq!(pb.duration_secs, Some(200.0));
        assert!(pb.paused);
    }

    #[test]
    fn an_unrelated_property_is_left_for_the_caller() {
        let mut pb = Playback::default();
        assert!(!pb.apply(PROP_PLAYLIST_POS, &json!(3)));
        assert!(!pb.apply("some-other-prop", &json!(1)));
        assert_eq!(pb.elapsed_secs, None);
    }

    #[test]
    fn a_null_value_clears_rather_than_leaving_a_stale_reading() {
        let mut pb = Playback::default();
        pb.apply(PROP_TIME_POS, &json!(42.0));
        pb.apply(PROP_DURATION, &json!(90.0));
        // mpv sends null for a property with no current value, which
        // is what happens when the playlist drains.
        pb.apply(PROP_TIME_POS, &Value::Null);
        pb.apply(PROP_DURATION, &Value::Null);
        assert_eq!(
            pb.elapsed_secs, None,
            "a stale position would keep the scrubber moving"
        );
        assert_eq!(pb.duration_secs, None);
    }

    #[test]
    fn an_absent_pause_reads_as_not_paused() {
        let mut pb = Playback::default();
        pb.apply(PROP_PAUSE, &json!(true));
        pb.apply(PROP_PAUSE, &Value::Null);
        assert!(!pb.paused);
    }

    /// The property path and the af label are one contract written in
    /// two places; mpv resolves neither half on its own.
    #[test]
    fn the_level_property_path_matches_the_af_label() {
        assert_eq!(
            PROP_LEVEL_RMS,
            format!(
                "af-metadata/{}/lavfi.astats.Overall.RMS_level",
                crate::eq::LEVEL_METADATA_LABEL
            )
        );
    }

    /// mpv hands af-metadata through as the string the filter wrote,
    /// not as a json number; reading it as a number gets `None` for
    /// every frame, which is a silently dead meter.
    #[test]
    fn a_level_reading_arrives_as_a_string() {
        let mut pb = Playback::default();
        assert!(pb.apply(PROP_LEVEL_RMS, &json!("-21.008032")));
        let level = pb.level().expect("fresh reading");
        assert!((level - (60.0 - 21.008032) / 60.0).abs() < 1e-5, "{level}");
    }

    #[test]
    fn a_level_reading_also_accepts_a_number() {
        let mut pb = Playback::default();
        pb.apply(PROP_LEVEL_RMS, &json!(-30.0));
        assert_eq!(pb.level(), Some(0.5));
    }

    #[test]
    fn digital_silence_reads_as_the_bottom_of_the_meter() {
        let mut pb = Playback::default();
        pb.apply(PROP_LEVEL_RMS, &json!("-inf"));
        assert_eq!(pb.level(), Some(0.0));
    }

    /// af cleared or playback stopped: mpv pushes a change with no
    /// value at all, which must drop the meter back to its fallback
    /// rather than leaving the last reading on screen.
    #[test]
    fn a_vanished_level_reading_clears_immediately() {
        let mut pb = Playback::default();
        pb.apply(PROP_LEVEL_RMS, &json!("-12.0"));
        assert!(pb.level().is_some());
        pb.apply(PROP_LEVEL_RMS, &Value::Null);
        assert_eq!(pb.level(), None);
    }

    /// mpv simply stops pushing while paused, so a reading is only
    /// believed for `LEVEL_FRESH_FOR` past when it landed.
    #[test]
    fn a_stale_level_reading_is_not_believed() {
        let mut pb = Playback::default();
        pb.apply(PROP_LEVEL_RMS, &json!("-12.0"));
        let landed = pb.level_at.expect("timestamped");
        assert!(pb.level_as_of(landed + LEVEL_FRESH_FOR).is_some());
        assert!(pb
            .level_as_of(landed + LEVEL_FRESH_FOR + Duration::from_millis(1))
            .is_none());
    }

    #[test]
    fn a_zero_sized_frame_is_rejected_rather_than_panicking() {
        let frame = RawFrame {
            width: 0,
            height: 4,
            stride: 0,
            data: vec![],
        };
        assert!(frame.to_image().is_none());
    }

    #[test]
    fn a_screenshot_reply_without_the_fields_mpv_promises_is_rejected() {
        assert!(parse_raw_frame(&json!({"w": 2, "h": 1, "stride": 8})).is_none());
        assert!(parse_raw_frame(&json!(null)).is_none());
        // what mpv 0.41 actually sends over json ipc, once the
        // unparseable line is coerced: a map with no `data` at all.
        assert!(
            parse_raw_frame(&json!({"w": 640, "h": 480, "stride": 2560, "format": "bgr0"}))
                .is_none()
        );
    }

    #[test]
    fn capture_off_is_off() {
        let off = Capture::off();
        assert!(!off.on);
        assert_eq!(off.max_edge, None);
    }
}
