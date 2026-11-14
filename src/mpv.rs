use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
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
const OBS_EOF_REACHED: u64 = 7;
const OBS_MUTE: u64 = 8;
const OBS_SPECTRUM: u64 = 9;
const OBS_PAUSED_FOR_CACHE: u64 = 10;
const OBS_BUFFERING_STATE: u64 = 11;
const OBS_CACHE_AHEAD: u64 = 12;
const OBS_SEEKING: u64 = 13;

/// How long any one blocking command waits for mpv's reply.
const REPLY_TIMEOUT: Duration = Duration::from_secs(10);

/// Commands whose reply never came. A timed-out `loadfile` may still run
/// in mpv later, so the app's queue mirror can no longer be trusted until
/// it compares itself with mpv; the main loops watch this counter.
static REPLY_TIMEOUTS: AtomicU64 = AtomicU64::new(0);

pub fn reply_timeouts() -> u64 {
    REPLY_TIMEOUTS.load(Ordering::Relaxed)
}

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
/// mpv's own mute. Observed alongside `pause` because muted and
/// "volume happens to be zero" are the same silence on screen, so the
/// bar needs to be able to tell them apart.
pub const PROP_MUTE: &str = "mute";
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
/// mpv paused *itself* because the cache ran dry — the network stall
/// a motionless progress bar otherwise hides. Not the user's pause;
/// `playback_status` exists to keep those two apart on screen.
pub const PROP_PAUSED_FOR_CACHE: &str = "paused-for-cache";
/// mpv's own estimate of how full the cache is, 0-100, pushed while
/// it waits. `None` when mpv is not currently stalled.
pub const PROP_BUFFERING_STATE: &str = "cache-buffering-state";
/// End timestamp of the demuxer's cached range. Subtract `time-pos`
/// to get seconds buffered ahead; `demuxer-cache-duration` is the
/// *span* of all cached data and is not an ahead-of-position measure.
pub const PROP_CACHE_AHEAD: &str = "demuxer-cache-time";
/// True while a seek is in flight.
pub const PROP_SEEKING: &str = "seeking";
/// RMS output level in dBFS, from the first (dry-left) channel in the
/// labeled `astats` analysis filter. Spelled out rather than built with
/// `format!` so it can be matched in `Playback::apply` for free; a
/// test ties it back to `eq::LEVEL_METADATA_LABEL`.
///
/// The label has to be mpv's *own* filter label (`af=@wimlevel:lavfi=
/// [...]`), not a label inside the lavfi graph: `af-metadata/<label>`
/// resolves nothing at all for the latter.
pub const PROP_LEVEL_RMS: &str = "af-metadata/wimlevel/lavfi.astats.1.RMS_level";
/// The complete astats metadata object. Each analysis band occupies
/// one mono channel after the dry stereo pair.
pub const PROP_SPECTRUM: &str = "af-metadata/wimlevel";

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
    pub muted: bool,
    /// mpv is holding at the end of a file under `keep-open`; see
    /// `PROP_EOF_REACHED`.
    pub eof_reached: bool,
    /// mpv paused itself waiting for the cache (the network stall);
    /// see `PROP_PAUSED_FOR_CACHE`. Deliberately not folded into
    /// `paused`: the two need different words on screen.
    pub paused_for_cache: bool,
    /// mpv's buffering progress while it waits, 0-100.
    pub buffering_pct: Option<f64>,
    /// Media seconds buffered ahead of the position.
    pub cache_ahead_secs: Option<f64>,
    cache_end_secs: Option<f64>,
    /// A seek is in flight.
    pub seeking: bool,
    /// Last pushed `astats` RMS reading and when it landed; private
    /// because the pair is only meaningful together, see `level`.
    level_db: Option<f64>,
    level_at: Option<Instant>,
    spectrum_level: [f32; 32],
    spectrum_bands: usize,
    spectrum_at: Option<Instant>,
}

#[derive(Debug, Clone)]
pub struct SubtitleTrackInfo {
    pub id: u64,
    pub ff_index: Option<u64>,
    pub external_path: Option<PathBuf>,
    pub label: String,
}

