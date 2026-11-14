//! Everything shellswim owns, so the prompt and a one-shot `-c` share
//! one startup and one step loop rather than two that can drift.
//!
//! The fields are owned here (wim keeps the same set as `main`
//! locals); `CoreCtx` is still built per use, because the borrow of
//! the core and the borrow of the surface have to stay disjoint.

use std::collections::{HashMap, HashSet, VecDeque};
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
            muted: $h.playback.muted,
            crossfade_enabled: &mut $h.crossfade_enabled,
            overridden: &mut $h.overridden,
            undo: &mut $h.undo,
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

use wim::catalog::{self, BackgroundSyncResult};
use wim::core::{
    self, BetweenTracks, CoreCtx, Drive, Intent, RemoteSearchOutcome, ScrobbleState, StarFailed,
    SubtitleOutcome, TrackResolver,
};
use wim::download::DownloadEvent;
use wim::notify::{Notifications, Severity};
use wim::playqueue::{PlayQueue, PosChange};
use wim::search::SearchItem;
use wim::shellsong::{Console, LineKind, Op, Program};
use wim::shellsong_host::{CorpusCandidates, Ramp, SearchSettle};
use wim::ui::results_list::label;
use wim::{app, cache, config, favorites, library, lyrics, mpv, search};

use crate::surface::SwimSurface;

/// How long a one-shot waits for a command that is still resolving a
/// name against a server. The language times its own await out at 8s;
/// this is the backstop for a program that never gets that far.
const ONESHOT_CAP: Duration = Duration::from_secs(30);

/// A one-shot steps this often, which is slow enough to cost nothing
/// and fast enough that a local `play()` finishes in one or two.
const ONESHOT_TICK: Duration = Duration::from_millis(10);

enum TorBoxPayload {
    Lines(Vec<String>),
    Item(SearchItem, bool),
}

struct TorBoxReply {
    label: String,
    result: Result<TorBoxPayload, String>,
}

fn is_torbox_op(op: &Op) -> bool {
    matches!(
        op,
        Op::TorBoxSearch(_)
            | Op::TorBoxStatus
            | Op::TorBoxList(_)
            | Op::TorBoxAdd { .. }
            | Op::TorBoxPlay { .. }
            | Op::TorBoxQueue { .. }
            | Op::TorBoxControl { .. }
    )
}

