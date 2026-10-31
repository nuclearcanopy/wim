//! Everything shellswim owns, so the prompt and a one-shot `-c` share
//! one startup and one step loop rather than two that can drift.
//!
//! The fields are owned here (wim keeps the same set as `main`
//! locals); `CoreCtx` is still built per use, because the borrow of
//! the core and the borrow of the surface have to stay disjoint.

use std::collections::HashSet;
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// The one `CoreCtx` literal, as a macro rather than a method: it
/// borrows most of `Host` plus two locals (`now`, `now_playing`),
/// and a method cannot return a struct borrowing locals it made.
/// Same reason `main.rs` keeps its own `core_ctx!`.
macro_rules! core_ctx {
    ($h:expr, $now:expr, $np:expr) => {
        CoreCtx {
            cfg: &$h.cfg,
            player: &$h.player,
            app: &mut $h.app,
            notifications: &mut $h.notifications,
            queue: &mut $h.play_queue,
            previous_track: &mut $h.previous_track,
            cache: &$h.cache_index,
            download_tx: &$h.download_tx,
            downloading: &mut $h.downloading,
            scrobble: &mut $h.scrobble,
            subtitle_tx: &$h.subtitle_tx,
            console: &mut $h.console,
            now: $now,
            now_playing: &$np,
            favorites: &mut $h.favorites,
            remote_starred: &mut $h.remote_starred,
            lyrics_client: &$h.lyrics_client,
            lyrics_loader: &mut $h.lyrics_loader,
            star_tx: &$h.star_tx,
            track_resolver: &mut $h.track_resolver,
            base_volume: &mut $h.base_volume,
            crossfade_enabled: &mut $h.crossfade_enabled,
            undo: &mut $h.undo,
            gap_secs: &mut $h.gap_secs,
            between_tracks: &mut $h.between_tracks,
            fade_in_started_at: &mut $h.fade_in_started_at,
            fade: &mut $h.fade,
            sleep_due: &mut $h.sleep_due,
            resume_due: &mut $h.resume_due,
            scan_rx: &mut $h.scan_rx,
            remote_search_tx: &$h.remote_search_tx,
            settle: &mut $h.settle,
            saved_queues: &mut $h.saved_queues,
            vars: &mut $h.vars,
            bindings: &mut $h.bindings,
        }
    };
}

use wim::core::{
    self, drive, BetweenTracks, CoreCtx, Drive, GapAction, Intent, RemoteSearchOutcome,
    ScrobbleState, StarFailed, SubtitleReady, TrackResolver,
};
use wim::download::DownloadEvent;
use wim::notify::Notifications;
use wim::playqueue::{PlayQueue, PosChange};
use wim::search::SearchItem;
use wim::shellsong::{Console, LineKind, Program};
use wim::shellsong_host::{CorpusCandidates, Ramp, SearchSettle};
use wim::ui::results_list::label;
use wim::{app, cache, config, favorites, library, lyrics, mpv, search, trace};

use crate::surface::SwimSurface;

/// How long a one-shot waits for a command that is still resolving a
/// name against a server. The language times its own await out at 8s;
/// this is the backstop for a program that never gets that far.
const ONESHOT_CAP: Duration = Duration::from_secs(20);

/// A one-shot steps this often, which is slow enough to cost nothing
/// and fast enough that a local `play()` finishes in one or two.
const ONESHOT_TICK: Duration = Duration::from_millis(10);

