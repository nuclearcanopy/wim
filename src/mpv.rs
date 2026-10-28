use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
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
            // not from whatever vo would've shown it in.
            .arg("--vo=null")
            .arg(format!("--input-ipc-server={}", sock_path.display()))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
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

    /// `"weak"`: a no-op (not an error) if there's nothing further
    /// along mpv's own playlist, since it already holds every queued
    /// track (see `playlist_append`) and knows better than we do
    /// whether there's anywhere to skip to.
    pub fn playlist_next(&self) -> Result<()> {
        self.command(&[json!("playlist-next"), json!("weak")])
            .map(|_| ())
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

    /// One video frame as raw `bgr0` pixels, for embedding video in
    /// the workspace (see `main::VideoPane`/`main::capture_video_frame`)
    /// instead of leaving it to mpv's own native window. Round-trips
    /// over the same IPC socket as every other command, so calling
    /// this on every render tick would stall the UI on each reply;
    /// callers are expected to throttle.
    pub fn screenshot_raw(&self) -> Result<RawFrame> {
        let v = self.command(&[json!("screenshot-raw")])?;
        let width = v.get("w").and_then(Value::as_u64).context("missing w")? as u32;
        let height = v.get("h").and_then(Value::as_u64).context("missing h")? as u32;
        let stride = v
            .get("stride")
            .and_then(Value::as_u64)
            .context("missing stride")? as u32;
        let data_b64 = v
            .get("data")
            .and_then(Value::as_str)
            .context("missing data")?;
        let data = base64::Engine::decode(&base64::engine::general_purpose::STANDARD, data_b64)
            .context("decoding screenshot-raw's base64 data")?;
        Ok(RawFrame {
            width,
            height,
            stride,
            data,
        })
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
    pub fn into_image(self) -> Option<image::RgbImage> {
        let mut rgb = Vec::with_capacity((self.width * self.height * 3) as usize);
        for row in 0..self.height {
            let row_start = (row * self.stride) as usize;
            for col in 0..self.width {
                let px = row_start + (col * 4) as usize;
                let pixel = self.data.get(px..px + 3)?;
                rgb.push(pixel[2]); // r
                rgb.push(pixel[1]); // g
                rgb.push(pixel[0]); // b
            }
        }
        image::RgbImage::from_raw(self.width, self.height, rgb)
    }
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
        let img = frame.into_image().expect("valid frame");
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
        assert!(frame.into_image().is_none());
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

}