fn run_torbox_op(cfg: &config::Config, op: Op) -> Result<TorBoxPayload, String> {
    use wim::torbox::{Client, Service};
    let queue = matches!(&op, Op::TorBoxQueue { .. });
    let client = Client::from_config(cfg).map_err(|_| {
        "TorBox is not configured; set torbox_token_file or WIM_TORBOX_TOKEN_FILE".to_string()
    })?;
    match op {
        Op::TorBoxStatus => client
            .account_items(Service::Torrent, 0, 1)
            .map(|_| TorBoxPayload::Lines(vec!["TorBox connection verified".into()]))
            .map_err(|_| {
                "TorBox connection failed; check token file, network/VPN access, and account".into()
            }),
        Op::TorBoxSearch(query) => client
            .search_library(&query)
            .map(|search| {
                if search.matches.is_empty() {
                    return TorBoxPayload::Lines(vec!["no TorBox library matches".into()]);
                }
                let mut lines: Vec<String> = search
                    .matches
                    .iter()
                    .take(100)
                    .map(|item| {
                        format!(
                            "{} · {} · id={} · {} files",
                            wim::torbox_workspace::safe_display_name(
                                &item.name,
                                cfg.watch_reveal_spoilers,
                            ),
                            item.status,
                            item.id,
                            item.files.len()
                        )
                    })
                    .collect();
                if !search.failed_services.is_empty() {
                    let failed: Vec<&str> = search
                        .failed_services
                        .iter()
                        .map(|service| service.as_str())
                        .collect();
                    lines.push(format!(
                        "note: these services could not be listed; results may be incomplete: {}",
                        failed.join(", ")
                    ));
                }
                TorBoxPayload::Lines(lines)
            })
            .map_err(|_| {
                "TorBox library search failed; check token, network/VPN access, and account".into()
            }),
        Op::TorBoxList(service) => client
            .all_account_items(service)
            .map(|items| {
                let mut lines = Vec::new();
                for item in &items {
                    lines.push(format!(
                        "{} · {} · id={} · {} files",
                        wim::torbox_workspace::safe_display_name(
                            &item.name,
                            cfg.watch_reveal_spoilers,
                        ),
                        item.status,
                        item.id,
                        item.files.len()
                    ));
                    for file in item
                        .files
                        .iter()
                        .filter(|file| wim::torbox::is_playable_file_name(&file.name))
                    {
                        lines.push(format!(
                            "  {} · file_id={} · {} bytes",
                            wim::torbox_workspace::safe_display_name(
                                &file.name,
                                cfg.watch_reveal_spoilers,
                            ),
                            file.id,
                            file.size
                        ));
                    }
                }
                if lines.is_empty() {
                    lines.push(format!("no {} account items", service.as_str()));
                }
                lines.push(format!(
                    "{} {} account items",
                    items.len(),
                    service.as_str()
                ));
                TorBoxPayload::Lines(lines)
            })
            .map_err(|_| {
                "TorBox account listing failed; check token, network/VPN access, and account".into()
            }),
        Op::TorBoxAdd { service, link } => {
            let response = match service {
                Service::Torrent => client.add_magnet(&link),
                Service::Usenet => client.add_usenet_link(&link),
                Service::WebDownload => client.add_web_download(&link),
            }
            .map_err(|_| format!("TorBox could not add this {} link", service.as_str()))?;
            response
                .ensure_success()
                .map_err(|_| format!("TorBox rejected this {} link", service.as_str()))?;
            Ok(TorBoxPayload::Lines(vec![format!(
                "added to TorBox {}; refresh with torbox_list({})",
                service.as_str(),
                match service {
                    Service::WebDownload => "web",
                    other => other.as_str(),
                }
            )]))
        }
        Op::TorBoxPlay {
            service,
            id,
            file_id,
        }
        | Op::TorBoxQueue {
            service,
            id,
            file_id,
        } => {
            let account_item = client
                .find_account_item(service, id)
                .map_err(|_| {
                    "TorBox account lookup failed; check token, network/VPN access, and account"
                        .to_string()
                })?
                .ok_or_else(|| format!("TorBox item {id} was not found"))?;
            let client = std::sync::Arc::new(client);
            let item = account_item
                .playable_items(client)
                .into_iter()
                .find(|item| matches!(item, SearchItem::TorBoxFile { file_id: found, .. } if *found == file_id))
                .ok_or_else(|| format!("playable TorBox file {file_id} was not found in item {id}"))?;
            Ok(TorBoxPayload::Item(item, queue))
        }
        Op::TorBoxControl {
            service,
            id,
            operation,
        } => {
            client
                .control(service, id, &operation)
                .map_err(|_| format!("TorBox could not {operation} item {id}"))?
                .ensure_success()
                .map_err(|_| format!("TorBox rejected {operation} for item {id}"))?;
            Ok(TorBoxPayload::Lines(vec![format!(
                "TorBox {operation} complete for item {id}"
            )]))
        }
        _ => Err("not a TorBox operation".into()),
    }
}

fn launch_torbox_op(op: Op, cfg: config::Config, tx: Sender<TorBoxReply>) {
    let label = op.describe();
    std::thread::spawn(move || {
        let result = run_torbox_op(&cfg, op);
        let _ = tx.send(TorBoxReply { label, result });
    });
}

