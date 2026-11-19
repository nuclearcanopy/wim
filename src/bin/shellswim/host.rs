//! Everything shellswim owns, so the prompt and a one-shot `-c` share
//! one startup and one step loop rather than two that can drift.
//!
//! The fields are owned here (wim keeps the same set as `main`
//! locals); `CoreCtx` is still built per use, because the borrow of
//! the core and the borrow of the surface have to stay disjoint.

use std::collections::HashSet;
use std::sync::mpsc::{self, Receiver, Sender};
use std::time::{Duration, Instant};

/// The one `CoreCtx` literal, as a macro rather than a method: it
/// borrows most of `Host` plus two locals (`now`, `now_playing`),
/// and a method cannot return a struct borrowing locals it made.
/// Same reason `main.rs` keeps its own `core_ctx!`.
macro_rules! core_ctx {
    ($h:expr, $now:expr, $np:expr) => {
        CoreCtx {
            cfg: &$h.session.cfg,
            player: &$h.player,
            app: &mut $h.session.app,
            notifications: &mut $h.session.notifications,
            queue: &mut $h.session.play_queue,
            previous_track: &mut $h.session.previous_track,
            cache: &$h.session.cache_index,
            download_tx: &$h.session.download_tx,
            downloading: &mut $h.session.downloading,
            scrobble: &mut $h.session.scrobble,
            subtitle_tx: &$h.session.subtitle_tx,
            console: &mut $h.session.console,
            now: $now,
            now_playing: &$np,
            favorites: &mut $h.session.favorites,
            remote_starred: &mut $h.session.remote_starred,
            lyrics_client: &$h.session.lyrics_client,
            lyrics_loader: &mut $h.session.lyrics_loader,
            star_tx: &$h.session.star_tx,
            track_resolver: &mut $h.session.track_resolver,
            base_volume: &mut $h.session.base_volume,
            muted: $h.session.playback.muted,
            crossfade_enabled: &mut $h.session.crossfade_enabled,
            overridden: &mut $h.session.overridden,
            undo: &mut $h.session.undo,
            between_tracks: &mut $h.session.between_tracks,
            fade_in_started_at: &mut $h.session.fade_in_started_at,
            fade: &mut $h.session.fade,
            sleep_due: &mut $h.session.sleep_due,
            resume_due: &mut $h.session.resume_due,
            scan_rx: &mut $h.session.scan_rx,
            remote_search_tx: &$h.session.remote_search_tx,
            settle: &mut $h.session.settle,
            saved_queues: &mut $h.session.saved_queues,
            vars: &mut $h.session.vars,
            bindings: &mut $h.session.bindings,
        }
    };
}

use wim::core::{self, CoreCtx, Drive, Intent};
use wim::notify::Severity;
use wim::search::SearchItem;
use wim::shellsong::{LineKind, Op, Program};
use wim::swimsession::SwimSession;
use wim::ui::results_list::label;
use wim::{app, config, mpv};

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

/// The mpv configuration `Host::start` collects instead of applying,
/// so the prompt never waits for mpv's socket.
struct PlayerSetup {
    commands: Vec<Vec<serde_json::Value>>,
    proxy_index: Option<usize>,
    replaygain_index: usize,
}

/// What the background setup learned, as plain data for the UI thread.
pub struct PlayerSetupResult {
    pub warnings: Vec<String>,
    pub proxy_ready: bool,
    pub observe_error: Option<String>,
}

pub struct Host {
    pub player: mpv::Mpv,
    /// A server being added or reconnected, answering off the ui thread.
    pub pending_connect: Option<crate::nav_ui::PendingConnect>,
    pub pending_connectivity: Option<std::sync::mpsc::Receiver<wim::catalog::ConnectivityReport>>,
    _instance_lock: Option<std::fs::File>,
    pub lyrics_pane: wim::lyricspane::LyricsPane,
    pub first_run: bool,
    /// Kept alive for the player's lifetime; dropping it stops the relay.
    pub _media_proxy: Option<wim::media_proxy::MediaProxy>,
    pub proxy_ready: bool,
    player_setup: Option<PlayerSetup>,
    pub session: SwimSession,
    torbox_tx: Sender<TorBoxReply>,
    torbox_rx: Receiver<TorBoxReply>,
    torbox_pending: bool,
}