pub struct Host {
    pub cfg: config::Config,
    pub player: mpv::Mpv,
    pub app: app::App,
    pub notifications: Notifications,
    pub play_queue: PlayQueue,
    pub previous_track: Option<SearchItem>,
    pub cache_index: Arc<Mutex<cache::CacheIndex>>,
    pub download_tx: Sender<DownloadEvent>,
    pub download_rx: Receiver<DownloadEvent>,
    pub downloading: HashSet<String>,
    pub subtitle_tx: Sender<SubtitleReady>,
    pub subtitle_rx: Receiver<SubtitleReady>,
    pub remote_search_tx: Sender<core::RemoteSearchQuery>,
    pub remote_search_result_rx: Receiver<RemoteSearchOutcome>,
    pub scan_rx: Receiver<library::ScanBatch>,
    pub scrobble: ScrobbleState,
    pub favorites: favorites::Favorites,
    pub remote_starred: HashSet<String>,
    pub lyrics_client: lyrics::Client,
    pub lyrics_loader: lyrics::Loader,
    pub track_resolver: TrackResolver,
    pub star_tx: mpsc::Sender<StarFailed>,
    pub star_rx: mpsc::Receiver<StarFailed>,
    pub base_volume: f64,
    pub crossfade_enabled: bool,
    /// Live, from `config::gap_seconds`; `gap()` changes it for the
    /// session.
    pub gap_secs: f64,
    pub between_tracks: BetweenTracks,
    pub resume_due: Option<Duration>,
    /// The query whose results have been printed, and the names
    /// already printed for it. Kept so a server answering after the
    /// settle window prints only its *new* matches rather than
    /// repeating the list or saying nothing at all.
    echoed: Option<(String, HashSet<String>)>,
    pub undo: wim::undo::Recorder,
    /// Last `keep-open` written, so evaluating it every pump costs no
    /// ipc until the answer changes.
    keep_open_held: Option<bool>,
    pub fade_in_started_at: Option<Instant>,
    pub fade: Option<Ramp>,
    pub sleep_due: Option<Duration>,
    pub settle: SearchSettle,
    pub saved_queues: Vec<String>,
    /// The live `let` bindings and the per-identity settings, both
    /// read once here and written back by the ops that change them.
    pub vars: wim::shellsong::Scope,
    pub bindings: wim::shellsong_host::Bindings,
    /// `.shg` names behind `run(...)`'s completion; a directory read,
    /// so snapshotted rather than done per keystroke.
    pub scripts: Vec<String>,
    pub playback: mpv::Playback,
    pub console: Console,
    pub program: Option<Box<Program>>,
    pub swim: SwimSurface,
    pub start: Instant,
}

impl Host {
    /// Nothing here blocks on the library, the network or the
    /// terminal: the scan and the catalog warm on their own threads
    /// and the prompt is usable before either answers.
    pub fn start() -> anyhow::Result<Host> {
        let cfg = config::load_or_init()?;
        trace::mark("config loaded");

        // `rescan()` replaces this channel wholesale: the scan thread
        // signs off by dropping its sender, so a finished one cannot
        // be reused.
        let (scan_tx, scan_rx) = mpsc::channel::<library::ScanBatch>();
        library::spawn_scan(cfg.library_paths.clone(), scan_tx);
        // last session's catalog, off disk, so the first prompt can
        // already complete track names.
        let warming = search::spawn_warm_from_cache(&cfg.navidrome_servers);

        let player = mpv::Mpv::spawn()?;
        trace::mark("mpv spawned");
        player.set_replaygain("track")?;
        player.observe_playback()?;

        let mut app = app::App::new(warming.take());
        app.scanning_library = true;
        trace::mark("corpus warmed from cache");

        let (download_tx, download_rx) = mpsc::channel();
        let (subtitle_tx, subtitle_rx) = mpsc::channel();
        let (remote_search_tx, remote_search_rx) = mpsc::channel();
        let (remote_search_result_tx, remote_search_result_rx) = mpsc::channel();
        core::spawn_background_search(remote_search_rx, remote_search_result_tx);

        let base_volume = core::current_volume(&player).unwrap_or(100.0);
        let crossfade_enabled = cfg.crossfade_enabled;
        let gap_secs = cfg.gap_seconds.max(0.0);
        let (star_tx, star_rx) = mpsc::channel();
        Ok(Host {
            cfg,
            player,
            app,
            notifications: Notifications::default(),
            play_queue: PlayQueue::default(),
            previous_track: None,
            cache_index: Arc::new(Mutex::new(cache::CacheIndex::load())),
            download_tx,
            download_rx,
            downloading: HashSet::new(),
            subtitle_tx,
            subtitle_rx,
            remote_search_tx,
            remote_search_result_rx,
            scan_rx,
            scrobble: ScrobbleState::default(),
            favorites: favorites::load(),
            remote_starred: HashSet::new(),
            lyrics_client: lyrics::Client::new(),
            lyrics_loader: lyrics::Loader::new(),
            track_resolver: TrackResolver::new(),
            star_tx,
            star_rx,
            base_volume,
            crossfade_enabled,
            gap_secs,
            between_tracks: BetweenTracks::new(),
            resume_due: None,
            echoed: None,
            undo: wim::undo::Recorder::default(),
            keep_open_held: None,
            fade_in_started_at: None,
            fade: None,
            sleep_due: None,
            settle: SearchSettle::default(),
            saved_queues: Vec::new(),
            vars: wim::shellsong_host::load_vars(),
            bindings: wim::shellsong_host::load_bindings(),
            scripts: wim::shellsong_host::script_names(),
            playback: mpv::Playback::default(),
            console: Console::empty(),
            program: None,
            swim: SwimSurface::default(),
            start: Instant::now(),
        })
    }