impl Playback {
    /// Folds one observed change in, returning whether it was one of
    /// ours. mpv sends `null` for a property that currently has no
    /// value (no file loaded), which is `None` rather than stale.
    pub fn apply(&mut self, name: &str, value: &Value) -> bool {
        match name {
            PROP_TIME_POS => {
                self.elapsed_secs = value.as_f64();
                self.update_cache_ahead();
            }
            PROP_DURATION => self.duration_secs = value.as_f64(),
            // an absent `pause` means nothing is loaded, which reads
            // as not-paused everywhere downstream.
            PROP_PAUSE => self.paused = value.as_bool().unwrap_or(false),
            PROP_MUTE => self.muted = value.as_bool().unwrap_or(false),
            // absent reads as "not at a held end", same as `pause`.
            PROP_EOF_REACHED => self.eof_reached = value.as_bool().unwrap_or(false),
            // absent reads as "not stalled" / "not seeking" / "no
            // cache report", same reasoning as `pause`: a property
            // with no value must not read as a stall.
            // mpv exposes this as the number of seconds it has been
            // paused for cache (not a boolean); tolerate a boolean too
            // for versions/builds that type it differently.
            PROP_PAUSED_FOR_CACHE => {
                self.paused_for_cache = value
                    .as_bool()
                    .or_else(|| value.as_f64().map(|secs| secs > 0.0))
                    .unwrap_or(false)
            }
            PROP_BUFFERING_STATE => self.buffering_pct = value.as_f64(),
            PROP_CACHE_AHEAD => {
                self.cache_end_secs = value.as_f64();
                self.update_cache_ahead();
            }
            PROP_SEEKING => self.seeking = value.as_bool().unwrap_or(false),
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
            PROP_SPECTRUM => {
                self.spectrum_level = [0.0; 32];
                self.spectrum_bands = 0;
                if let Some(metadata) = value.as_object() {
                    for (key, value) in metadata {
                        let Some(channel) = key
                            .strip_prefix("lavfi.astats.")
                            .and_then(|key| key.strip_suffix(".RMS_level"))
                            .and_then(|channel| channel.parse::<usize>().ok())
                        else {
                            continue;
                        };
                        // The dry stereo output is channels 1/2; analysis
                        // bands follow as mono channels. Iterating the small
                        // metadata map avoids formatting and allocating 32
                        // lookup keys on every update.
                        let Some(band) = channel.checked_sub(3).filter(|&i| i < 32) else {
                            continue;
                        };
                        let Some(db) = value
                            .as_f64()
                            .or_else(|| value.as_str().and_then(|s| s.parse().ok()))
                        else {
                            continue;
                        };
                        self.spectrum_level[band] = if db.is_finite() {
                            ((db as f32 + 58.0) / 48.0).clamp(0.0, 1.0)
                        } else {
                            0.0
                        };
                        self.spectrum_bands = self.spectrum_bands.max(band + 1);
                    }
                }
                self.spectrum_at = (self.spectrum_bands > 0).then(Instant::now);
            }
            _ => return false,
        }
        true
    }

    fn update_cache_ahead(&mut self) {
        self.cache_ahead_secs = self
            .cache_end_secs
            .zip(self.elapsed_secs)
            .map(|(end, pos)| (end - pos).max(0.0));
    }

    /// Last output level reading, retained for callers that need a
    /// scalar meter. Returns `None` when no fresh reading is in hand.
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

    /// Frequency-band levels from the last astats metadata event, if
    /// fresh. Missing channels (e.g. configured bands beyond the graph)
    /// are returned as silence.
    pub fn spectrum(&self, bands: usize) -> Option<&[f32]> {
        let now = Instant::now();
        if now.duration_since(self.spectrum_at?) > LEVEL_FRESH_FOR || self.paused {
            return None;
        }
        Some(&self.spectrum_level[..bands.min(self.spectrum_bands)])
    }
}

struct Pending {
    reply_tx: Sender<Result<Value, String>>,
}

/// Cloneable IPC side of the player. The child and event receiver remain on
/// the main thread; only bounded startup work uses this handle on a worker.
pub struct CommandClient {
    sock: Arc<Mutex<Link>>,
    next_id: Arc<AtomicU64>,
    pending: Arc<Mutex<HashMap<u64, Pending>>>,
}

impl CommandClient {
    pub fn command(&self, args: &[Value]) -> Result<Value> {
        self.command_batch(&[args.to_vec()])
            .pop()
            .expect("one reply per command")
    }

    pub fn command_batch(&self, cmds: &[Vec<Value>]) -> Vec<Result<Value>> {
        command_batch_on(&self.sock, &self.next_id, &self.pending, cmds)
    }

    pub fn observe_playback(&self) -> Result<()> {
        for result in self.command_batch(&observe_commands()) {
            result?;
        }
        Ok(())
    }
}

struct BackgroundCommand(Arc<AtomicUsize>);