fn place_submitted_program(
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
    pub subtitle_tx: Sender<SubtitleOutcome>,
    pub subtitle_rx: Receiver<SubtitleOutcome>,
    pub remote_search_tx: Sender<core::RemoteSearchQuery>,
    pub remote_search_result_rx: Receiver<RemoteSearchOutcome>,
    pub scan_rx: Receiver<library::ScanBatch>,
    sync_rx: Receiver<BackgroundSyncResult>,
    _servers_tx: Sender<Vec<config::NavidromeConfig>>,
    server_reachable: HashMap<String, bool>,
    pub scrobble: ScrobbleState,
    scrobble_failed_rx: Receiver<core::ScrobbleFailed>,
    pub favorites: favorites::Favorites,
    pub remote_starred: HashSet<String>,
    pub lyrics_client: lyrics::Client,
    pub lyrics_loader: lyrics::Loader,
    pub lyrics_pane: wim::lyricspane::LyricsPane,
    pub first_run: bool,
    pub interactive_prompt: bool,
    pub track_resolver: TrackResolver,
    pub star_tx: mpsc::Sender<StarFailed>,
    pub star_rx: mpsc::Receiver<StarFailed>,
    pub base_volume: f64,
    pub crossfade_enabled: bool,
    pub between_tracks: BetweenTracks,
    /// Per-identity settings the user has overridden by hand; see
    /// `core::track_started`. shellswim applied none of these before.
    pub overridden: wim::shellsong_host::Overridden,
    pub resume_due: Option<Duration>,
    /// The query whose results have been printed, and the names
    /// already printed for it. Kept so a server answering after the
    /// settle window prints only its *new* matches rather than
    /// repeating the list or saying nothing at all.
    echoed: Option<(String, HashSet<String>)>,
    pub undo: wim::undo::Recorder,
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
    queued_programs: VecDeque<Box<Program>>,
    pub swim: SwimSurface,
    pub start: Instant,
    torbox_tx: Sender<TorBoxReply>,
    torbox_rx: Receiver<TorBoxReply>,
    torbox_pending: bool,
}