impl Host {
    /// Nothing here blocks on the library, the network or the
    /// terminal: the scan and the catalog warm on their own threads
    /// and the prompt is usable before either answers.
    pub fn start() -> anyhow::Result<Host> {
        // No meter is drawn here: skip the analysis bank in mpv's filter graph.
        wim::eq::set_analysis(false);
        let first_run = !config::config_path()?.exists();
        let cfg = config::load_or_init()?;
        wim::trace_mark!("config loaded");
        let mut session = SwimSession::new(cfg);
        let cfg = &session.cfg;

        // returns as soon as mpv is forked; the socket wait is on a
        // connector thread. nothing below touches the player, so the
        // prompt no longer waits for it: `observe_playback` happens
        // after the first frame, in `main`.
        let player = mpv::Mpv::spawn()?;
        // Everything below is sent to mpv as one ordered batch after the
        // first frame (`begin_player_setup`), never waited on here.
        let mut setup_commands = vec![vec![
            serde_json::json!("set_property"),
            serde_json::json!("demuxer-readahead-secs"),
            serde_json::json!(cfg.buffer_ahead_secs.max(1.0)),
        ]];
        // every network stream goes through the same address-pinning
        // relay wim uses; only configured Navidrome origins may resolve
        // to private addresses. Failing closed beats streaming unpinned.
        let trusted_origins = cfg
            .navidrome_servers
            .iter()
            .map(|server| server.url.clone())
            .collect::<Vec<_>>();
        let media_proxy = wim::media_proxy::MediaProxy::start(&trusted_origins).ok();
        let (proxy_index, proxy_unavailable) = match &media_proxy {
            Some(proxy) => {
                setup_commands.push(vec![
                    serde_json::json!("set_property"),
                    serde_json::json!("http-proxy"),
                    serde_json::json!(proxy.proxy_url()),
                ]);
                (Some(setup_commands.len() - 1), None)
            }
            None => (
                None,
                Some(String::from(
                    "secure media relay is unavailable; online Watch sources are disabled",
                )),
            ),
        };
        let replaygain_index = setup_commands.len();
        setup_commands.push(vec![
            serde_json::json!("set_property"),
            serde_json::json!("replaygain"),
            serde_json::json!("track"),
        ]);
        setup_commands.extend(mpv::Mpv::subtitle_style_commands(&cfg.subtitles));
        wim::trace_mark!("mpv forked");

        // applied with the rest of the player setup, off the first frame
        setup_commands.push(vec![
            serde_json::json!("set_property"),
            serde_json::json!("af"),
            serde_json::json!(wim::eq::build_player_af_filter(
                &session.app.eq_gains,
                cfg.eq_enabled,
                cfg.visualizer_bands as usize,
            )),
        ]);

        let (torbox_tx, torbox_rx) = mpsc::channel();
        session.push_startup_alerts(proxy_unavailable);
        let instance_lock = match wim::persistence::state_dir()
            .map(|dir| wim::fsutil::claim_instance_lock(&dir))
        {
            Ok(wim::fsutil::InstanceLock::Held(file)) => Some(file),
            Ok(wim::fsutil::InstanceLock::Contended) => {
                session.notifications.push(
                    Severity::Alert,
                    "another wim is running; play history, favorites and command history are merged, but the saved queue, resume position and settings are whichever wim saves last",
                );
                None
            }
            _ => None,
        };
        Ok(Host {
            _instance_lock: instance_lock,
            player,
            pending_connect: None,
            pending_connectivity: None,
            lyrics_pane: wim::lyricspane::LyricsPane::default(),
            first_run,
            _media_proxy: media_proxy,
            proxy_ready: false,
            player_setup: Some(PlayerSetup {
                commands: setup_commands,
                proxy_index,
                replaygain_index,
            }),
            session,
            torbox_tx,
            torbox_rx,
            torbox_pending: false,
        })
    }

    fn setup_outcome(
        plan: &PlayerSetup,
        results: &[anyhow::Result<serde_json::Value>],
    ) -> PlayerSetupResult {
        let mut warnings = Vec::new();
        if let Some(Err(error)) = results.first() {
            warnings.push(format!("buffer_ahead_secs could not be applied: {error}"));
        }
        let proxy_ready = plan
            .proxy_index
            .and_then(|index| results.get(index))
            .is_some_and(Result::is_ok);
        if plan.proxy_index.is_some() && !proxy_ready {
            warnings.push("secure media relay could not be applied".into());
        }
        if let Some(Err(error)) = results.get(plan.replaygain_index) {
            warnings.push(format!("replaygain could not be applied: {error}"));
        }
        PlayerSetupResult {
            warnings,
            proxy_ready,
            observe_error: None,
        }
    }

