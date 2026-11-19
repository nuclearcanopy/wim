//! The UI-free half of shellswim's host: everything the prompt and a
//! one-shot `-c` share that does not touch a terminal, so the mobile
//! front-ends can drive the same session.
//!
//! The bin's `Host` holds one of these and keeps what is terminal- or
//! process-bound: the TUI, the TorBox wiring, the lyrics pane, the
//! instance lock and the mpv child. Playback goes through `&dyn Player`
//! arguments, never an mpv handle owned here.

use crate::catalog::{self, BackgroundSyncResult};
use crate::core::{
    self, BetweenTracks, RemoteSearchOutcome, ScrobbleState, StarFailed, SubtitleOutcome,
    TrackResolver,
};
use crate::download::DownloadEvent;
use crate::notify::{Notifications, Severity};
use crate::player::Player;
use crate::playqueue::PlayQueue;
use crate::search::SearchItem;
use crate::shellsong::{Console, LineKind, Program};
use crate::shellsong_host::{CorpusCandidates, SearchSettle};
use crate::swimsurface::SwimSurface;
use crate::{app, cache, config, favorites, library, lyrics, mpv, search};
use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Where a submitted program goes: straight to the active slot, or
/// behind whatever is running or already waiting. `Some` means "run it
/// now".
pub fn place_submitted_program(
    active: bool,
    core_waiting: bool,
    waiting: &mut VecDeque<Box<Program>>,
    program: Box<Program>,
) -> Option<Box<Program>> {
    if active || core_waiting || !waiting.is_empty() {
        waiting.push_back(program);
        None
    } else {
        Some(program)
    }
}

pub struct SwimSession {
    pub download_tx: mpsc::Sender<DownloadEvent>,
    pub download_rx: Receiver<DownloadEvent>,
    pub downloading: HashSet<String>,
    pub subtitle_tx: mpsc::Sender<SubtitleOutcome>,
    pub subtitle_rx: Receiver<SubtitleOutcome>,
    pub remote_search_tx: mpsc::Sender<core::RemoteSearchQuery>,
    pub remote_search_result_rx: Receiver<RemoteSearchOutcome>,
    pub lyrics_client: lyrics::Client,
    pub lyrics_loader: lyrics::Loader,
    /// Whether a prompt is on screen; one-shots print rather than
    /// open panes and echo only once a search settles.
    pub interactive_prompt: bool,
    pub track_resolver: TrackResolver,
    pub star_tx: mpsc::Sender<StarFailed>,
    pub star_rx: Receiver<StarFailed>,
    /// The query whose results have been printed, and the names
    /// already printed for it. Kept so a server answering after the
    /// settle window prints only its *new* matches rather than
    /// repeating the list or saying nothing at all.
    pub echoed: Option<(String, HashSet<String>)>,
    pub settle: SearchSettle,
    pub saved_queues: Vec<String>,
    pub bindings: crate::shellsong_host::Bindings,
    /// `.shg` names behind `run(...)`'s completion; a directory read,
    /// so snapshotted rather than done per keystroke.
    pub scripts: Vec<String>,
    pub notifications: Notifications,
    pub mpv_exit_reported: bool,
    pub seen_reply_timeouts: u64,
    pub last_reconcile: Instant,
    pub play_queue: PlayQueue,
    pub previous_track: Option<SearchItem>,
    pub scrobble: ScrobbleState,
    pub scrobble_failed_rx: Receiver<core::ScrobbleFailed>,
    pub favorites: favorites::Favorites,
    pub remote_starred: HashSet<String>,
    /// Not read from the player: that is an ipc round trip on the
    /// startup path for a value that is provably the ceiling at
    /// start. A fresh mpv is at full volume (`a_fresh_mpv_starts_at_
    /// full_volume` checks that against a real mpv).
    pub base_volume: f64,
    pub crossfade_enabled: bool,
    pub between_tracks: BetweenTracks,
    /// Per-identity settings the user has overridden by hand; see
    /// `core::track_started`. shellswim applied none of these before.
    pub overridden: crate::shellsong_host::Overridden,
    pub resume_due: Option<Duration>,
    pub undo: crate::undo::Recorder,
    pub fade_in_started_at: Option<Instant>,
    pub fade: Option<crate::shellsong_host::Ramp>,
    pub sleep_due: Option<Duration>,
    pub playback: mpv::Playback,
    pub cfg: config::Config,
    pub app: app::App,
    pub cache_index: Arc<Mutex<cache::CacheIndex>>,
    startup_cache: Option<Receiver<cache::CacheIndex>>,
    warming_corpus: Option<search::WarmingCorpus>,
    cached_entries: Option<search::CachedEntries>,
    pub scan_rx: Receiver<library::ScanBatch>,
    pub sync_rx: Receiver<BackgroundSyncResult>,
    pub servers_tx: mpsc::Sender<Vec<config::NavidromeConfig>>,
    pub server_reachable: HashMap<String, bool>,
    /// What the core hands over to the front-end (`SwimSurface`).
    pub swim: SwimSurface,
    pub console: Console,
    /// The live `let` bindings and the per-identity settings, both
    /// read once here and written back by the ops that change them.
    pub vars: crate::shellsong::Scope,
    pub program: Option<Box<Program>>,
    queued_programs: VecDeque<Box<Program>>,
    pub start: Instant,
}