impl Host {
    /// Nothing here blocks on the library, the network or the
    /// terminal: the scan and the catalog warm on their own threads
    /// and the prompt is usable before either answers.
    pub fn start() -> anyhow::Result<Host> {
        let first_run = !config::config_path()?.exists();
        let cfg = config::load_or_init()?;
        wim::trace_mark!("config loaded");

        // `rescan()` replaces this channel wholesale: the scan thread
        // signs off by dropping its sender, so a finished one cannot
        // be reused.
        let (scan_tx, scan_rx) = mpsc::channel::<library::ScanBatch>();
        library::spawn_scan(cfg.library_paths.clone(), scan_tx);
        // last session's catalog, off disk, so the first prompt can
        // already complete track names.
        let warming = search::spawn_warm_from_cache(&cfg.navidrome_servers);
        let (sync_tx, sync_rx) = mpsc::channel::<BackgroundSyncResult>();
        let (servers_tx, servers_rx) = mpsc::channel();
        catalog::spawn_background_sync(cfg.navidrome_servers.clone(), sync_tx, servers_rx);

        // returns as soon as mpv is forked; the socket wait is on a
        // connector thread. nothing below touches the player, so the
        // prompt no longer waits for it: `observe_playback` happens
        // after the first frame, in `main`.
        let player = mpv::Mpv::spawn()?;
        let buffer_warning = player
            .set_buffer_ahead(cfg.buffer_ahead_secs)
            .err()
            .map(|error| format!("buffer_ahead_secs could not be applied: {error}"));
        player.set_replaygain("track")?;
        player.set_subtitle_style(&cfg.subtitles);
        wim::trace_mark!("mpv forked");

        let mut app = app::App::new(warming.take());
        app.scanning_library = true;
        app.syncing_servers = !cfg.navidrome_servers.is_empty();
        let restored_eq = wim::eq::restore(
            cfg.eq_preset.as_deref(),
            cfg.eq_gains,
            &cfg.eq_custom_presets,
        );
        app.eq_source = restored_eq.source;
        app.eq_custom = restored_eq.modified;
        app.eq_gains = restored_eq.gains;
        if let Some(wim::eq::Source::Builtin(index)) = restored_eq.source {
            app.eq_preset = index;
        }
        let _ = player.set_af(&wim::eq::build_player_af_filter(
            &app.eq_gains,
            cfg.eq_enabled,
            cfg.visualizer_bands as usize,
        ));
        wim::trace_mark!("corpus warmed from cache");

        let (download_tx, download_rx) = mpsc::channel();
        let (subtitle_tx, subtitle_rx) = mpsc::channel();
        let (remote_search_tx, remote_search_rx) = mpsc::channel();
        let (remote_search_result_tx, remote_search_result_rx) = mpsc::channel();
        core::spawn_background_search(remote_search_rx, remote_search_result_tx);

        // not read from mpv: that is an ipc round trip, so it would
        // put the 65-120ms socket wait back on the startup path for a
        // value that is provably 100 here. `--no-config` means no
        // mpv.conf can have set it and nothing has loaded a file yet,
        // so mpv is at its documented default; `a_fresh_mpv_starts_at_
        // full_volume` checks that against a real mpv rather than
        // trusting it.
        let base_volume = wim::shellsong_host::VOLUME_CEILING;
        let crossfade_enabled = cfg.crossfade_enabled;
        let gap_at_start = cfg.gap_seconds;
        let lyrics_client = lyrics::Client::with_providers(
            cfg.lyrics.providers.clone(),
            cfg.resolve_lyrics_api_keys(),
        );
        let (star_tx, star_rx) = mpsc::channel();
        let (scrobble_failed_tx, scrobble_failed_rx) = mpsc::channel();
        let (torbox_tx, torbox_rx) = mpsc::channel();
        let mut notifications = Notifications::default();
        if cfg.offline_download_format != "flac"
            && cfg.effective_offline_download_bitrate_kbps() <= 96
        {
            notifications.push(
                Severity::Alert,
                format!(
                    "low offline audio quality: downloads will use {} kbps",
                    cfg.effective_offline_download_bitrate_kbps()
                ),
            );
        }
        if let Some(warning) = buffer_warning {
            notifications.push(Severity::Alert, warning);
        }
        let cache_index = Arc::new(Mutex::new(cache::CacheIndex::load()));
        core::sweep_cache(&cache_index, cfg.cache_eviction_days);
        cache::initialize_budget(cfg.offline_cache_max_bytes);
        Ok(Host {
            cfg,
            player,
            app,
            notifications,
            play_queue: PlayQueue::default(),
            previous_track: None,
            cache_index,
            download_tx,
            download_rx,
            downloading: HashSet::new(),
            subtitle_tx,
            subtitle_rx,
            remote_search_tx,
            remote_search_result_rx,
            scan_rx,
            sync_rx,
            _servers_tx: servers_tx,
            server_reachable: HashMap::new(),
            scrobble: ScrobbleState {
                failed_tx: Some(scrobble_failed_tx),
                ..ScrobbleState::default()
            },
            scrobble_failed_rx,
            favorites: favorites::load(),
            remote_starred: HashSet::new(),
            lyrics_client,
            lyrics_loader: lyrics::Loader::new(),
            lyrics_pane: wim::lyricspane::LyricsPane::default(),
            first_run,
            interactive_prompt: false,
            track_resolver: TrackResolver::new(),
            star_tx,
            star_rx,
            base_volume,
            crossfade_enabled,
            between_tracks: BetweenTracks::new(gap_at_start),
            overridden: wim::shellsong_host::Overridden::default(),
            resume_due: None,
            echoed: None,
            undo: wim::undo::Recorder::default(),
            fade_in_started_at: None,
            fade: None,
            sleep_due: None,
            settle: SearchSettle::default(),
            saved_queues: Vec::new(),
            vars: wim::shellsong_host::load_vars(),
            bindings: wim::shellsong_host::load_bindings(),
            scripts: wim::shellsong_host::script_names(),
            playback: mpv::Playback::default(),
            console: Console::new(),
            program: None,
            queued_programs: VecDeque::new(),
            swim: SwimSurface::default(),
            start: Instant::now(),
            torbox_tx,
            torbox_rx,
            torbox_pending: false,
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
        while let Ok(reply) = self.torbox_rx.try_recv() {
            self.torbox_pending = false;
            let now = self.start.elapsed();
            let now_playing = self.play_queue.current().cloned();
            let mut surf = self.swim.borrowed();
            let mut core = core_ctx!(self, now, now_playing);
            match reply.result {
                Ok(TorBoxPayload::Lines(lines)) => {
                    for line in lines {
                        core.console.note(line);
                    }
                }
                Ok(TorBoxPayload::Item(item, queue)) => {
                    let action = if queue {
                        app::Action::Queued(item)
                    } else {
                        app::Action::Play(item)
                    };
                    match core::dispatch_action(action, &mut core, &mut surf) {
                        core::Dispatch::Continue => {}
                        core::Dispatch::Quit => self.torbox_pending = false,
                    }
                }
                Err(error) => core.console.fail(format!("{}: {error}", reply.label)),
            }
        }
        // a queue batch resolved; the resolver already dropped any from
        // a superseded request, which here would append to the queue
        // rather than merely drawing something stale.
        while let Some((into, label, tracks)) = self.track_resolver.poll() {
            let now = self.start.elapsed();
            let now_playing = self.play_queue.current().cloned();
            let mut surf = self.swim.borrowed();
            let mut core = core_ctx!(self, now, now_playing);
            match tracks {
                Ok(tracks) if tracks.is_empty() => core
                    .console
                    .note(format!("nothing to {} in {label}", into.verb())),
                Ok(tracks) => match into {
                    Intent::Play => {
                        match core::start_session(tracks, false, &mut core, &mut surf) {
                            Ok(status) => core.console.note(status),
                            Err(why) => core.console.error = Some(why),
                        }
                    }
                    Intent::Back => {
                        core::append_resolved(&tracks, &mut core);
                        // the row for this queue op was written before the
                        // fan-out landed, so it holds a state with none of
                        // these tracks in it and redo would lose them.
                        let after = wim::undo::Snapshot::capture(&core);
                        core.undo.amend_row(&format!("queued {label}"), after);
                    }
                    Intent::Front => match core::front_resolved(&tracks, &mut core) {
                        Ok(n) => core.console.note(format!("queued {n} tracks next")),
                        Err(why) => core.console.note(why),
                    },
                    Intent::Download => {
                        if core.cfg.offline_download_format != "flac"
                            && core.cfg.effective_offline_download_bitrate_kbps() <= 96
                        {
                            core.notifications.push(
                                Severity::Alert,
                                format!(
                                    "low offline audio quality: downloads will use {} kbps",
                                    core.cfg.effective_offline_download_bitrate_kbps()
                                ),
                            );
                        }
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
        while let Ok(failed) = self.scrobble_failed_rx.try_recv() {
            self.notifications.push(
                Severity::Error,
                format!("ListenBrainz {} failed: {}", failed.kind, failed.error),
            );
        }

        // a lyrics fetch landed. the loader has already dropped any
        // reply for a track the user moved past, so whatever arrives
        // belongs to the request still standing.
        if let Some((track, result)) = self.lyrics_loader.poll() {
            match result {
                Ok(Some(l)) => {
                    self.lyrics_client.track_display(&l);
                    if self.interactive_prompt && self.swim.lyrics_requested {
                        self.lyrics_pane.open_lyrics(&l, track);
                        self.swim.lyrics_requested = false;
                    } else {
                        let text = wim::core::lyrics_text(&l);
                        let count = text.lines().count();
                        for line in text.lines() {
                            self.console.note(line.to_string());
                        }
                        if let Some(attribution) = l.attribution.as_deref() {
                            self.console.note(attribution.to_owned());
                        }
                        self.console.note(format!("{count} lines for {track}"));
                        self.swim.lyrics_requested = false;
                    }
                }
                Ok(None) => {
                    self.swim.lyrics_requested = false;
                    self.console.note(format!("no lyrics for {track}"));
                }
                Err(e) => {
                    self.swim.lyrics_requested = false;
                    self.console.note(format!("lyrics lookup failed: {e}"));
                }
            }
        }

        let mut drained = false;
        let mut advanced: Option<SearchItem> = None;
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
                    advanced = self.play_queue.current().cloned();
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
                    self.console
                        .note(format!("looped back to: {}", label(&track)));
                    advanced = Some(track);
                }
            }
        }

        // a settled search prints what it found. shellswim has no
        // sidebar, so `search(x)` used to answer with a count and
        // nothing else, which is no answer at all.
        if self.swim.echo_results && self.settle.settled(self.start.elapsed()) {
            self.swim.echo_results = false;
            let mut shown = HashSet::new();
            let lines = core::echo_lines(self.app.results(), &mut shown, self.console_width());
            if lines.is_empty() {
                self.console.note("no matches".to_string());
            }
            for line in lines {
                self.console.note(line);
            }
            self.echoed = Some((self.app.query.clone(), shown));
        }

        // a track started, by advancing or by a loop-all restart.
        if let Some(track) = advanced {
            // Keep video subtitle discovery aligned with Wim. The
            // lookup is dispatched to its worker thread and is a no-op
            // for audio or when no OpenSubtitles key is configured.
            core::fire_subtitle_fetch(&self.cfg, &self.subtitle_tx, &track);
            if self.play_queue.current().is_some() {
                self.console.note(format!("now playing: {}", label(&track)));
            }
            let now = self.start.elapsed();
            let np = Some(track.clone());
            let started = {
                let mut surf = self.swim.borrowed();
                let mut core = core_ctx!(self, now, np);
                let s = core::track_started(&track, &mut core);
                let _ = &mut surf;
                s
            };
            // through the same slot `queue_program` writes, so the
            // two routes cannot diverge: assigning `self.program`
            // here used to cut a typed program short on a track
            // change, where the slot waits its turn instead.
            if !started.ops.is_empty() {
                self.swim.queued = Some(Box::new(wim::shellsong::Program::of(started.ops)));
            }
            if let Some(bound) = started.bound {
                self.console.note(bound);
            }
            if self.interactive_prompt && (self.lyrics_pane.is_open() || self.swim.lyrics_requested)
            {
                self.request_lyrics(&track);
            }
        }

        // the between-track gap, same mechanism as wim's loop: mpv
        // holds at each file's end under `keep-open` and pauses
        // itself, and this waits the gap out before advancing.
        self.between_tracks.step(
            &self.player,
            self.playback.eof_reached,
            self.play_queue.upcoming_len() > 0,
            Instant::now(),
        );

        // Keep time-based playback behavior live even while the prompt
        // is idle or a command is waiting for network results.
        let now = self.start.elapsed();
        if let Some(ramp) = self.fade {
            let _ = self.player.set_volume_nowait(ramp.level_at(now));
            if ramp.finished(now) {
                self.fade = None;
            }
        }
        if self.crossfade_enabled {
            if let Some(started) = self.fade_in_started_at {
                let span = config::crossfade_seconds(self.cfg.crossfade_seconds);
                let t = (started.elapsed().as_secs_f64() / span).clamp(0.0, 1.0);
                let _ = self.player.set_volume_nowait(self.base_volume * t);
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
                    let _ = self
                        .player
                        .set_volume_nowait(self.base_volume * (remaining / span).clamp(0.0, 1.0));
                }
            }
        }
        if self.resume_due.is_some_and(|due| now >= due) {
            self.resume_due = None;
            let _ = self.player.set_pause(false);
            self.console.note("resumed".to_string());
        }
        if let Some(due) = self.sleep_due {
            let fade =
                Duration::from_secs_f64(config::crossfade_seconds(self.cfg.crossfade_seconds));
            match wim::shellsong_host::sleep_step(due, fade, self.base_volume, now) {
                wim::shellsong_host::SleepStep::Waiting => {}
                wim::shellsong_host::SleepStep::Fading(level) => {
                    let _ = self.player.set_volume_nowait(level);
                }
                wim::shellsong_host::SleepStep::Expired => {
                    self.sleep_due = None;
                    if !self.playback.paused {
                        let _ = self.player.toggle_pause();
                    }
                    let _ = self.player.set_volume(self.base_volume);
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
                if wim::listenbrainz::should_scrobble(&self.cfg.scrobble, elapsed, duration) {
                    let listened_at = self
                        .scrobble
                        .started_unix
                        .unwrap_or_else(wim::listenbrainz::now_unix);
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
            // deliberately does *not* settle: a local scan batch is
            // not an answer to the remote query the window is waiting
            // on, and clearing it here let a program proceed while the
            // search was still outstanding. wim never did this, so the
            // two front-ends now agree.
            self.app.resync_results();
        }
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
            let avail = self.console_width();
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
                    let _ = self.player.sub_add(&ready.path);
                }
                // said rather than swallowed: only "no subtitles for
                // this film" is silent now, so a refused key does not
                // look like one.
                core::SubtitleOutcome::Failed { why, .. } => self.console.fail(why),
            }
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
        if self.torbox_pending {
            return Drive::Await;
        }
        // the same rule wim's loop uses: a program the core queued
        // starts only once nothing else is running, so bound settings
        // cannot interrupt something the user typed.
        if self.program.is_none() {
            self.program = self
                .swim
                .queued
                .take()
                .or_else(|| self.queued_programs.pop_front());
        }
        let Some(program) = self.program.as_mut() else {
            return Drive::Done;
        };
        let now = self.start.elapsed();
        let now_playing = self.play_queue.current().cloned();
        let mut surf = self.swim.borrowed();
        let mut core = core_ctx!(self, now, now_playing);
        let tx = self.torbox_tx.clone();
        let cfg = self.cfg.clone();
        let pending = &mut self.torbox_pending;
        let outcome = core::drive_with(
            program,
            &mut core,
            &mut surf,
            |c| c.settle.settled(c.now),
            |op, core, surf| {
                if is_torbox_op(&op) {
                    launch_torbox_op(op, cfg.clone(), tx.clone());
                    *pending = true;
                    core::OpDrive::Await
                } else {
                    match core::apply_op(op, core, surf) {
                        core::Dispatch::Continue => core::OpDrive::Continue,
                        core::Dispatch::Quit => core::OpDrive::Quit,
                    }
                }
            },
        );
        if outcome != Drive::Await {
            self.program = None;
        }
        outcome
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

    /// Parses a line against the live bindings and script directory,
    /// the same environment used by interactive submission.
    pub fn begin(&mut self, line: &str) {
        self.console.push(LineKind::Command, line.to_string());
        let read = |name: &str| wim::shellsong_host::read_script(name);
        let world = wim::shellsong::World {
            scope: &self.vars,
            read: Some(&read),
        };
        match Program::parse_in(line, &world) {
            Ok(p) => self.submit_program(Box::new(p)),
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
            while self.has_program_work() {
                self.pump();
                if self.step() == Drive::Quit {
                    self.program = None;
                    break;
                }
                if self.has_program_work() {
                    if Instant::now() >= deadline {
                        self.console.fail(format!("gave up waiting on '{line}'"));
                        self.program = None;
                        self.queued_programs.clear();
                        self.swim.queued = None;
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

    fn request_lyrics(&mut self, item: &SearchItem) {
        let Some((artist, title, album)) = core::lyrics_query(item) else {
            self.lyrics_pane.close();
            self.swim.lyrics_requested = false;
            return;
        };
        self.lyrics_pane.close();
        self.swim.lyrics_requested = true;
        let name = label(item);
        if let Some((server, id)) = core::lyrics_remote(item) {
            self.lyrics_loader.request_subsonic(
                &self.lyrics_client,
                server,
                id,
                name,
                &artist,
                &title,
                &album,
                None,
            );
        } else if let Some(path) = core::lyrics_path(item) {
            self.lyrics_loader.request_local(
                &self.lyrics_client,
                name,
                &artist,
                &title,
                &album,
                None,
                path,
            );
        } else {
            self.lyrics_loader
                .request(&self.lyrics_client, name, &artist, &title, &album, None);
        }
    }
}

#[cfg(test)]
mod program_queue_tests {
    use super::*;

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