    /// Sends the collected mpv configuration, then registers the property
    /// observers, on a worker; the answer is `finish_player_setup`'s input.
    pub fn begin_player_setup(&mut self) -> Receiver<PlayerSetupResult> {
        let plan = self.player_setup.take().expect("player setup runs once");
        self.player.run_background(move |client| {
            let results = client.command_batch(&plan.commands);
            let mut outcome = Self::setup_outcome(&plan, &results);
            outcome.observe_error = client.observe_playback().err().map(|e| e.to_string());
            outcome
        })
    }

    pub fn finish_player_setup(&mut self, outcome: PlayerSetupResult) {
        self.proxy_ready = outcome.proxy_ready;
        for warning in outcome.warnings {
            self.session.notifications.push(Severity::Alert, warning);
        }
        if let Some(error) = outcome.observe_error {
            self.session.console.error =
                Some(format!("mpv will not report playback position: {error}"));
        }
    }

    /// One-shots have no prompt to keep responsive: configure mpv now.
    pub fn apply_player_setup_blocking(&mut self) {
        if let Some(plan) = self.player_setup.take() {
            let results = self.player.command_batch(&plan.commands);
            let outcome = Self::setup_outcome(&plan, &results);
            self.finish_player_setup(outcome);
        }
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
        self.session.now()
    }

    pub fn submit_program(&mut self, program: Box<Program>) {
        self.session.submit_program(program);
    }

    pub fn has_program_work(&self) -> bool {
        self.session.has_program_work()
    }

    pub fn begin(&mut self, line: &str) {
        self.session.begin(line);
    }

    /// Plays or queues one chosen item exactly as `play(...)` /
    /// `queue(...)` would for that item.
    pub fn act_on_item(&mut self, item: SearchItem, queue: bool) {
        let now = self.session.start.elapsed();
        let now_playing = self.session.play_queue.current().cloned();
        let mut surf = self.session.swim.borrowed();
        let mut core = core_ctx!(self, now, now_playing);
        let action = if queue {
            app::Action::Queued(item)
        } else {
            app::Action::Play(item)
        };
        let _ = core::dispatch_action(action, &mut core, &mut surf);
    }

    /// Runs one op through the same path the prompt uses, so undo and
    /// redo from the tree record, refuse at barriers and report alike.
    pub fn apply_one(&mut self, op: wim::shellsong::Op) {
        let now = self.session.start.elapsed();
        let now_playing = self.session.play_queue.current().cloned();
        let mut surf = self.session.swim.borrowed();
        let mut core = core_ctx!(self, now, now_playing);
        let _ = core::apply_op(op, &mut core, &mut surf);
    }

    /// Crosses to a sibling branch of the undo tree and restores it.
    pub fn switch_undo_branch(&mut self, delta: isize) {
        let now = self.session.start.elapsed();
        let now_playing = self.session.play_queue.current().cloned();
        let mut core = core_ctx!(self, now, now_playing);
        match core.undo.switch_branch(delta) {
            Some(snapshot) => {
                if let Err(why) = snapshot.restore(&mut core) {
                    core.console.fail(why);
                }
            }
            None => core.console.note("no other branch here"),
        }
    }

    pub fn now_playing(&self) -> Option<SearchItem> {
        self.session.play_queue.current().cloned()
    }