impl SwimSession {
    /// Nothing here blocks on the library or the network: the scan and
    /// the catalog warm on their own threads.
    pub fn new(cfg: config::Config) -> SwimSession {
        let startup_cache = Some(cache::spawn_startup(
            cfg.offline_cache_max_bytes,
            Some(cfg.cache_eviction_days),
        ));
        // `rescan()` replaces this channel wholesale: the scan thread
        // signs off by dropping its sender, so a finished one cannot
        // be reused.
        let (scan_tx, scan_rx) = mpsc::channel::<library::ScanBatch>();
        library::spawn_scan(cfg.library_paths.clone(), scan_tx);
        // last session's catalog, off disk, so the first prompt can
        // already complete track names.
        let warming_corpus = Some(search::spawn_warm_from_cache(&cfg.navidrome_servers));
        let (sync_tx, sync_rx) = mpsc::channel::<BackgroundSyncResult>();
        let (servers_tx, servers_rx) = mpsc::channel();
        catalog::spawn_background_sync(cfg.navidrome_servers.clone(), sync_tx, servers_rx);

        let mut app = app::App::new(search::Corpus::new());
        app.scanning_library = true;
        app.syncing_servers = !cfg.navidrome_servers.is_empty();
        app.loading_cached_catalog = !cfg.navidrome_servers.is_empty();
        let restored_eq = crate::eq::restore(
            cfg.eq_preset.as_deref(),
            cfg.eq_gains,
            &cfg.eq_custom_presets,
        );
        app.eq_source = restored_eq.source;
        app.eq_custom = restored_eq.modified;
        app.eq_gains = restored_eq.gains;
        if let Some(crate::eq::Source::Builtin(index)) = restored_eq.source {
            app.eq_preset = index;
        }
        let (scrobble_failed_tx, scrobble_failed_rx) = mpsc::channel();
        let (download_tx, download_rx) = mpsc::channel();
        let (subtitle_tx, subtitle_rx) = mpsc::channel();
        let (remote_search_tx, remote_search_rx) = mpsc::channel();
        let (remote_search_result_tx, remote_search_result_rx) = mpsc::channel();
        core::spawn_background_search(remote_search_rx, remote_search_result_tx);
        let lyrics_client = lyrics::Client::with_providers(
            cfg.lyrics.providers.clone(),
            cfg.resolve_lyrics_api_keys(),
        );
        let (star_tx, star_rx) = mpsc::channel();
        SwimSession {
            download_tx,
            download_rx,
            downloading: HashSet::new(),
            subtitle_tx,
            subtitle_rx,
            remote_search_tx,
            remote_search_result_rx,
            lyrics_client,
            lyrics_loader: lyrics::Loader::new(),
            interactive_prompt: false,
            track_resolver: TrackResolver::new(),
            star_tx,
            star_rx,
            echoed: None,
            settle: SearchSettle::default(),
            saved_queues: Vec::new(),
            bindings: crate::shellsong_host::load_bindings(),
            scripts: crate::shellsong_host::script_names(),
            notifications: Notifications::default(),
            mpv_exit_reported: false,
            seen_reply_timeouts: mpv::reply_timeouts(),
            last_reconcile: Instant::now(),
            play_queue: PlayQueue::default(),
            previous_track: None,
            scrobble: ScrobbleState {
                failed_tx: Some(scrobble_failed_tx),
                ..ScrobbleState::default()
            },
            scrobble_failed_rx,
            favorites: favorites::load(),
            remote_starred: HashSet::new(),
            base_volume: crate::shellsong_host::VOLUME_CEILING,
            crossfade_enabled: cfg.crossfade_enabled,
            between_tracks: BetweenTracks::new(cfg.gap_seconds),
            overridden: crate::shellsong_host::Overridden::default(),
            resume_due: None,
            undo: crate::undo::Recorder::default(),
            fade_in_started_at: None,
            fade: None,
            sleep_due: None,
            playback: mpv::Playback::default(),
            cfg,
            app,
            cache_index: Arc::new(Mutex::new(cache::CacheIndex::default())),
            startup_cache,
            warming_corpus,
            cached_entries: None,
            scan_rx,
            sync_rx,
            servers_tx,
            server_reachable: HashMap::new(),
            swim: SwimSurface::default(),
            console: Console::new(),
            vars: crate::shellsong_host::load_vars(),
            program: None,
            queued_programs: VecDeque::new(),
            start: Instant::now(),
        }
    }