impl Drop for BackgroundCommand {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

/// A running mpv instance controlled over its JSON IPC socket, rather than
/// linking libmpv directly; keeps the build free of C FFI/pkg-config and
/// matches how mpv is normally driven by external controllers.
pub struct Mpv {
    child: Mutex<Child>,
    sock_path: PathBuf,
    /// Resolved on the first command rather than at spawn.
    ///
    /// mpv creates its ipc socket 65-120ms after exec, which was 97%
    /// of wim's whole startup (72.7ms of 73.5ms, measured) and all of
    /// it spent waiting. fork and exec cost 0.4ms, so the wait is the
    /// only expensive part and it does not have to be on the critical
    /// path: nothing can be played until the user asks for something.
    sock: Arc<Mutex<Link>>,
    next_id: Arc<AtomicU64>,
    pending: Arc<Mutex<HashMap<u64, Pending>>>,
    events: Receiver<Event>,
    background_commands: Arc<AtomicUsize>,
}

/// The ipc socket, before and after the connector thread finds it.
enum Link {
    /// Waiting on the connector. The receiver yields exactly once.
    Pending(Receiver<Result<UnixStream, String>>),
    Live(UnixStream),
    /// mpv never came up. Kept rather than retried: a connect that
    /// already spent its budget will not start working, and every
    /// later command should say the same thing instead of each paying
    /// the budget again.
    Dead(String),
}

/// Request ids the connector's own setup uses, so they cannot collide
/// with `next_id`'s. Nothing waits on these replies; the reader drops
/// a reply whose id is not in `pending`, which is the same path a
/// timed-out command's late reply takes.
const SETUP_ID_BASE: u64 = 1;
/// Where ordinary commands start, past the setup block.
const FIRST_COMMAND_ID: u64 = 1000;

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
        // $XDG_RUNTIME_DIR is a private 0700 directory; a predictable name
        // in shared /tmp lets another local user squat the path.
        let sock_dir = std::env::var_os("XDG_RUNTIME_DIR")
            .map(PathBuf::from)
            .filter(|dir| dir.is_dir())
            .unwrap_or_else(std::env::temp_dir);
        let sock_path = sock_dir.join(format!(
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
            // `--no-config` leaves ~/.config/mpv/scripts loading; a script
            // there runs with the full IPC socket's reach. Same for
            // yt-dlp: stream urls here come from servers and providers,
            // and none of them should be handed to an external extractor.
            .arg("--load-scripts=no")
            .arg("--ytdl=no")
            // mpv owns video playback and opens its normal native window
            // when `vid=auto`; music items explicitly use `vid=no`.
            // Keep the window optional while idle so audio-only playback
            // remains a quiet backend for Wim.
            .arg("--vo=gpu")
            // Prefer mpv's safe hardware decoders when the system exposes
            // them, while retaining its software fallback for unsupported
            // codecs or broken driver paths. `vo=gpu` alone only selects a
            // renderer; it does not enable hardware video decoding.
            .arg("--hwdec=auto-safe")
            .arg("--force-window=no")
            .arg(format!("--input-ipc-server={}", sock_path.display()))
            // stdin stays closed: `--no-terminal` already implies
            // `--no-input-terminal`, and mpv must never compete with the
            // tui for keystrokes. stdout is this terminal, because mpv's
            // stderr
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

        let pending: Arc<Mutex<HashMap<u64, Pending>>> = Arc::new(Mutex::new(HashMap::new()));
        let (event_tx, event_rx) = mpsc::channel();
        let (link_tx, link_rx) = mpsc::channel();

        // the connect wait, off the critical path. the fork above
        // stays on this thread on purpose: `reap_with_parent` sets
        // `PR_SET_PDEATHSIG`, which fires when the parent *thread*
        // exits, so forking from a worker would kill mpv the moment
        // that worker finished.
        let connect_path = sock_path.clone();
        let reader_pending = pending.clone();
        thread::spawn(move || {
            let sock = match connect(&connect_path) {
                Ok(sock) => sock,
                Err(why) => {
                    let _ = link_tx.send(Err(why));
                    return;
                }
            };
            let Ok(reader_sock) = sock.try_clone() else {
                let _ = link_tx.send(Err("cloning mpv ipc socket".to_string()));
                return;
            };
            // A stalled mpv must not block a writer (holding the link lock)
            // forever. SO_SNDTIMEO only affects writes, not the reader.
            let _ = sock.set_write_timeout(Some(Duration::from_secs(2)));
            thread::spawn(move || read_replies(reader_sock, reader_pending, event_tx));
            // the setup goes down the socket *before* it is published,
            // so a command the user typed in the first 70ms cannot
            // overtake the property observers or replaygain: it cannot
            // reach the socket until this send happens.
            let mut sock = sock;
            if let Err(why) = write_setup(&mut sock) {
                let _ = link_tx.send(Err(why));
                return;
            }
            let _ = link_tx.send(Ok(sock));
        });

        Ok(Mpv {
            child: Mutex::new(child),
            sock_path,
            sock: Arc::new(Mutex::new(Link::Pending(link_rx))),
            next_id: Arc::new(AtomicU64::new(FIRST_COMMAND_ID)),
            pending,
            events: event_rx,
            background_commands: Arc::new(AtomicUsize::new(0)),
        })
    }

    /// Resolves the socket if the connector has finished, and runs `f`
    /// on it. The first caller pays whatever is left of mpv's startup;
    /// every one after it pays a mutex.
    fn with_sock<R>(&self, f: impl FnOnce(&mut UnixStream) -> Result<R>) -> Result<R> {
        with_sock(&self.sock, f)
    }
}

fn with_sock<R>(link: &Mutex<Link>, f: impl FnOnce(&mut UnixStream) -> Result<R>) -> Result<R> {
    let mut guard = link.lock().unwrap();
    if let Link::Pending(rx) = &*guard {
        *guard = match rx.recv() {
            Ok(Ok(sock)) => Link::Live(sock),
            Ok(Err(why)) => Link::Dead(why),
            Err(_) => Link::Dead("the mpv connector thread died".to_string()),
        };
    }
    match &mut *guard {
        Link::Live(sock) => f(sock),
        Link::Dead(why) => bail!("{why}"),
        Link::Pending(_) => unreachable!("resolved just above"),
    }
}

/// mpv creates the socket asynchronously after exec, 65-120ms in on
/// this box. A flat 20ms sleep per attempt spent most of that asleep
/// past the socket actually being ready, so back off instead: 1ms
/// doubling to 20ms, same ~1s total budget.
fn connect(sock_path: &Path) -> Result<UnixStream, String> {
    const CONNECT_BUDGET: Duration = Duration::from_secs(1);
    const MAX_BACKOFF: Duration = Duration::from_millis(20);
    let mut waited = Duration::ZERO;
    let mut backoff = Duration::from_millis(1);
    loop {
        match UnixStream::connect(sock_path) {
            Ok(s) => return Ok(s),
            Err(_) if waited < CONNECT_BUDGET => {
                thread::sleep(backoff);
                waited += backoff;
                backoff = (backoff * 2).min(MAX_BACKOFF);
            }
            // the path is deliberately not in the message: it is a
            // temp socket name, but every other error string in this
            // repo that carried a path or url turned out to carry a
            // credential too, so the habit is the rule.
            Err(e) => return Err(format!("connecting to mpv ipc socket: {}", e.kind())),
        }
    }
}

/// The one setting that is safe to apply before the socket is handed
/// over: per-track replaygain.
///
/// Order-independent, which is why it can live here: mpv reads the
/// gain from each file's own tags as it plays, so setting it when no
/// file exists yet is exactly right. **The property observers are
/// deliberately not here.** Registering one makes mpv report that
/// property's current value at once, and wim registers them only
/// after restoring last session's track, or the first `idle-active`
/// describes an mpv that has not started playing. That ordering is
/// the front-end's to choose, so each one still calls
/// `observe_playback` itself.
fn write_setup(sock: &mut UnixStream) -> Result<(), String> {
    let lines: Vec<Vec<Value>> = vec![vec![
        json!("set_property"),
        json!("replaygain"),
        json!("track"),
    ]];
    for (n, args) in lines.into_iter().enumerate() {
        let payload = json!({ "command": args, "request_id": SETUP_ID_BASE + n as u64 });
        let mut line = match serde_json::to_vec(&payload) {
            Ok(line) => line,
            Err(e) => return Err(format!("encoding mpv setup: {e}")),
        };
        // leading newline: a write that timed out midway leaves a fragment
        // in mpv's buffer, which this makes its own (dropped) line
        line.insert(0, b'\n');
        line.push(b'\n');
        if let Err(e) = sock.write_all(&line) {
            return Err(format!("writing mpv setup: {}", e.kind()));
        }
    }
    Ok(())
}

/// Every property the frame loop reads, as `observe_property` calls.
///
/// One list, because the connector sends it at startup and
/// `observe_playback` re-sends it after a reconnect; two copies would
/// drift and the symptom is a reading that silently never updates.
fn observe_commands() -> Vec<Vec<Value>> {
    [
        (OBS_PLAYLIST_POS, PROP_PLAYLIST_POS),
        (OBS_IDLE_ACTIVE, PROP_IDLE_ACTIVE),
        (OBS_TIME_POS, PROP_TIME_POS),
        (OBS_DURATION, PROP_DURATION),
        (OBS_PAUSE, PROP_PAUSE),
        (OBS_MUTE, PROP_MUTE),
        (OBS_LEVEL_RMS, PROP_LEVEL_RMS),
        (OBS_SPECTRUM, PROP_SPECTRUM),
        (OBS_EOF_REACHED, PROP_EOF_REACHED),
        (OBS_PAUSED_FOR_CACHE, PROP_PAUSED_FOR_CACHE),
        (OBS_BUFFERING_STATE, PROP_BUFFERING_STATE),
        (OBS_CACHE_AHEAD, PROP_CACHE_AHEAD),
        (OBS_SEEKING, PROP_SEEKING),
    ]
    .into_iter()
    .map(|(id, name)| vec![json!("observe_property"), json!(id), json!(name)])
    .collect()
}

/// The reply reader, formerly inline in `spawn`.
fn read_replies(
    reader_sock: UnixStream,
    reader_pending: Arc<Mutex<HashMap<u64, Pending>>>,
    event_tx: Sender<Event>,
) {
    let mut reader = BufReader::new(reader_sock);
    let mut raw = Vec::new();
    loop {
        raw.clear();
        // Bytes, not `lines()`: one non-UTF-8 line must not end the reader
        // (and fail every pending request) while mpv is still alive.
        match reader.read_until(b'\n', &mut raw) {
            Ok(0) | Err(_) => break,
            Ok(_) => {}
        }
        let line = String::from_utf8_lossy(&raw).into_owned();
        if line.trim().is_empty() {
            continue;
        }
        let Ok(v) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        if let Some(id) = v.get("request_id").and_then(Value::as_u64) {
            if let Some(p) = reader_pending.lock().unwrap().remove(&id) {
                let result = match v.get("error").and_then(Value::as_str) {
                    Some("success") | None => Ok(v.get("data").cloned().unwrap_or(Value::Null)),
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
            Some(_name) => {
                // event names only ("end-file", "file-loaded", …), and
                // never the payload: payloads can carry paths, and the
                // names alone already show the load/pause/eof sequence
                // a playback attempt made. `tick` fires about once a
                // second forever, so it is noise here. The log's value
                // type is a fixed vocabulary, so the names are matched
                // to it here rather than passed through: anything not
                // in the list is "other", which still shows the
                // sequence without inventing a text channel.
                #[cfg(feature = "dev-telemetry")]
                if _name != "tick" {
                    const LOGGED_EVENTS: &[&str] = &[
                        "file-loaded",
                        "end-file",
                        "start-file",
                        "idle",
                        "pause",
                        "unpause",
                        "seek",
                        "playback-restart",
                    ];
                    #[cfg(feature = "dev-telemetry")]
                    let label = LOGGED_EVENTS
                        .iter()
                        .find(|logged| **logged == _name)
                        .copied()
                        .unwrap_or("other");
                    crate::trace_diag!(
                        "mpv",
                        "event",
                        &[("event", crate::trace::Value::Label(label))],
                    );
                }
                let _ = event_tx.send(Event::Other(v));
            }
            None => {}
        }
    }
    // A dead player cannot answer any outstanding requests. Wake every
    // waiter immediately rather than spending a timeout per startup batch.
    if let Ok(mut pending) = reader_pending.lock() {
        for (_, request) in pending.drain() {
            let _ = request
                .reply_tx
                .send(Err("mpv ipc disconnected".to_string()));
        }
    }
}

fn command_batch_on(
    sock: &Mutex<Link>,
    next_id: &AtomicU64,
    pending: &Mutex<HashMap<u64, Pending>>,
    cmds: &[Vec<Value>],
) -> Vec<Result<Value>> {
    let mut ids = Vec::with_capacity(cmds.len());
    let mut rxs = Vec::with_capacity(cmds.len());
    let mut wire = Vec::new();
    {
        let mut pending = pending.lock().unwrap();
        for args in cmds {
            let id = next_id.fetch_add(1, Ordering::SeqCst);
            let (tx, rx) = mpsc::channel();
            pending.insert(id, Pending { reply_tx: tx });
            ids.push(id);
            rxs.push(rx);
            let payload = json!({ "command": args, "request_id": id });
            let mut line = serde_json::to_vec(&payload).unwrap_or_default();
            // leading newline: a write that timed out midway leaves a fragment
            // in mpv's buffer, which this makes its own (dropped) line
            line.insert(0, b'\n');
            line.push(b'\n');
            wire.extend_from_slice(&line);
        }
    }
    if let Err(e) = with_sock(sock, |sock| Ok(sock.write_all(&wire)?)) {
        let mut pending = pending.lock().unwrap();
        for id in &ids {
            pending.remove(id);
        }
        return (0..cmds.len())
            .map(|_| Err(anyhow::anyhow!("writing to mpv ipc socket: {e}")))
            .collect();
    }
    let deadline = Instant::now() + REPLY_TIMEOUT;
    ids.into_iter()
        .zip(rxs)
        .map(
            |(id, rx)| match rx.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
                Ok(Ok(v)) => Ok(v),
                Ok(Err(e)) => Err(anyhow::anyhow!("mpv command error: {e}")),
                Err(_) => {
                    pending.lock().unwrap().remove(&id);
                    REPLY_TIMEOUTS.fetch_add(1, Ordering::Relaxed);
                    Err(anyhow::anyhow!("timed out waiting for mpv reply"))
                }
            },
        )
        .collect()
}

impl Mpv {
    /// Whether mpv has exited on its own (crashed, was killed, or quit).
    /// Takes `&self`: the child handle lives behind a mutex so the
    /// per-frame check needs no exclusive access to the player.
    pub fn exited(&self) -> Option<String> {
        let mut child = self
            .child
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        match child.try_wait() {
            Ok(Some(status)) => Some(status.to_string()),
            _ => None,
        }
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
        let sent = serde_json::to_vec(&payload)
            .map_err(anyhow::Error::from)
            .and_then(|mut line| {
                // leading newline: a write that timed out midway leaves a fragment
                // in mpv's buffer, which this makes its own (dropped) line
                line.insert(0, b'\n');
                line.push(b'\n');
                self.with_sock(|sock| sock.write_all(&line).context("writing to mpv ipc socket"))
            });
        if let Err(e) = sent {
            // nothing will ever answer this id; do not leave it queued
            self.pending.lock().unwrap().remove(&id);
            return Err(e);
        }

        match rx.recv_timeout(REPLY_TIMEOUT) {
            Ok(Ok(v)) => Ok(v),
            Ok(Err(e)) => bail!("mpv command error: {e}"),
            Err(_) => {
                self.pending.lock().unwrap().remove(&id);
                REPLY_TIMEOUTS.fetch_add(1, Ordering::Relaxed);
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
        let sent = serde_json::to_vec(&payload)
            .map_err(anyhow::Error::from)
            .and_then(|mut line| {
                // leading newline: a write that timed out midway leaves a fragment
                // in mpv's buffer, which this makes its own (dropped) line
                line.insert(0, b'\n');
                line.push(b'\n');
                self.with_sock(|sock| sock.write_all(&line).context("writing to mpv ipc socket"))
            });
        if let Err(e) = sent {
            // nothing will ever answer this id; do not leave it queued
            self.pending.lock().unwrap().remove(&id);
            return Err(e);
        }
        Ok(())
    }

    /// Writes every command, then collects every reply: the whole
    /// batch costs one round trip's latency instead of one each, since
    /// mpv reads a connection's commands in order and answers them in
    /// order. One result per input, in the same order; a failure in
    /// one says nothing about the others.
    pub fn command_batch(&self, cmds: &[Vec<Value>]) -> Vec<Result<Value>> {
        command_batch_on(&self.sock, &self.next_id, &self.pending, cmds)
    }

    /// Run ordered setup commands without making the terminal thread wait for
    /// either the socket or mpv's replies. The child itself is still spawned
    /// on the main thread, as required by Linux's parent-death signal.
    pub fn command_batch_background(&self, cmds: Vec<Vec<Value>>) -> Receiver<Vec<Result<Value>>> {
        self.run_background(move |client| client.command_batch(&cmds))
    }

    pub fn run_background<T: Send + 'static>(
        &self,
        run: impl FnOnce(CommandClient) -> T + Send + 'static,
    ) -> Receiver<T> {
        let client = CommandClient {
            sock: Arc::clone(&self.sock),
            next_id: Arc::clone(&self.next_id),
            pending: Arc::clone(&self.pending),
        };
        let active = Arc::clone(&self.background_commands);
        let (tx, rx) = mpsc::channel();
        active.fetch_add(1, Ordering::SeqCst);
        thread::spawn(move || {
            let guard = BackgroundCommand(active);
            let result = run(client);
            drop(guard);
            let _ = tx.send(result);
        });
        rx
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
        for result in self.command_batch(&observe_commands()) {
            result?;
        }
        Ok(())
    }

    pub fn observe_playback_background(&self) -> Receiver<Vec<Result<Value>>> {
        self.command_batch_background(observe_commands())
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

    /// Appends several entries in playlist order with one socket write
    /// and one IPC wait, retaining one result per media item. This is
    /// for bulk restoration, not interactive enqueue: the caller must
    /// update its mirrored queue only for successful replies.
    pub fn playlist_append_batch(&self, paths: &[String]) -> Vec<Result<()>> {
        let commands = paths
            .iter()
            .map(|path| vec![json!("loadfile"), json!(path), json!("append-play")])
            .collect::<Vec<_>>();
        self.command_batch(&commands)
            .into_iter()
            .map(|result| result.map(|_| ()))
            .collect()
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

    /// `"weak"` means "do not stop playback if there is nowhere to
    /// go", which is not the same as succeeding.
    ///
    /// **Measured, because the comment here used to claim the
    /// opposite**: with a single entry loaded, `["playlist-next",
    /// "weak"]` replies `{"error":"error running command"}`. It does
    /// leave the track playing and `playlist-pos` where it was, so
    /// nothing breaks, but the reply *is* an error and anything
    /// surfacing it shows the user one. Callers that know whether
    /// there is a next entry should say so themselves rather than let
    /// this answer for them; see `Op::SkipNext`.
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

    /// How far ahead of the position mpv keeps the demuxer cache,
    /// in media seconds. This is the preload-while-paused knob: mpv
    /// keeps reading ahead while paused, but its default
    /// `demuxer-readahead-secs` is **1** (measured on 0.41.0), so a
    /// pause on default settings buffers almost nothing and the
    /// "pause it and let it load" move silently doesn't work. The
    /// byte cap (`demuxer-max-bytes`, 150 MiB by default) stays
    /// alone: it is generous enough for minutes of audio and most
    /// video, and a second knob to keep them consistent is worse
    /// than one that means what it says.
    ///
    /// Applied before the first load (the caller does this right
    /// after spawn): the property is per-file in mpv, so a mid-run
    /// change only takes effect on the next track.
    pub fn set_buffer_ahead(&self, secs: f64) -> Result<()> {
        let secs = secs.max(1.0);
        self.set_property("demuxer-readahead-secs", json!(secs))
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
        let _ = self.command_batch(&Self::subtitle_style_commands(cfg));
    }

    /// The same ordered commands can be included in asynchronous startup.
    pub fn subtitle_style_commands(cfg: &crate::config::SubtitleConfig) -> Vec<Vec<Value>> {
        vec![
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
        ]
    }

    /// Loads an external subtitle file onto the currently playing
    /// video (e.g. one just downloaded from opensubtitles) and
    /// switches to it immediately ("select" flag), rather than just
    /// adding it to the track list unselected.
    pub fn sub_add(&self, path: &str) -> Result<()> {
        self.command(&[json!("sub-add"), json!(path), json!("select")])?;
        Ok(())
    }

    /// Cycle through the subtitle tracks reported by mpv. Uses mpv's
    /// track metadata so embedded tracks and already-loaded sidecars share
    /// one selection path; the final position disables subtitles.
    pub fn cycle_subtitle_track(&self) -> Result<Option<String>> {
        let tracks = self.get_property("track-list")?;
        let subtitle_tracks = tracks
            .as_array()
            .into_iter()
            .flatten()
            .filter(|track| track.get("type").and_then(Value::as_str) == Some("sub"))
            .collect::<Vec<_>>();
        if subtitle_tracks.is_empty() {
            self.set_property("sid", json!("no"))?;
            return Ok(None);
        }
        let active = self
            .get_property("sid")
            .ok()
            .and_then(|value| value.as_u64());
        let next = active
            .and_then(|id| {
                subtitle_tracks
                    .iter()
                    .position(|track| track.get("id").and_then(Value::as_u64) == Some(id))
            })
            .map_or(0, |index| index + 1);
        if next >= subtitle_tracks.len() {
            self.set_property("sid", json!("no"))?;
            return Ok(Some("subtitles off".to_owned()));
        }
        let track = subtitle_tracks[next];
        let id = track
            .get("id")
            .and_then(Value::as_u64)
            .context("mpv subtitle track has no id")?;
        self.set_property("sid", json!(id))?;
        let language = track
            .get("lang")
            .and_then(Value::as_str)
            .unwrap_or("unknown");
        let title = track
            .get("title")
            .and_then(Value::as_str)
            .unwrap_or("subtitle track");
        Ok(Some(format!("subtitle: {language} · {title}")))
    }

    pub fn toggle_subtitle_visibility(&self) -> Result<bool> {
        let visible = self
            .get_property("sub-visibility")?
            .as_bool()
            .unwrap_or(true);
        self.set_property("sub-visibility", json!(!visible))?;
        Ok(!visible)
    }

    /// Returns the selected subtitle's source without exposing it in diagnostics.
    pub fn selected_subtitle_track(&self) -> Result<Option<SubtitleTrackInfo>> {
        let tracks = self.get_property("track-list")?;
        let sid = self
            .get_property("sid")
            .ok()
            .and_then(|value| value.as_u64());
        Ok(tracks
            .as_array()
            .into_iter()
            .flatten()
            .find(|track| {
                track.get("type").and_then(Value::as_str) == Some("sub")
                    && (track.get("selected").and_then(Value::as_bool) == Some(true)
                        || sid
                            .is_some_and(|id| track.get("id").and_then(Value::as_u64) == Some(id)))
            })
            .map(|track| {
                let lang = track
                    .get("lang")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown");
                let title = track
                    .get("title")
                    .and_then(Value::as_str)
                    .unwrap_or("subtitle track");
                SubtitleTrackInfo {
                    id: track.get("id").and_then(Value::as_u64).unwrap_or_default(),
                    ff_index: track.get("ff-index").and_then(Value::as_u64),
                    external_path: track
                        .get("external-filename")
                        .and_then(Value::as_str)
                        .map(PathBuf::from),
                    label: format!("{lang} · {title}"),
                }
            }))
    }

    pub fn selected_audio_ff_index(&self) -> Result<Option<u64>> {
        let tracks = self.get_property("track-list")?;
        Ok(tracks
            .as_array()
            .into_iter()
            .flatten()
            .find(|track| {
                track.get("type").and_then(Value::as_str) == Some("audio")
                    && track.get("selected").and_then(Value::as_bool) == Some(true)
            })
            .and_then(|track| track.get("ff-index").and_then(Value::as_u64)))
    }

    pub fn select_subtitle_id(&self, id: u64) -> Result<()> {
        self.set_property("sid", json!(id))
    }

    pub fn seek(&self, seconds: f64, relative: bool) -> Result<()> {
        let flag = if relative { "relative" } else { "absolute" };
        self.command(&[json!("seek"), json!(seconds), json!(flag)])?;
        Ok(())
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
        // Quitting during startup must not wait for the connector or a
        // setup reply. The child is ours, so terminate it directly then.
        let connected = self
            .sock
            .try_lock()
            .is_ok_and(|link| matches!(*link, Link::Live(_)));
        let quit_sent = connected && self.background_commands.load(Ordering::SeqCst) == 0;
        if quit_sent {
            let _ = self.command(&[json!("quit")]);
        }
        let child = self
            .child
            .get_mut()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if !quit_sent {
            let _ = child.kill();
        }
        // A wedged mpv must not hang exit: give it a moment, then kill.
        let deadline = Instant::now() + Duration::from_secs(1);
        while matches!(child.try_wait(), Ok(None)) {
            if Instant::now() >= deadline {
                let _ = child.kill();
                let _ = child.wait();
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
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
        // Alternate in the same process and keep the best round of
        // each: sequential cross-run timings on this machine vary more
        // than the optimization being measured (see docs/testing.md).
        let mut seq = Duration::MAX;
        let mut batched = Duration::MAX;
        for _ in 0..5 {
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
            seq = seq.min(t0.elapsed() / N);

            let t0 = Instant::now();
            for _ in 0..N {
                mpv.set_subtitle_style(&cfg);
            }
            batched = batched.min(t0.elapsed() / N);
        }
        println!(
            "subtitle style (best of 5): sequential {:.0}us, batched {:.0}us",
            seq.as_secs_f64() * 1e6,
            batched.as_secs_f64() * 1e6
        );
    }

    /// End-to-end guard for the bulk queue path: mpv accepts the
    /// pipelined append-play commands in order, and the API returns one
    /// success per entry. Explicit because it spawns a real child.
    #[test]
    #[ignore]
    fn playlist_append_batch_reaches_mpv_and_preserves_entry_count() {
        let mpv = Mpv::spawn().expect("mpv on PATH");
        mpv.set_property("ao", json!("null"))
            .expect("silent output");
        let paths = [
            "av://lavfi:sine=frequency=440:duration=600".to_string(),
            "av://lavfi:sine=frequency=550:duration=600".to_string(),
            "av://lavfi:sine=frequency=660:duration=600".to_string(),
        ];
        let results = mpv.playlist_append_batch(&paths);
        assert_eq!(results.len(), paths.len());
        for result in results {
            result.expect("every generated playlist entry should be accepted");
        }
        let count = mpv
            .command(&[json!("get_property"), json!("playlist-count")])
            .expect("read playlist count");
        assert_eq!(count.as_u64(), Some(paths.len() as u64));
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
    fn cache_ahead_is_end_timestamp_minus_position_in_either_update_order() {
        let mut pb = Playback::default();
        pb.apply(PROP_TIME_POS, &json!(12.5));
        pb.apply(PROP_CACHE_AHEAD, &json!(42.5));
        assert_eq!(pb.cache_ahead_secs, Some(30.0));
        pb.apply(PROP_TIME_POS, &json!(20.0));
        assert_eq!(pb.cache_ahead_secs, Some(22.5));

        let mut reverse = Playback::default();
        reverse.apply(PROP_CACHE_AHEAD, &json!(42.5));
        reverse.apply(PROP_TIME_POS, &json!(12.5));
        assert_eq!(reverse.cache_ahead_secs, Some(30.0));
        reverse.apply(PROP_TIME_POS, &Value::Null);
        assert_eq!(reverse.cache_ahead_secs, None);
    }

    #[test]
    fn paused_for_cache_is_mpvs_elapsed_seconds_not_a_boolean() {
        let mut pb = Playback::default();
        pb.apply(PROP_PAUSED_FOR_CACHE, &json!(0.72));
        assert!(pb.paused_for_cache);
        pb.apply(PROP_PAUSED_FOR_CACHE, &json!(0.0));
        assert!(!pb.paused_for_cache);
        pb.apply(PROP_PAUSED_FOR_CACHE, &Value::Null);
        assert!(!pb.paused_for_cache);
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
                "af-metadata/{}/lavfi.astats.1.RMS_level",
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
    fn spectrum_metadata_reads_mono_channels_as_frequency_bands() {
        let mut pb = Playback::default();
        let mut metadata = serde_json::Map::new();
        for (channel, db) in [(3, "-12"), (4, "-48")] {
            metadata.insert(format!("lavfi.astats.{channel}.RMS_level"), json!(db));
        }
        assert!(pb.apply(PROP_SPECTRUM, &Value::Object(metadata)));
        let spectrum = pb.spectrum(2).expect("fresh spectrum");
        assert!(spectrum[0] > 0.9, "strong low band: {spectrum:?}");
        assert!(spectrum[1] < 0.25, "weak high band: {spectrum:?}");
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

#[cfg(test)]
mod reader_survives_garbage_tests {
    use super::*;

    #[test]
    fn a_non_utf8_line_does_not_end_the_reader_or_lose_the_next_reply() {
        let (mut mpv_side, wim_side) = UnixStream::pair().unwrap();
        let pending = Arc::new(Mutex::new(HashMap::new()));
        let (tx, rx) = mpsc::channel();
        pending.lock().unwrap().insert(7, Pending { reply_tx: tx });
        let (event_tx, _event_rx) = mpsc::channel();
        let reader = {
            let pending = pending.clone();
            thread::spawn(move || read_replies(wim_side, pending, event_tx))
        };
        mpv_side.write_all(b"\xff\xfe not json\n").unwrap();
        mpv_side
            .write_all(b"{\"request_id\":7,\"error\":\"success\",\"data\":42}\n")
            .unwrap();
        let reply = rx.recv_timeout(Duration::from_secs(2)).expect("reply");
        assert_eq!(reply.unwrap(), json!(42));
        drop(mpv_side);
        reader.join().unwrap();
    }

    #[test]
    fn eof_fails_every_waiter_immediately() {
        let (mpv_side, wim_side) = UnixStream::pair().unwrap();
        let pending = Arc::new(Mutex::new(HashMap::new()));
        let (tx, rx) = mpsc::channel();
        pending.lock().unwrap().insert(1, Pending { reply_tx: tx });
        let (event_tx, _event_rx) = mpsc::channel();
        drop(mpv_side);
        read_replies(wim_side, pending, event_tx);
        assert!(rx.recv_timeout(Duration::from_secs(1)).unwrap().is_err());
    }
}