    /// Everything the background threads finished since the last call.
    /// Each one is a `try_recv`, so this is the whole reason nothing
    /// in shellswim blocks.
    pub fn pump(&mut self) {
        self.session.pump_player_health(&self.player);
        while let Ok(reply) = self.torbox_rx.try_recv() {
            self.torbox_pending = false;
            let now = self.session.start.elapsed();
            let now_playing = self.session.play_queue.current().cloned();
            let mut surf = self.session.swim.borrowed();
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
        while let Some((into, label, tracks)) = self.session.track_resolver.poll() {
            let now = self.session.start.elapsed();
            let now_playing = self.session.play_queue.current().cloned();
            let mut surf = self.session.swim.borrowed();
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
                    Intent::Browse => {
                        for track in &tracks {
                            core.console.note(wim::ui::results_list::label(track));
                        }
                        self.session.swim.list_requested = Some(wim::swimsurface::ListWorkspace {
                            title: label.clone(),
                            rows: tracks
                                .into_iter()
                                .map(|t| {
                                    let text = wim::ui::results_list::label(&t);
                                    (t, text)
                                })
                                .collect(),
                        });
                    }
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

        self.session.pump_failures();
        // a lyrics fetch landed. the loader has already dropped any
        // reply for a track the user moved past, so whatever arrives
        // belongs to the request still standing.
        if let Some((track, result)) = self.session.lyrics_loader.poll() {
            match result {
                Ok(Some(l)) => {
                    self.session.lyrics_client.track_display(&l);
                    if self.session.interactive_prompt && self.session.swim.lyrics_requested {
                        self.lyrics_pane.open_lyrics(&l, track);
                        self.session.swim.lyrics_requested = false;
                    } else {
                        let text = wim::core::lyrics_text(&l);
                        let count = text.lines().count();
                        for line in text.lines() {
                            self.session.console.note(line.to_string());
                        }
                        if let Some(attribution) = l.attribution.as_deref() {
                            self.session.console.note(attribution.to_owned());
                        }
                        self.session
                            .console
                            .note(format!("{count} lines for {track}"));
                        self.session.swim.lyrics_requested = false;
                    }
                }
                Ok(None) => {
                    self.session.swim.lyrics_requested = false;
                    self.session.console.note(format!("no lyrics for {track}"));
                }
                Err(e) => {
                    self.session.swim.lyrics_requested = false;
                    self.session
                        .console
                        .note(format!("lyrics lookup failed: {e}"));
                }
            }
        }

        let advanced = self.session.pump_player_events(&self.player);
        // a settled search prints what it found. shellswim has no
        // sidebar, so `search(x)` used to answer with a count and
        // nothing else, which is no answer at all.
        // Local and cached results are final once loading finishes; a
        // slow server's matches follow through the "...and from the
        // servers" lines below rather than holding these back.
        let local_ready =
            !self.session.app.scanning_library && !self.session.app.loading_cached_catalog;
        // one-shots keep waiting for the settle window so a script's
        // output is complete; only the interactive prompt echoes early
        if self.session.swim.echo_results
            && ((self.session.interactive_prompt && local_ready)
                || self.session.settle.settled(self.session.start.elapsed()))
        {
            self.session.swim.echo_results = false;
            let mut shown = HashSet::new();
            let lines =
                core::echo_lines(self.session.app.results(), &mut shown, self.console_width());
            if lines.is_empty() {
                self.session.console.note("no matches".to_string());
            }
            for line in lines {
                self.session.console.note(line);
            }
            self.session.echoed = Some((self.session.app.query.clone(), shown));
        }

        // a track started, by advancing or by a loop-all restart.
        if let Some(track) = advanced {
            // Keep video subtitle discovery aligned with Wim. The
            // lookup is dispatched to its worker thread and is a no-op
            // for audio or when no OpenSubtitles key is configured.
            core::fire_subtitle_fetch(&self.session.cfg, &self.session.subtitle_tx, &track);
            if self.session.play_queue.current().is_some() {
                self.session
                    .console
                    .note(format!("now playing: {}", label(&track)));
            }
            let now = self.session.start.elapsed();
            let np = Some(track.clone());
            let started = {
                let mut surf = self.session.swim.borrowed();
                let mut core = core_ctx!(self, now, np);
                let s = core::track_started(&track, &mut core);
                let _ = &mut surf;
                s
            };
            // through the same slot `queue_program` writes, so the
            // two routes cannot diverge: assigning `self.session.program`
            // here used to cut a typed program short on a track
            // change, where the slot waits its turn instead.
            if !started.ops.is_empty() {
                self.session.swim.queued = Some(Box::new(wim::shellsong::Program::of(started.ops)));
            }
            if let Some(bound) = started.bound {
                self.session.console.note(bound);
            }
            if self.session.interactive_prompt
                && (self.lyrics_pane.is_open() || self.session.swim.lyrics_requested)
            {
                self.request_lyrics(&track);
            }
        }

        self.session.pump_timers(&self.player);
        // A local batch does not settle a pending remote search.
        self.session.pump_library();
        self.session
            .pump_replies(&self.player, self.console_width());
        // notifications are wim's toast channel; an overlay here would
        // cover the thing it is talking about, so the text goes into
        // the scrollback, which is the only output shellswim has.
        for note in self.session.notifications.drain_text() {
            self.session.console.note(note);
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
        self.session.take_next_program();
        let Some(program) = self.session.program.as_mut() else {
            return Drive::Done;
        };
        let now = self.session.start.elapsed();
        let now_playing = self.session.play_queue.current().cloned();
        let mut surf = self.session.swim.borrowed();
        let mut core = core_ctx!(self, now, now_playing);
        let tx = self.torbox_tx.clone();
        let cfg = self.session.cfg.clone();
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
            self.session.program = None;
        }
        outcome
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
                    self.session.program = None;
                    break;
                }
                if self.has_program_work() {
                    if Instant::now() >= deadline {
                        self.session
                            .console
                            .fail(format!("gave up waiting on '{line}'"));
                        self.session.abandon_programs();
                        break;
                    }
                    std::thread::sleep(ONESHOT_TICK);
                }
            }
        }
        // one last drain, so a download or a track change that landed
        // on the way out still gets reported.
        self.pump();
        self.session
            .console
            .lines
            .iter()
            .any(|l| l.kind == LineKind::Error)
    }

    fn request_lyrics(&mut self, item: &SearchItem) {
        let Some((artist, title, album)) = core::lyrics_query(item) else {
            self.lyrics_pane.close();
            self.session.swim.lyrics_requested = false;
            return;
        };
        self.lyrics_pane.close();
        self.session.swim.lyrics_requested = true;
        let name = label(item);
        if let Some((server, id)) = core::lyrics_remote(item) {
            self.session.lyrics_loader.request_subsonic(
                &self.session.lyrics_client,
                server,
                id,
                name,
                &artist,
                &title,
                &album,
                None,
            );
        } else if let Some(path) = core::lyrics_path(item) {
            self.session.lyrics_loader.request_local(
                &self.session.lyrics_client,
                name,
                &artist,
                &title,
                &album,
                None,
                path,
            );
        } else {
            self.session.lyrics_loader.request(
                &self.session.lyrics_client,
                name,
                &artist,
                &title,
                &album,
                None,
            );
        }
    }
}