    /// The terminal's width, asked for when something is about to be
    /// printed rather than carried on the console: the console is
    /// handed a rect at draw time and the host never sees it.
    fn console_width(&self) -> usize {
        crossterm::terminal::size()
            .map(|(w, _)| usize::from(w).saturating_sub(4))
            .unwrap_or(76)
    }

    pub fn now(&self) -> Duration {
        self.start.elapsed()
    }

    pub fn now_playing(&self) -> Option<SearchItem> {
        self.play_queue.current().cloned()
    }

    /// Local names the prompt completes against; never touches the
    /// network (see `shellsong::Candidates`).
    pub fn candidates(&self) -> CorpusCandidates<'_> {
        CorpusCandidates {
            corpus: &self.app.corpus,
            saved: &self.saved_queues,
            eq_presets: core::eq_preset_names(&self.cfg.eq_custom_presets),
            vars: &self.vars,
            scripts: &self.scripts,
        }
    }

    /// Everything the background threads finished since the last call.
    /// Each one is a `try_recv`, so this is the whole reason nothing
    /// in shellswim blocks.
    pub fn pump(&mut self) {
        // a queue batch resolved; the resolver already dropped any from
        // a superseded request, which here would append to the queue
        // rather than merely drawing something stale.
        while let Some((into, label, tracks)) = self.track_resolver.poll() {
            let now = self.start.elapsed();
            let now_playing = self.play_queue.current().cloned();
            let mut core = core_ctx!(self, now, now_playing);
            match tracks {
                Ok(tracks) if tracks.is_empty() => core
                    .console
                    .note(format!("nothing to {} in {label}", into.verb())),
                Ok(tracks) => match into {
                    Intent::Back => core::append_resolved(&tracks, &mut core),
                    Intent::Front => match core::front_resolved(&tracks, &mut core) {
                        Ok(n) => core.console.note(format!("queued {n} tracks next")),
                        Err(why) => core.console.note(why),
                    },
                    Intent::Download => {
                        let done = core::download_resolved(&tracks, &mut core);
                        core.console.note(done.status);
                    }
                },
                Err(why) => core.console.note(why),
            }
        }

        // the server refused a star, so the optimistic flip goes back.
        while let Ok(failed) = self.star_rx.try_recv() {
            if failed.wanted {
                self.remote_starred.remove(&failed.id);
            } else {
                self.remote_starred.insert(failed.id.clone());
            }
            let verb = if failed.wanted { "star" } else { "unstar" };
            self.console
                .note(format!("couldn't {verb}: {}", failed.error));
        }

        // a lyrics fetch landed. the loader has already dropped any
        // reply for a track the user moved past, so whatever arrives
        // belongs to the request still standing.
        if let Some((track, result)) = self.lyrics_loader.poll() {
            match result {
                Ok(Some(l)) => {
                    let text = wim::core::lyrics_text(&l);
                    let count = text.lines().count();
                    for line in text.lines() {
                        self.console.note(line.to_string());
                    }
                    self.console.note(format!("{count} lines for {track}"));
                }
                Ok(None) => self.console.note(format!("no lyrics for {track}")),
                Err(e) => self.console.note(format!("lyrics lookup failed: {e}")),
            }
        }

        let mut drained = false;
        while let Ok(ev) = self.player.events().try_recv() {
            let mpv::Event::PropertyChange { name, value } = ev else {
                continue;
            };
            if self.playback.apply(&name, &value) {
                continue;
            }
            if name == mpv::PROP_PLAYLIST_POS {
                if let PosChange::Advanced(_) =
                    self.play_queue.set_pos(value.as_i64().unwrap_or(-1))
                {
                    if let Some(track) = self.play_queue.current().cloned() {
                        self.console.note(format!("now playing: {}", label(&track)));
                    }
                }
            } else if name == mpv::PROP_IDLE_ACTIVE {
                // this was not handled at all, so a queue that ran out
                // left the bar showing a track that had finished and
                // `now_playing` never cleared.
                drained = value.as_bool().unwrap_or(false);
            }
        }
        if drained {
            // the same decision wim makes, from the same function.
            match core::drained(
                &self.player,
                &self.cache_index,
                &mut self.play_queue,
                self.app.loop_mode,
            ) {
                core::Drained::Stale => {}
                core::Drained::Cleared => self.console.note("queue finished".to_string()),
                core::Drained::Failed(why) => self.console.fail(why),
                core::Drained::Restarted(track) => {
                    // the hooks a restart owes, as far as shellswim
                    // has any: wim additionally re-applies
                    // per-identity settings and the crossfade ramp,
                    // neither of which shellswim does on an ordinary
                    // advance either. see ROADMAP.
                    let now = self.start.elapsed();
                    let np = Some(track.clone());
                    let core = core_ctx!(self, now, np);
                    core::fire_subtitle_fetch(core.cfg, core.subtitle_tx, &track);
                    core::start_tracking_playback(core.cfg, &track, core.scrobble);
                    self.console
                        .note(format!("looped back to: {}", label(&track)));
                }
            }
        }

        // a settled search prints what it found. shellswim has no
        // sidebar, so `search(x)` used to answer with a count and
        // nothing else, which is no answer at all.
        if self.swim.echo_results && self.settle.settled(self.start.elapsed()) {
            self.swim.echo_results = false;
            let mut shown = HashSet::new();
            let lines = core::echo_lines(&self.app.results, &mut shown, self.console_width());
            if lines.is_empty() {
                self.console.note("no matches".to_string());
            }
            for line in lines {
                self.console.note(line);
            }
            self.echoed = Some((self.app.query.clone(), shown));
        }

        // `pause(30s)` letting go.
        if self
            .resume_due
            .is_some_and(|due| self.start.elapsed() >= due)
        {
            self.resume_due = None;
            let _ = self.player.set_pause(false);
            self.console.note("resumed".to_string());
        }

        // the between-track gap, same mechanism as wim's loop: mpv
        // holds at each file's end under `keep-open` and pauses
        // itself, and this waits the gap out before advancing.
        let gap = Duration::from_secs_f64(self.gap_secs.max(0.0));
        let more_to_play = self.play_queue.upcoming_len() > 0;
        let want_hold = !gap.is_zero() && more_to_play;
        if self.keep_open_held != Some(want_hold) {
            let _ = self.player.set_keep_open(want_hold);
            self.keep_open_held = Some(want_hold);
        }
        if self
            .between_tracks
            .tick(self.playback.eof_reached, gap, more_to_play, Instant::now())
            == Some(GapAction::Release)
        {
            let _ = self.player.playlist_next();
            // mpv stays paused across `playlist-next` once it has
            // held; see `mpv::PROP_EOF_REACHED`.
            let _ = self.player.set_pause(false);
        }

        let mut scanned = 0;
        loop {
            match self.scan_rx.try_recv() {
                Ok(batch) => {
                    self.app.corpus.add_local_tracks(batch);
                    scanned += 1;
                }
                Err(TryRecvError::Empty) => break,
                // the scan thread signing off is what clears
                // "scanning"; it drops its sender when it is done.
                Err(TryRecvError::Disconnected) => {
                    self.app.scanning_library = false;
                    break;
                }
            }
        }
        if scanned > 0 {
            self.app.resync_results();
            self.settle.answered();
        }
        let mut answered = false;
        while let Ok(outcome) = self.remote_search_result_rx.try_recv() {
            self.app
                .corpus
                .add_subsonic_search_result(outcome.result, &outcome.client);
            self.app.resync_results();
            // an answer is in, so the results for whatever was asked
            // are as complete as a program should wait for.
            self.settle.answered();
            answered = true;
        }
        // a server that answered *after* the settle window already
        // missed the echo, so its matches are printed as they land
        // rather than silently enriching a list that is on screen and
        // now wrong. only for the query still being shown: anything
        // else is an answer to a question nobody is looking at.
        if answered {
            let avail = self.console_width();
            if let Some((query, shown)) = self.echoed.as_mut() {
                if *query == self.app.query {
                    let lines = core::echo_lines(&self.app.results, shown, avail);
                    if !lines.is_empty() {
                        self.console.note("...and from the servers:".to_string());
                        for line in lines {
                            self.console.note(line);
                        }
                    }
                }
            }
        }
        while let Ok(event) = self.download_rx.try_recv() {
            match event {
                DownloadEvent::Progress { .. } => {}
                DownloadEvent::Done { id, label } => {
                    self.downloading.remove(&id);
                    self.console.note(format!("downloaded: {label}"));
                }
                DownloadEvent::Failed { id, label, error } => {
                    self.downloading.remove(&id);
                    self.console
                        .fail(format!("download failed for {label}: {error}"));
                }
            }
        }
        while let Ok(sub) = self.subtitle_rx.try_recv() {
            let _ = self.player.sub_add(&sub.path);
        }
        // notifications are wim's toast channel; an overlay here would
        // cover the thing it is talking about, so the text goes into
        // the scrollback, which is the only output shellswim has.
        for note in self.notifications.drain_text() {
            self.console.note(note);
        }
    }

    /// One frame of the running program. The two borrows are built
    /// from disjoint fields of `self`, which is the whole reason
    /// `CoreCtx` and `Surface` are separate arguments.
    pub fn step(&mut self) -> Drive {
        let Some(program) = self.program.as_mut() else {
            return Drive::Done;
        };
        let now = self.start.elapsed();
        let now_playing = self.play_queue.current().cloned();
        let mut surf = self.swim.borrowed();
        let mut core = core_ctx!(self, now, now_playing);
        let outcome = drive(program, &mut core, &mut surf, |c| c.settle.settled(c.now));
        if outcome != Drive::Await {
            self.program = None;
        }
        outcome
    }

    /// Parses and starts a line, exactly as submitting it at the
    /// prompt does, so nothing scripted can behave differently from
    /// something typed.
    pub fn begin(&mut self, line: &str) {
        self.console.push(LineKind::Command, line.to_string());
        match Program::parse(line) {
            Ok(p) => self.program = Some(Box::new(p)),
            Err(e) => self.console.fail(e.to_string()),
        }
    }

    /// Runs lines to completion for a one-shot. Returns whether
    /// anything failed, so `-c` can exit non-zero and be usable in a
    /// script that checks.
    pub fn run_lines(&mut self, lines: &[String]) -> bool {
        let deadline = Instant::now() + ONESHOT_CAP;
        for line in lines {
            self.begin(line);
            while self.program.is_some() {
                self.pump();
                if self.step() == Drive::Quit {
                    self.program = None;
                    break;
                }
                if self.program.is_some() {
                    if Instant::now() >= deadline {
                        self.console.fail(format!("gave up waiting on '{line}'"));
                        self.program = None;
                        break;
                    }
                    std::thread::sleep(ONESHOT_TICK);
                }
            }
        }
        // one last drain, so a download or a track change that landed
        // on the way out still gets reported.
        self.pump();
        self.console.lines.iter().any(|l| l.kind == LineKind::Error)
    }
}