    pub fn poll_startup_cache(&mut self) -> anyhow::Result<bool> {
        let Some(rx) = &self.startup_cache else {
            return Ok(true);
        };
        match rx.try_recv() {
            Ok(index) => {
                *self
                    .cache_index
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner) = index;
                self.startup_cache = None;
                crate::trace_mark!("playback cache ready");
                Ok(true)
            }
            Err(TryRecvError::Empty) => Ok(false),
            Err(TryRecvError::Disconnected) => anyhow::bail!("playback cache startup failed"),
        }
    }

    pub fn wait_startup_cache(&mut self) -> anyhow::Result<()> {
        if let Some(rx) = self.startup_cache.take() {
            let index = rx
                .recv()
                .map_err(|_| anyhow::anyhow!("playback cache startup failed"))?;
            *self
                .cache_index
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = index;
        }
        Ok(())
    }

    /// One-shots have no prompt to show while a catalog warms, and must
    /// search the same cached library they did before startup was split.
    pub fn wait_startup_catalog(&mut self) {
        if let Some(warming) = self.warming_corpus.take() {
            let mut cached = warming.take().into_cached_entries();
            self.app
                .corpus
                .merge_cached_batch(&mut cached, usize::MAX, |client| {
                    self.cfg
                        .navidrome_servers
                        .iter()
                        .any(|nav| client.matches(nav))
                });
            self.app.resync_results();
        }
        self.app.loading_cached_catalog = false;
    }

    /// Warmed catalogs and local scans do not depend on mpv becoming ready.
    pub fn pump_library(&mut self) {
        if self
            .warming_corpus
            .as_ref()
            .is_some_and(|warm| warm.is_finished())
        {
            self.cached_entries = self
                .warming_corpus
                .take()
                .map(|warm| warm.take().into_cached_entries());
        }
        let mut changed = false;
        if let Some(cached) = &mut self.cached_entries {
            changed |= self.app.corpus.merge_cached_batch(
                cached,
                search::CACHED_ROWS_PER_FRAME,
                |client| {
                    self.cfg
                        .navidrome_servers
                        .iter()
                        .any(|nav| client.matches(nav))
                },
            ) > 0;
            if cached.is_empty() {
                self.cached_entries = None;
                self.app.loading_cached_catalog = false;
            }
        }
        for _ in 0..8 {
            match self.scan_rx.try_recv() {
                Ok(batch) => {
                    self.app.corpus.add_local_tracks(batch);
                    changed = true;
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    self.app.scanning_library = false;
                    break;
                }
            }
        }
        if changed {
            self.app.resync_results();
        }
    }

    pub fn has_cached_rows_pending(&self) -> bool {
        self.cached_entries.is_some()
    }

    /// What the config says is worth an alert at launch, in the order
    /// the prompt has always shown them; `proxy_unavailable` is the
    /// host's own finding about the media relay.
    pub fn push_startup_alerts(&mut self, proxy_unavailable: Option<String>) {
        let cfg = &self.cfg;
        if cfg.offline_download_format != "flac"
            && cfg.effective_offline_download_bitrate_kbps() <= 96
        {
            self.notifications.push(
                Severity::Alert,
                format!(
                    "low offline audio quality: downloads will use {} kbps",
                    cfg.effective_offline_download_bitrate_kbps()
                ),
            );
        }
        if let Some(warning) = proxy_unavailable {
            self.notifications.push(Severity::Alert, warning);
        }
        for server in &cfg.navidrome_servers {
            if let Some(warning) = crate::subsonic::insecure_transport_warning(&server.url) {
                self.notifications.push(Severity::Alert, warning);
            }
        }
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

    pub fn now(&self) -> Duration {
        self.start.elapsed()
    }

    /// Retain every submitted program. Shellsong programs may await a
    /// search or remote operation for several frames, so replacing the
    /// active slot would silently discard the user's earlier command.
    pub fn submit_program(&mut self, program: Box<Program>) {
        match place_submitted_program(
            self.program.is_some(),
            self.swim.queued.is_some(),
            &mut self.queued_programs,
            program,
        ) {
            Some(program) => self.program = Some(program),
            None => self.console.note(format!(
                "queued command ({} waiting)",
                self.queued_programs.len()
            )),
        }
    }

    pub fn has_program_work(&self) -> bool {
        self.program.is_some() || !self.queued_programs.is_empty() || self.swim.queued.is_some()
    }

    /// Drops everything waiting, for a one-shot that gave up.
    pub fn abandon_programs(&mut self) {
        self.program = None;
        self.queued_programs.clear();
        self.swim.queued = None;
    }

    /// Starts the next program if nothing is active: the one the core
    /// queued, else the oldest waiting submission.
    pub fn take_next_program(&mut self) {
        if self.program.is_none() {
            self.program = self
                .swim
                .queued
                .take()
                .or_else(|| self.queued_programs.pop_front());
        }
    }

    /// Parses a line against the live bindings and script directory,
    /// the same environment used by interactive submission.
    pub fn begin(&mut self, line: &str) {
        self.console.push(LineKind::Command, line.to_string());
        let read = |name: &str| crate::shellsong_host::read_script(name);
        let world = crate::shellsong::World {
            scope: &self.vars,
            read: Some(&read),
        };
        match Program::parse_in(line, &world) {
            Ok(p) => self.submit_program(Box::new(p)),
            Err(e) => self.console.fail(e.to_string()),
        }
    }

    /// Notes an mpv that timed out or exited, and reconciles the queue.
    pub fn pump_player_health(&mut self, player: &dyn Player) {
        if crate::mpv::reply_timeouts() != self.seen_reply_timeouts
            && !self.mpv_exit_reported
            && !player.background_active()
            && self.last_reconcile.elapsed() >= Duration::from_secs(5)
        {
            self.last_reconcile = Instant::now();
            if let Some(note) =
                crate::core::reconcile_queue_after_timeout(player, &mut self.play_queue)
            {
                self.notifications.push(Severity::Alert, note);
            }
            self.seen_reply_timeouts = crate::mpv::reply_timeouts();
        }
        if !self.mpv_exit_reported {
            if let Some(status) = player.exited() {
                self.mpv_exit_reported = true;
                self.notifications.push(
                    Severity::Error,
                    format!(
                        "mpv exited ({status}); playback is unavailable until shellswim restarts"
                    ),
                );
            }
        }
    }

    /// Reports star and scrobble failures that came back from workers.
    pub fn pump_failures(&mut self) {
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
        while let Ok(failed) = self.scrobble_failed_rx.try_recv() {
            self.notifications.push(
                Severity::Error,
                format!("ListenBrainz {} failed: {}", failed.kind, failed.error),
            );
        }
    }

    /// Applies catalog syncs, remote searches, downloads and subtitles
    /// that finished since the last call.
    pub fn pump_replies(&mut self, player: &dyn Player, width: usize) {
        while let Ok(sync) = self.sync_rx.try_recv() {
            self.app.syncing_servers = false;
            match sync.result {
                Ok((catalog, starred)) => {
                    if let Some(warning) = catalog::merge_catalog_into_app(&mut self.app, catalog) {
                        self.notifications.push(Severity::Alert, warning);
                    }
                    self.remote_starred.extend(starred);
                    if self.server_reachable.insert(sync.url.clone(), true) == Some(false) {
                        self.notifications
                            .push(Severity::Info, format!("{} is back online", sync.url));
                    }
                }
                Err(message) => match self.server_reachable.insert(sync.url.clone(), false) {
                    None => self
                        .notifications
                        .push(Severity::Error, format!("{}: {message}", sync.url)),
                    Some(true) => self.notifications.push(
                        Severity::Alert,
                        format!("{} went offline: {message}", sync.url),
                    ),
                    Some(false) => {}
                },
            }
        }
        let mut answered = false;
        while let Ok(outcome) = self.remote_search_result_rx.try_recv() {
            self.app
                .corpus
                .add_subsonic_search_result(outcome.result, &outcome.client);
            self.app.resync_results();
            // an answer is in, so the results for whatever was asked
            // are as complete as a program should wait for.
            self.settle.answered(&outcome.query);
            answered = true;
        }
        // a server that answered *after* the settle window already
        // missed the echo, so its matches are printed as they land
        // rather than silently enriching a list that is on screen and
        // now wrong. only for the query still being shown: anything
        // else is an answer to a question nobody is looking at.
        if answered {
            let avail = width;
            if let Some((query, shown)) = self.echoed.as_mut() {
                if *query == self.app.query {
                    let lines = core::echo_lines(self.app.results(), shown, avail);
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
            if !core::subtitle_belongs_to_playing(&sub, self.play_queue.current()) {
                continue;
            }
            match sub {
                core::SubtitleOutcome::Ready(ready) => {
                    let _ = player.sub_add(&ready.path);
                }
                // said rather than swallowed: only "no subtitles for
                // this film" is silent now, so a refused key does not
                // look like one.
                core::SubtitleOutcome::Failed { why, .. } => self.console.fail(why),
            }
        }
    }

    /// Time-driven playback: the between-track gap, fades, the sleep
    /// timer, resume, and the scrobble threshold.
    pub fn pump_timers(&mut self, player: &dyn Player) {
        // the between-track gap, same mechanism as wim's loop: mpv
        // holds at each file's end under `keep-open` and pauses
        // itself, and this waits the gap out before advancing.
        self.between_tracks.step(
            player,
            self.playback.eof_reached,
            self.play_queue.upcoming_len() > 0,
            Instant::now(),
        );

        // Keep time-based playback behavior live even while the prompt
        // is idle or a command is waiting for network results.
        let now = self.start.elapsed();
        if let Some(ramp) = self.fade {
            let _ = player.set_volume_nowait(ramp.level_at(now));
            if ramp.finished(now) {
                self.fade = None;
            }
        }
        if self.crossfade_enabled {
            if let Some(started) = self.fade_in_started_at {
                let span = config::crossfade_seconds(self.cfg.crossfade_seconds);
                let t = (started.elapsed().as_secs_f64() / span).clamp(0.0, 1.0);
                let _ = player.set_volume_nowait(self.base_volume * t);
                if t >= 1.0 {
                    self.fade_in_started_at = None;
                }
            } else if let (Some(elapsed), Some(duration)) = (
                self.playback.elapsed_secs,
                core::playing_duration(self.play_queue.current(), self.playback.duration_secs),
            ) {
                let span = config::crossfade_seconds(self.cfg.crossfade_seconds);
                let remaining = duration - elapsed;
                if remaining <= span {
                    let _ = player
                        .set_volume_nowait(self.base_volume * (remaining / span).clamp(0.0, 1.0));
                }
            }
        }
        if self.resume_due.is_some_and(|due| now >= due) {
            self.resume_due = None;
            let _ = player.set_pause(false);
            self.console.note("resumed".to_string());
        }
        if let Some(due) = self.sleep_due {
            let fade =
                Duration::from_secs_f64(config::crossfade_seconds(self.cfg.crossfade_seconds));
            match crate::shellsong_host::sleep_step(due, fade, self.base_volume, now) {
                crate::shellsong_host::SleepStep::Waiting => {}
                crate::shellsong_host::SleepStep::Fading(level) => {
                    let _ = player.set_volume_nowait(level);
                }
                crate::shellsong_host::SleepStep::Expired => {
                    self.sleep_due = None;
                    if !self.playback.paused {
                        let _ = player.toggle_pause();
                    }
                    let _ = player.set_volume(self.base_volume);
                    self.notifications
                        .push(Severity::Info, "sleep timer: paused");
                    self.console.note("slept".to_string());
                }
            }
        }
        if !self.scrobble.scrobbled {
            if let (Some(item), Some(elapsed), Some(duration)) = (
                self.play_queue.current(),
                self.playback.elapsed_secs,
                self.playback.duration_secs,
            ) {
                if crate::listenbrainz::should_scrobble(&self.cfg.scrobble, elapsed, duration) {
                    let listened_at = self
                        .scrobble
                        .started_unix
                        .unwrap_or_else(crate::listenbrainz::now_unix);
                    core::fire_scrobble(
                        &self.cfg,
                        item,
                        listened_at,
                        self.scrobble.failed_tx.as_ref(),
                    );
                    self.scrobble.scrobbled = true;
                }
            }
        }
    }
}

#[cfg(test)]
mod program_queue_tests {
    use super::*;
    use crate::shellsong::Op;

    #[test]
    fn a_submission_waits_behind_the_active_program_without_replacing_it() {
        let mut waiting = VecDeque::new();
        let submitted = Box::new(Program::of(vec![Op::ClearConsole]));
        let submitted_ptr = (&*submitted) as *const Program;
        let active = place_submitted_program(true, false, &mut waiting, submitted);

        assert!(active.is_none());
        assert_eq!(waiting.len(), 1);
        assert_eq!(
            (&*waiting.pop_front().unwrap()) as *const Program,
            submitted_ptr
        );
    }

    #[test]
    fn a_submission_waits_behind_a_track_binding_even_when_idle() {
        let mut waiting = VecDeque::new();
        let submitted = Box::new(Program::of(vec![Op::ClearConsole]));

        assert!(place_submitted_program(false, true, &mut waiting, submitted).is_none());
        assert_eq!(waiting.len(), 1);
    }

    #[test]
    fn later_submissions_keep_fifo_order_behind_core_work() {
        let mut waiting = VecDeque::new();
        let first = Box::new(Program::of(vec![Op::ClearConsole]));
        let second = Box::new(Program::of(vec![Op::ClearQueue]));
        let first_ptr = (&*first) as *const Program;
        let second_ptr = (&*second) as *const Program;

        assert!(place_submitted_program(false, true, &mut waiting, first).is_none());
        assert!(place_submitted_program(false, false, &mut waiting, second).is_none());
        assert_eq!(waiting.len(), 2);
        assert_eq!(
            (&*waiting.pop_front().unwrap()) as *const Program,
            first_ptr
        );
        assert_eq!(
            (&*waiting.pop_front().unwrap()) as *const Program,
            second_ptr
        );
    }
}