/// The prompt must be usable before mpv answers. These scan the source (see
/// docs/testing.md): a blocking player call slipped back into the start path
/// costs ~100 ms on every launch and no behavioural test would notice.
#[cfg(test)]
mod startup_path_tests {
    const HOST: &str = include_str!("host.rs");
    const MAIN: &str = include_str!("main.rs");
    const WIM_MAIN: &str = include_str!("../../main.rs");

    /// `reduced_motion` must reach both binaries through the shared rule.
    #[test]
    fn both_binaries_wire_reduced_motion() {
        assert!(WIM_MAIN.contains("init_reduced_motion("));
        assert!(MAIN.contains("init_reduced_motion("));
    }

    /// The text from `header` to the next line that closes the function at
    /// the same indentation.
    fn function<'a>(source: &'a str, header: &str, indent: &str) -> &'a str {
        let start = source
            .find(header)
            .unwrap_or_else(|| panic!("{header} not found"));
        let closer = format!("\n{indent}}}\n");
        let len = source[start..].find(&closer).expect("function end");
        &source[start..start + len]
    }

    const BLOCKING: [&str; 7] = [
        ".get_property(",
        ".command(",
        ".set_property(",
        "command_batch(",
        "wait_startup_",
        "apply_player_setup_blocking",
        ".recv()",
    ];

    #[test]
    fn host_start_does_no_mpv_io() {
        let body = function(HOST, "pub fn start() -> anyhow::Result<Host> {", "    ");
        assert!(body.contains("Mpv::spawn()"), "scan lost its anchor");
        for needle in BLOCKING {
            assert!(!body.contains(needle), "Host::start must not call {needle}");
        }
    }

    #[test]
    fn prompt_draws_before_it_waits_for_mpv() {
        let body = function(MAIN, "fn prompt() -> anyhow::Result<()> {", "");
        let before_loop = &body[..body.find("    loop {").expect("prompt loop")];
        assert!(before_loop.contains("begin_player_setup"));
        for needle in ["apply_player_setup_blocking", "wait_startup_"] {
            assert!(
                !before_loop.contains(needle),
                "prompt must not call {needle} before its first frame"
            );
        }
    }

    #[test]
    fn one_shot_still_blocks_on_setup_by_design() {
        let body = function(MAIN, "fn oneshot(cli: &Cli) -> anyhow::Result<()> {", "");
        assert!(body.contains("apply_player_setup_blocking"));
        assert!(body.contains("wait_startup_cache"));
        assert!(body.contains("wait_startup_catalog"));
    }

    #[test]
    fn animation_is_capped_at_the_shared_interval() {
        let body = function(MAIN, "fn prompt() -> anyhow::Result<()> {", "");
        assert!(body.contains("anim::animation_wait(frame)"));
        assert!(!body.contains("frame.interval()"));
    }
}
