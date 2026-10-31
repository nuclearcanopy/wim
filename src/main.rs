//! `wim`: the full pane ui. One of two front-ends over the `wim`
//! library; `shellswim` is the other. Everything not about panes,
//! focus and menus lives in the library, not here.

use wim::{
    anim, app, browse, cache, config, coverart, eq, favorites, hit, keymap, library, listenbrainz,
    lyrics, mpv, persistence, queues, search, shellsong, shellsong_host, suggest, term, trace, ui,
    video,
};

use std::collections::{HashMap, HashSet};
use std::sync::mpsc::{self, TryRecvError};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use crossterm::event::{self, Event, KeyCode, KeyModifiers, MouseButton, MouseEventKind};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui_image::picker::Picker;
use ratatui_image::StatefulImage;

use wim::catalog::{
    index_navidrome_server, merge_catalog_into_app, spawn_background_sync,
    test_navidrome_connectivity, BackgroundSyncResult,
};
use wim::core::{
    append_resolved, apply_eq_row, apply_op, browse_session, dispatch_action, download_resolved,
    download_tracks, drained, drive, enqueue_track, eq_preset_names, fire_scrobble,
    fire_subtitle_fetch, front_resolved, go_to_previous, load_playlist, lyrics_query,
    note_playback_started, notify_query_changed, op_done, op_fail, op_note, open_starred,
    playing_duration, print_listing, save_playback_state, set_vid_for_item, shellsong_focus,
    spawn_background_search, start_session, start_tracking_playback, suggestion_starred_keys,
    sweep_cache, toggle_star, track_started, BetweenTracks, CoreCtx, Dispatch, DownloadCtx,
    Drained, Drive, Intent, RemoteSearchOutcome, RemoteSearchQuery, Report, Resolution,
    ScrobbleState, SelectStep, SessionStart, StarFailed, SubtitleReady, Surface, TrackChangeCtx,
    TrackResolver,
};
use wim::download::DownloadEvent;
use wim::enrich::{
    next_enrichment_job, recompute_suggestions, spawn_background_enrichment, EnrichmentEvent,
    EnrichmentJob, Warmup,
};
use wim::eq_menu_keys::{
    adjust_eq_band, eq_label, eq_menu_editor_key, eq_saved_row_hint, persist_eq,
};
use wim::focus::{focus_direction_key, focus_in_direction, InputMode};
use wim::graphics::{
    art_mode_for_picker, cell_aspect, graphics_plausible, protocol_image, real_graphics,
    wants_full_art, warn_no_graphics, ProtocolCache,
};
use wim::keymap::{KeyAction, Keymap};
use wim::layout::{PanelLayout, ResizePanel};
use wim::lyricspane::LyricsPane;
use wim::navmanager::{NavManager, NavManagerStep};
use wim::notify::{Notifications, Severity};
use wim::onboarding::{Onboarding, Step};
use wim::playqueue::{PlayQueue, PosChange};
use wim::search::SearchItem;
use wim::shellsong::{Console, ConsoleAction, ViewTarget};
use wim::shellsong_host::{CorpusCandidates, Ramp, SearchSettle, SleepStep};
use wim::theme::{shade, NEAR_BLACK};
use wim::transport::{
    activate_transport, audio_level, commit_scrub, now_playing_is_video, ScrubAt, TransportCtx,
};
use wim::ui::banner::Banner;
use wim::ui::browse::BrowseListView;
use wim::ui::controlsbar::{
    self, ControlsBar, Field, Nav, NavKeys, Scrub, SCRUB_STEP, SCRUB_STEP_COARSE,
};
use wim::ui::divider::{HorizontalRule, VerticalRule};
use wim::ui::gap::Gap;
use wim::ui::login::{LoginForm, LoginView};
use wim::ui::lyrics_view::LyricsView;
use wim::ui::miniplayer::MiniPlayerBar;
use wim::ui::nav_manager::NavManagerListView;
use wim::ui::notify::NotificationOverlay;
use wim::ui::onboarding::{
    ControlsView, DoneView, LibraryPathsView, ListenBrainzView, WelcomeView,
};
use wim::ui::player::PlayerView;
use wim::ui::results_list::{label, ResultsList};
use wim::ui::searchbox::SearchBox;
use wim::ui::shellsong::ShellsongConsole;
use wim::ui::visualizer::LevelMeter;
use wim::Focus;

/// The one way a browse view is opened, so the keyboard's two enter
/// paths (a search result, and an album picked out of the artist
/// profile's ALBUMS column) and any pointer activation of the same
/// rows cannot drift apart; nothing here touches the network, the
/// loader's workers do. A kind with nothing to browse is a no-op.
fn open_browse(
    item: &SearchItem,
    loader: &mut browse::Loader,
    view: &mut Option<browse::BrowseView>,
    browse_art: &mut ProtocolCache,
    focus: &mut Focus,
) {
    let Some(opened) = loader.open(item) else {
        return;
    };
    // the previous view's cover must not sit under the new title; the
    // new one is built when `Loader::poll` hands the image over.
    browse_art.clear();
    *view = Some(opened);
    // opening something to browse and then still needing a separate
    // keypress just to be able to scroll it read as "enter doesn't do
    // anything"; land with it already focused instead.
    *focus = Focus::Workspace;
}

/// Crossterm's mouse event, reduced to what `hit::act` decides from.
/// Only the left button is wired up: no other button means anything
/// anywhere in this app, and a terminal reporting one should not be
/// able to press a control by accident.
fn mouse_kind(kind: MouseEventKind) -> hit::Kind {
    match kind {
        MouseEventKind::Down(MouseButton::Left) => hit::Kind::Down,
        MouseEventKind::Drag(MouseButton::Left) => hit::Kind::Drag,
        MouseEventKind::Up(MouseButton::Left) => hit::Kind::Up,
        MouseEventKind::ScrollUp => hit::Kind::WheelUp,
        MouseEventKind::ScrollDown => hit::Kind::WheelDown,
        _ => hit::Kind::Other,
    }
}

// ------------------------------------------------------ shellsong

/// wim's half of the seam: the pane state the core deliberately does
/// not hold, bundled per use the way `CoreCtx` is. Every method here
/// is the same write the matching keybind makes, so a shellsong
/// command and a keypress cannot drift; see docs/library-seam.md.
struct WimSurface<'a> {
    focus: &'a mut Focus,
    browse_view: &'a mut Option<browse::BrowseView>,
    browse_loader: &'a mut browse::Loader,
    browse_art: &'a mut ProtocolCache,
    player_minimized: &'a mut bool,
    flash: &'a mut Option<(Field, Instant)>,
    console_open: &'a mut bool,
    queue_selected: &'a mut usize,
    queue_view_open: &'a mut bool,
    suggestions: &'a mut Option<suggest::Suggestions>,
    suggestions_selected: &'a mut usize,
    settings_menu_open: &'a mut bool,
    keybinds_menu_open: &'a mut bool,
    keybinds_menu_selected: &'a mut usize,
    eq_menu_open: &'a mut bool,
    eq_menu_selected: &'a mut usize,
    /// Reset when the menu opens, so a half-typed name editor or an
    /// armed delete cannot survive a close and reopen. The click path
    /// always did this; `view(eq)` did not, and routing the click
    /// through the op made that difference visible. Fixed here rather
    /// than preserved, since opening fresh is right either way.
    eq_menu: &'a mut ui::eq_menu::EqMenuState,
    undo_menu_open: &'a mut bool,
    bottombar_selected: Field,
}

impl Surface for WimSurface<'_> {
    fn front_end(&self) -> &'static str {
        "wim"
    }

    /// A focused browse view owns the highlight, otherwise the
    /// sidebar does; exactly how the star and download keys resolve
    /// it.
    fn selection(&self, core: &CoreCtx) -> Option<SearchItem> {
        if let (Some(bv), Focus::Workspace) = (self.browse_view.as_ref(), *self.focus) {
            return if bv.albums_focused {
                bv.albums.get(bv.albums_selected).cloned()
            } else {
                bv.tracks.get(bv.selected).cloned()
            };
        }
        core.app.results.get(core.app.selected).cloned()
    }

    fn focus(&self) -> shellsong::Focus {
        shellsong_focus(*self.focus)
    }

    fn browsing(&self) -> bool {
        self.browse_view.is_some()
    }

    fn open_screen(&mut self, target: ViewTarget, core: &mut CoreCtx) -> (Dispatch, Report) {
        open_wim_screen(target, core, self)
    }

    fn move_selection(&mut self, step: SelectStep, core: &mut CoreCtx) -> (Dispatch, Report) {
        move_wim_selection(step, core, self)
    }

    fn go_browse_row(&mut self, core: &mut CoreCtx) -> (Dispatch, Report) {
        let session = self
            .browse_view
            .as_ref()
            .map(browse_session)
            .unwrap_or_default();
        match start_session(session, true, core, self) {
            Ok(status) => op_note(status),
            Err(why) => op_fail(why),
        }
    }

    fn open_browse(&mut self, item: &SearchItem, _core: &mut CoreCtx) -> (Dispatch, Report) {
        // the view itself (title, and "loading..." until the tracks
        // land) is the confirmation; no status line echo. nothing
        // here waits on the network: the loader answers at once and
        // fills the view in from `Loader::poll`.
        open_browse(
            item,
            self.browse_loader,
            self.browse_view,
            self.browse_art,
            self.focus,
        );
        op_done()
    }

    fn bar_field(&self) -> Option<Field> {
        Some(self.bottombar_selected)
    }

    fn flash(&mut self, field: Field) {
        *self.flash = Some((field, Instant::now()));
    }

    fn minimize_player(&mut self, minimize: bool) {
        *self.player_minimized = minimize;
    }

    fn queue_cursor(&mut self) -> Option<&mut usize> {
        Some(self.queue_selected)
    }

    fn close_console(&mut self) {
        *self.console_open = false;
    }
}

/// The selection `up()`/`down()`/`top()`/`bottom()` move is whichever
/// pane owns the keyboard, exactly as j/k and gg/G do: a focused
/// browse view, suggestions menu or queue view owns its own cursor,
/// otherwise it is the sidebar's results list.
fn move_wim_selection(
    step: SelectStep,
    core: &mut CoreCtx,
    surf: &mut WimSurface,
) -> (Dispatch, Report) {
    if *surf.focus == Focus::Workspace {
        if let Some(bv) = surf.browse_view.as_mut() {
            match step {
                SelectStep::By(d) => {
                    for _ in 0..d.unsigned_abs() {
                        if d < 0 {
                            bv.prev();
                        } else {
                            bv.next();
                        }
                    }
                }
                SelectStep::Top if bv.albums_focused => bv.albums_selected = 0,
                SelectStep::Bottom if bv.albums_focused => {
                    bv.albums_selected = bv.albums.len().saturating_sub(1);
                }
                SelectStep::Top => {
                    bv.selected = bv.visible_track_indices().first().copied().unwrap_or(0);
                }
                SelectStep::Bottom => {
                    bv.selected = bv.visible_track_indices().last().copied().unwrap_or(0);
                }
            }
            return op_done();
        }
        if let Some(sug) = surf.suggestions.as_ref() {
            let last = ui::suggestions::count(sug).saturating_sub(1) as i64;
            let at = *surf.suggestions_selected as i64;
            let to = match step {
                SelectStep::By(d) => (at + i64::from(d)).clamp(0, last),
                SelectStep::Top => 0,
                SelectStep::Bottom => last,
            };
            *surf.suggestions_selected = to as usize;
            return op_done();
        }
        if *surf.queue_view_open {
            let order = ui::queue::selectable(core.queue.entries(), core.queue.pos());
            if order.is_empty() {
                return op_fail("the queue is empty");
            }
            let at = order
                .iter()
                .position(|i| *i == *surf.queue_selected)
                .unwrap_or(0) as i64;
            let last = order.len() as i64 - 1;
            let to = match step {
                SelectStep::By(d) => (at + i64::from(d)).clamp(0, last),
                SelectStep::Top => 0,
                SelectStep::Bottom => last,
            };
            *surf.queue_selected = order[to as usize];
            return op_done();
        }
    }
    match step {
        SelectStep::By(d) => core.app.move_selection(d as isize),
        SelectStep::Top => core.app.select_top(),
        SelectStep::Bottom => core.app.select_bottom(),
    }
    op_done()
}

/// `view(target)`: the one screen-opening op. Every target writes the
/// same state its own keybind writes; the four listings and `help`
/// print into the scrollback instead and leave the console up (see
/// `ViewTarget::is_screen`).
fn open_wim_screen(
    target: ViewTarget,
    core: &mut CoreCtx,
    surf: &mut WimSurface,
) -> (Dispatch, Report) {
    match target {
        ViewTarget::Now => {
            if core.now_playing.is_none() {
                return op_fail("nothing is playing");
            }
            *surf.player_minimized = false;
            op_done()
        }
        ViewTarget::Suggestions => {
            let starred = suggestion_starred_keys(core.remote_starred, core.favorites);
            let catalog = suggest::Catalog::from_corpus(&core.app.corpus);
            let guard = core.cache.lock().expect("cache mutex poisoned");
            let suggestions = suggest::suggest(&suggest::Request::new(
                &catalog,
                &guard,
                &starred,
                cache::now_secs(),
            ));
            drop(guard);
            let total = ui::suggestions::count(&suggestions);
            *surf.suggestions = Some(suggestions);
            *surf.suggestions_selected = 0;
            *surf.focus = Focus::Workspace;
            op_note(format!("{total} suggestions"))
        }
        ViewTarget::Settings => {
            *surf.settings_menu_open = true;
            op_done()
        }
        ViewTarget::Keybinds => {
            *surf.keybinds_menu_open = true;
            *surf.keybinds_menu_selected = 0;
            op_done()
        }
        ViewTarget::Eq => {
            *surf.eq_menu_open = true;
            *surf.eq_menu = ui::eq_menu::EqMenuState::default();
            *surf.eq_menu_selected = ui::eq_menu::opening_selection(
                core.app.eq_source,
                core.cfg.eq_custom_presets.len(),
            );
            op_done()
        }
        ViewTarget::Undo => {
            if core.undo.is_empty() {
                // an empty tree is a blank screen that reads as
                // broken; saying so outright is the whole answer.
                return op_fail("nothing has been done yet");
            }
            *surf.undo_menu_open = true;
            op_done()
        }
        ViewTarget::Lyrics => open_wim_lyrics(core),
        // `is_screen()` routes the five listings to
        // `core::print_listing` before this is called; honoured here
        // too rather than unreachable, so the two cannot drift.
        ViewTarget::Queue
        | ViewTarget::Index
        | ViewTarget::History
        | ViewTarget::Playlists
        | ViewTarget::Vars
        | ViewTarget::Bindings
        | ViewTarget::Help => print_listing(target, core),
        ViewTarget::Starred => {
            let live = core.app.corpus.remote_clients();
            // the loader owns this view now, so its own generation bump
            // disowns anything still in flight; no separate invalidate.
            let bv = open_starred(
                surf.browse_loader,
                &core.cfg.navidrome_servers,
                core.favorites,
                &live,
            );
            let count = bv.tracks.len();
            *surf.browse_view = Some(bv);
            // same as opening an album: the listing draws its own
            // cursor, so j/k have to move it rather than the hidden
            // sidebar behind it.
            *surf.focus = Focus::Workspace;
            op_note(format!("{count} starred"))
        }
    }
}

/// `view(lyrics)`: asks for the same fetch the lyrics keybind does and
/// returns at once; the reply opens the pane from the main loop's
/// drain, so the console gets "fetching" rather than a frozen ui.
fn open_wim_lyrics(core: &mut CoreCtx) -> (Dispatch, Report) {
    let Some(item) = core.now_playing.clone() else {
        return op_fail("nothing is playing");
    };
    let Some((artist, title, album)) = lyrics_query(&item) else {
        return op_fail("no lyrics for this item");
    };
    let name = label(&item);
    core.lyrics_loader.request(
        core.lyrics_client,
        name.clone(),
        &artist,
        &title,
        &album,
        None,
    );
    op_note(format!("fetching lyrics for {name}"))
}

/// How often the main loop re-sweeps the download cache for stale
/// (unplayed past `cfg.cache_eviction_days`) files, on top of the
/// sweep that always runs once at startup; cheap local filesystem
/// work, not worth its own background thread the way network sync is.
const CACHE_SWEEP_INTERVAL: Duration = Duration::from_secs(3600);

/// `dir`: -1.0 or 1.0. One step per field, matching `ui::subtitle_menu`'s
/// field order; kept as a plain index match rather than a generic
/// field-descriptor table since there are only seven of these and
/// their underlying types (f64/u32/f32) don't share one anyway.
fn adjust_subtitle_field(sub: &mut config::SubtitleConfig, field: usize, dir: f64) {
    match field {
        0 => sub.delay_secs = (sub.delay_secs + dir * 0.5).clamp(-30.0, 30.0),
        1 => {
            sub.font_size = (sub.font_size as i64 + (dir * 2.0) as i64).clamp(10, 100) as u32;
        }
        2 => {
            sub.position = (sub.position as i64 + (dir * 5.0) as i64).clamp(0, 150) as u32;
        }
        3 => sub.outline_size = (sub.outline_size + dir * 0.5).clamp(0.0, 5.0),
        4 => sub.background_opacity = (sub.background_opacity + dir * 0.1).clamp(0.0, 1.0),
        5 => sub.blur = (sub.blur + dir * 0.5).clamp(0.0, 10.0),
        6 => sub.color_level = (sub.color_level + dir as f32 * 0.05).clamp(0.0, 1.0),
        _ => {}
    }
}

/// Warns once, right after a profile is saved, that the password it
/// just persisted is cleartext. Typing credentials into the app is the
/// only way it can store them (subsonic auth needs the cleartext for
/// every request), so this isn't a thing to fix silently; it's a thing
/// the user has to know in order to choose `password_file` instead.
fn warn_plaintext_password(cfg: &config::Config, notifications: &mut Notifications) {
    let path = config::config_path()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| "config.toml".to_string());
    let _ = cfg;
    notifications.push(
        Severity::Alert,
        format!(
            "password saved in cleartext (0600) at {path}; use password_file for a secret manager"
        ),
    );
}

fn navidrome_config_from_form(form: &LoginForm) -> config::NavidromeConfig {
    config::NavidromeConfig {
        url: form.url.clone(),
        username: form.username.clone(),
        password: Some(form.password.clone()),
        password_file: None,
        max_bitrate_kbps: None,
        transcode_format: None,
    }
}

/// What an empty corpus is still waiting on, or `None` once both the
/// local scan and the first server sync have reported in. Startup fills
/// the corpus from two background threads, so "nothing found" is only
/// honest after both have answered; until then this says which. Kept
/// short enough to fit the sidebar's width without being truncated
/// (see `ui::results_list`).
fn loading_message(scanning_library: bool, syncing_servers: bool) -> Option<&'static str> {
    match (scanning_library, syncing_servers) {
        (true, true) => Some("loading library..."),
        (true, false) => Some("scanning local library..."),
        (false, true) => Some("connecting to navidrome..."),
        (false, false) => None,
    }
}

/// What the results list should say instead of rendering nothing when
/// there's nothing to show; a blank screen with no explanation is a
/// dead end, never the right default.
fn empty_results_message(app: &app::App) -> String {
    if app.corpus.is_empty() {
        loading_message(app.scanning_library, app.syncing_servers)
            .unwrap_or(
                "no local music/video found: check library_paths in config, or set up navidrome (n)",
            )
            .to_string()
    } else if app.editing_query && !app.query.is_empty() {
        format!("no matches for \"{}\"", app.query)
    } else {
        "press / to search".to_string()
    }
}

fn rebuild_corpus(cfg: &config::Config) -> search::Corpus {
    let mut corpus = search::Corpus::new();
    corpus.add_local_tracks(library::scan(&cfg.library_paths));
    corpus
}

/// `KeyAction::DownloadSelected`, for both the browse view and the
/// sidebar, so the two cannot drift.
///
/// An album or artist expands through a network fan-out, so it goes to
/// the resolver and the batch arrives in the drain; a single song
/// starts downloading in this keypress.
///
/// Only speaks up when nothing started. Each started track pushes its
/// own progress toast, so saying "downloading 3 track(s)" on top of
/// three progress bars is noise, but "already cached" has nothing else
/// to show it. Both of these sites used to discard the message, so
/// asking to download something already on disk looked like a dead
/// key.
fn download_selected(
    item: &SearchItem,
    resolver: &mut TrackResolver,
    dctx: &mut DownloadCtx,
    notifications: &mut Notifications,
) {
    match resolver.request(item, Intent::Download) {
        Resolution::Ready(tracks) => {
            let done = download_tracks(&tracks, dctx, notifications);
            if done.started == 0 {
                notifications.push(Severity::Info, done.status);
            }
        }
        Resolution::Fetching(label) => {
            notifications.push(Severity::Info, format!("resolving {label}..."));
        }
    }
}

/// The commit this binary was built from, stamped by build.rs.
const BUILD: &str = env!("WIM_BUILD");

fn main() -> anyhow::Result<()> {
    // answered before anything else, so it works with no terminal, no
    // config and no server: `--version` exists specifically to settle
    // "am I running the build I think I am", which has cost more time
    // than any bug it was mistaken for.
    if std::env::args()
        .skip(1)
        .any(|a| a == "--version" || a == "-V")
    {
        println!("wim {} ({BUILD})", env!("CARGO_PKG_VERSION"));
        return Ok(());
    }
    trace::init();
    let is_first_run = !config::config_path()?.exists();
    let mut cfg = config::load_or_init()?;
    trace::mark("config loaded");
    // the local scan is the biggest single startup cost (a blocking tag
    // read per music file); it streams into the corpus from its own
    // thread while the ui is already up, drained next to `sync_rx` in
    // the main loop. nothing between here and the first frame waits on
    // it. see docs/architecture.md.
    // `mut` because `rescan()` replaces it with a fresh channel: the
    // scan thread signs off by dropping its sender, so a finished
    // channel can never be reused.
    let (scan_tx, mut scan_rx) = mpsc::channel::<library::ScanBatch>();
    library::spawn_scan(cfg.library_paths.clone(), scan_tx);
    // every configured server's catalog as of its last session, read
    // straight off the disk so the first frame has a full library
    // rather than "connecting to navidrome..."; started here so the
    // read overlaps mpv's spawn below, taken where `app` is built.
    // the background sync reconciles it against what each server
    // really has. see `search::spawn_warm_from_cache`.
    let warming_corpus = search::spawn_warm_from_cache(&cfg.navidrome_servers);

    let player = mpv::Mpv::spawn()?;
    trace::mark("mpv spawned");
    // per-track normalization is the sensible default; mpv reads the
    // gain from each file's own embedded tags as it plays.
    player.set_replaygain("track")?;
    // harmless to set before any video/subtitle is even loaded; mpv
    // just holds onto it for whenever one shows up.
    player.set_subtitle_style(&cfg.subtitles);
    let mut subtitle_delay = cfg.subtitles.delay_secs;
    // every global, single-purpose command key, remappable via 'K';
    // see keymap.rs for which keys deliberately aren't covered
    // (universal navigation primitives, applied the same way
    // everywhere rather than individually named commands).
    let mut keymap = Keymap::from_overrides(&cfg.keybinds);
    let mut app = app::App::new(warming_corpus.take());
    // so the first frame says "scanning" rather than "no local music
    // found"; cleared when the scan thread drops its sender. the
    // server equivalent is set below.
    app.scanning_library = true;
    // restores whichever eq state was last active (a built-in, one of
    // the user's saved curves, either of those nudged since, or a
    // nameless hand-tuned set) by name rather than a raw index, same
    // reasoning as `logo_style`; applied to mpv immediately so a
    // persisted eq (and the level meter's `astats` tap, see
    // `audio_level`) is live from the first frame, not only after the
    // first manual 'e' press. the three-state resolution itself is
    // `eq::restore`, which is pure and unit-tested.
    let restored = eq::restore(
        cfg.eq_preset.as_deref(),
        cfg.eq_gains,
        &cfg.eq_custom_presets,
    );
    app.eq_source = restored.source;
    app.eq_custom = restored.modified;
    app.eq_gains = restored.gains;
    if let Some(eq::Source::Builtin(i)) = restored.source {
        app.eq_preset = i;
    }
    let _ = player.set_af(&eq::build_af_filter(&app.eq_gains));
    // which pane j/k/arrows/enter navigate: the sidebar's own results
    // list, the workspace (whatever's actually open there), or the
    // bottom bar's own controls. only ever changed from
    // `InputMode::Move` (see the input-handling loop below), never as
    // a side effect of some other key.
    let mut focus = Focus::Sidebar;
    let mut input_mode = InputMode::Interact;
    let mut favorites = favorites::load();
    // song ids this session knows are starred, across every configured
    // server; arrives with each background sync result and is kept
    // current on every manual toggle. unioned rather than replaced, so
    // a toggle can't be clobbered by a fetch that was already in
    // flight when it happened.
    let mut remote_starred: HashSet<String> = HashSet::new();

    // mpv's playlist as an index -> `SearchItem` map; the one queue
    // there is, and what every "what's playing" read resolves through.
    let mut play_queue = PlayQueue::default();
    // folded from mpv's own observed property changes; see
    // `Mpv::observe_playback`. Never polled.
    let mut playback = mpv::Playback::default();
    // covers for tracks the open browse view does not have art for;
    // fetched on its own thread, see `coverart`.
    let mut cover_art = coverart::Cache::new(cfg.cache_eviction_days);
    // the player's own image protocol, separate from the browse
    // view's: they can be showing different albums at once, and
    // sharing one protocol made the player draw whatever album a view
    // had last opened. keyed so it rebuilds only when the cover
    // actually changes, since a protocol holds resize state.
    let mut player_art = ProtocolCache::default();
    // whatever was playing right before `now_playing` last changed;
    // purely for the player view's "previous: ..." line.
    let mut previous_track: Option<SearchItem> = None;
    let mut scrobble = ScrobbleState::default();
    // false (full player) by default for a standalone play; starting
    // playback from an open album/playlist sets this true instead, so
    // the track listing stays visible rather than getting replaced.
    // 'm' toggles it by hand either way.
    let mut player_minimized = false;
    // 'i' cycles ascii-bw -> ascii-color -> full-bw -> full-color ->
    // off from here; this is just the config-driven starting point,
    // with the graphics probe still out (so never a full mode yet, see
    // `art_mode_for_picker`). kept around so the probe landing can only
    // upgrade a mode the user hasn't since cycled by hand.
    let art_mode_pending = art_mode_for_picker(cfg.album_art, false);
    let mut art_mode = art_mode_pending;
    // one alert, not one per keypress, when `i` reaches a real mode
    // the terminal cannot render.
    let mut warned_no_graphics = false;
    // latest captured frame while playing a local video, rendered
    // through the same ascii pipeline as album art instead of mpv
    // opening its own window; `None` whenever nothing video is
    // playing or a capture hasn't landed yet. 'v' expands its pane
    // from main_content to the whole workspace (sidebar included),
    // never the bottom bar.
    // 'v': hands the whole terminal to mpv, which draws the video with
    // a terminal video output of its own while wim stops drawing
    // entirely. `video_vo` is the vo mpv is drawing with *right now*
    // (`None` = mpv is back on `vo=null` and the screen is wim's), and
    // `video_geometry` the cell size that vo was initialized at, since
    // mpv only reads it on init. see docs/video.md.
    let mut video_fullscreen = false;
    let mut video_vo: Option<video::Vo> = None;
    let mut video_geometry: (u16, u16) = (0, 0);
    // full keybind reference, opened with '?'; takes over the whole
    // screen the same way onboarding/the nav manager do.
    let mut help_open = false;
    // the table is taller than a 24-row terminal, so it scrolls rather
    // than letting its last column overflow and be clipped (which ate
    // the section holding how to quit). Reset on open; clamped against
    // `help_scroll_max`, which only the draw knows.
    let mut help_scroll: usize = 0;
    let mut help_scroll_max: usize = 0;
    // 't': a small centered box over whatever's already on screen
    // (not a full takeover like help_open), for live subtitle style
    // tweaks instead of only the ,/. delay-only adjustment.
    let mut subtitle_menu_open = false;
    let mut subtitle_menu_selected: usize = 0;
    // 'u': the discoverable "where are the app's own settings" home,
    // covering the whole workspace like the video pane's fullscreen
    // mode does, never the bottom bar.
    let mut settings_menu_open = false;
    let mut settings_menu_selected: usize = 0;
    // text-entry sub-mode for the settings menu's "listenbrainz
    // token" row (the one field there that isn't h/l-adjustable
    // numeric); enter on that row starts it, enter commits, esc
    // cancels without changing anything.
    let mut settings_token_editing = false;
    let mut settings_token_input = String::new();
    // 'K': not itself remappable, deliberately (remapping the key
    // that opens the remap menu away and forgetting the new one
    // would be a real way to lock yourself out of it; '?' help
    // always shows it regardless). `capturing_key` is Some while
    // waiting for the next keypress to bind to the selected action.
    let mut keybinds_menu_open = false;
    let mut keybinds_menu_selected: usize = 0;
    let mut capturing_key = false;
    let mut keybind_conflict: Option<KeyAction> = None;
    // left/right cycles through ui::banner::LOGOS while the workspace
    // has focus and the banner is what's showing; '0' resets to the
    // default (index 0, ANSI Shadow). Restored from config by name,
    // not a raw index (see `Config::logo_style`'s own reasoning).
    let mut logo_index = ui::banner::logo_index_for_name(cfg.logo_style.as_deref());
    // rolling window for the now-playing level meter; a fixed-size
    // ring (just truncate the front) rather than growing forever.
    let mut level_history: Vec<f32> = Vec::new();
    let mut notifications = Notifications::default();
    // track play counts/last-played/on-disk cache state (cache.rs);
    // shared with background download threads, so it's behind a
    // mutex rather than owned outright the way `app`/`player` are.
    let cache_index = Arc::new(Mutex::new(cache::CacheIndex::load()));
    sweep_cache(&cache_index, cfg.cache_eviction_days);
    let mut last_cache_sweep = Instant::now();
    // per-track download progress, reported back from whichever
    // thread `download::spawn_tracks` spawned (see
    // `spawn_background_sync` for the same pattern applied to
    // navidrome sync instead); polled every frame, same as `sync_rx`.
    let (download_tx, download_rx) = mpsc::channel::<DownloadEvent>();
    // track ids with a download already in flight, so a second play
    // crossing the threshold (or a second manual download press)
    // doesn't kick off a duplicate transfer of the same file.
    let mut downloading: HashSet<String> = HashSet::new();
    let lyrics_client = lyrics::Client::new();
    // the fetch runs off-thread; the key handler only asks.
    let mut lyrics_loader = lyrics::Loader::new();
    // a refused star comes back here so the optimistic flip can be put
    // back; see `toggle_star`.
    let (star_tx, star_rx) = mpsc::channel::<StarFailed>();
    let mut track_resolver = TrackResolver::new();
    // the shellsong console: ':' opens it, and while it is open it
    // claims every key and every paste (see its input tier, which sits
    // at the same tier `editing_query` does and for the same reason).
    // Built once rather than per open, so the scrollback, the history
    // and the completion ranking all survive closing it; `Console::new`
    // is also the only thing here that touches the disk.
    let mut console = Console::new();
    let mut console_open = false;
    // the program a submitted line parsed into, held across frames:
    // a command that searches blocks on `AwaitResults` until the host
    // says the results settled, so it cannot run in one pass.
    let mut shellsong_program: Option<Box<shellsong::Program>> = None;
    // `load_queue`'s completion candidates, snapshotted rather than
    // read off the disk on every keystroke.
    let mut saved_queues: Vec<String> = Vec::new();
    // the `let name = [...]` bindings, loaded once: a submitted line is
    // parsed against them, so they are read per line rather than per
    // keystroke, and `define`/`unset` write the file back.
    let mut shellsong_vars = shellsong_host::load_vars();
    // `run(...)`'s completion candidates, snapshotted on console open
    // for the same reason the saved queues are.
    let mut shellsong_scripts: Vec<String> = Vec::new();
    // the per-identity settings, and what they are currently
    // overriding so it can be put back when an unbound track starts.
    let mut shellsong_bindings = shellsong_host::load_bindings();
    let mut shellsong_overridden = shellsong_host::Overridden::default();
    // ops the host itself asked for, waiting for the step loop so they
    // go through `apply_shellsong_op` like everything else. One slot:
    // a newer track change supersedes an older pending settle.
    let mut shellsong_queued: Option<Box<shellsong::Program>> = None;
    // `~/.config/wim/init.shg`, if there is one, as the first program
    // the step loop drives. Spelled as the language rather than wired
    // specially: it is one `run(init)` line through the same parse and
    // the same stepper, so a startup script and a typed one cannot
    // behave differently.
    if shellsong_host::has_init_script() {
        let read = |name: &str| shellsong_host::read_script(name);
        let world = shellsong::World {
            scope: &shellsong_vars,
            read: Some(&read),
        };
        let line = format!("run({})", shellsong_host::INIT_SCRIPT);
        match shellsong::Program::parse_in(&line, &world) {
            Ok(program) => shellsong_program = Some(Box::new(program)),
            // into the scrollback rather than a toast: it is a
            // shellsong error and `:` is where you go to read them.
            Err(e) => console.fail(format!("{}.shg: {e}", shellsong_host::INIT_SCRIPT)),
        }
    }
    // backs `Env::search_settled`; see `shellsong_host::SearchSettle`.
    let mut settle = SearchSettle::default();
    // the queue view owns main_content like the browse/lyrics views do;
    // its cursor is separate from playback, so scrolling it never moves
    // what mpv is playing.
    let mut queue_view_open = false;
    let mut queue_selected: usize = 0;
    // computed when the menu opens and again when a background
    // enrichment answer lands, never per frame: the engine walks the
    // whole corpus and the play history, which is not something to
    // redo sixty times a second for a list that only changes when its
    // inputs do.
    let mut suggestions: Option<suggest::Suggestions> = None;
    let mut suggestions_selected: usize = 0;
    // the engine's view of the library, kept across menu opens because
    // the background fetches land in here; rebuilding it from the
    // corpus per open would throw every fetched tracklist away.
    let mut suggest_catalog = suggest::Catalog::new();
    // getArtistInfo2/getTopSongs answers from previous sessions as
    // well as this one, negative answers included.
    let mut enrichment = suggest::EnrichmentCache::load();
    // album ids already asked about this session; see
    // `next_enrichment_job` for why this is not persisted.
    let mut warmup = Warmup::default();
    let mut lyrics = LyricsPane::default();
    // where the last frame put everything, filled in by the draw pass
    // below and read by the next mouse event; same reason
    // `LyricsPane::saw_width` is written from in there, taken all the way.
    // Declared without a value because every iteration replaces it
    // before anything reads it. See `hit` and
    // docs/input-precedence.md.
    let mut hits: hit::HitMap;
    // opened via enter on an album/playlist; takes over main_content
    // until esc, same modal-ish precedence as lyrics but independent
    // of it (browsing doesn't require anything to already be playing).
    let mut browse_view: Option<browse::BrowseView> = None;
    // fills `browse_view` in from its worker threads, so opening one
    // costs no frames; drained once a pass below.
    let mut browse_loader = browse::Loader::default();

    // tracks the last known reachability per server url, so the
    // background sync below only notifies on an actual transition
    // (going offline, coming back), not every single retry while a
    // server stays down.
    let mut server_reachable: HashMap<String, bool> = HashMap::new();
    // every configured server is connected by the background sync
    // thread, never here: a round-trip per server before the first
    // frame meant a stalled server could hold the whole ui at a black
    // screen indefinitely. all of them feed the same unified corpus,
    // there's no "active server" concept to pick between (consistent
    // with search spanning everything), and each one's reachability is
    // reported as it lands (see the `sync_rx` drain). 'I' shows a full
    // local/per-server breakdown on demand instead of announcing a
    // count unprompted on every startup.
    app.syncing_servers = !cfg.navidrome_servers.is_empty();

    // background re-sync, which also does the initial connect; keeps
    // navidrome live without a manual refresh key ("constantly
    // updating" per the spec).
    let (sync_tx, sync_rx) = mpsc::channel::<BackgroundSyncResult>();
    let (servers_tx, servers_rx) = mpsc::channel::<Vec<config::NavidromeConfig>>();
    spawn_background_sync(cfg.navidrome_servers.clone(), sync_tx, servers_rx);

    // subtitles found by `fire_subtitle_fetch`'s thread, loaded into
    // mpv here (it owns the handle); polled every frame same as
    // `sync_rx`.
    let (subtitle_tx, subtitle_rx) = mpsc::channel::<SubtitleReady>();

    // live remote search (song titles especially; see
    // `spawn_background_search`'s own doc) debounced off query
    // changes rather than a timer, polled every frame same as `sync_rx`.
    let (remote_search_tx, remote_search_rx) = mpsc::channel::<RemoteSearchQuery>();
    let (remote_search_result_tx, remote_search_result_rx) = mpsc::channel::<RemoteSearchOutcome>();
    spawn_background_search(remote_search_rx, remote_search_result_tx);

    // the suggestions menu's getAlbum/getArtistInfo2/getTopSongs
    // fetches, which are what let four of the nine generators fire at
    // all; jobs out, answers back, polled every frame same as
    // `sync_rx`. See `spawn_background_enrichment`.
    let (enrich_job_tx, enrich_job_rx) = mpsc::channel::<EnrichmentJob>();
    let (enrich_tx, enrich_rx) = mpsc::channel::<EnrichmentEvent>();
    spawn_background_enrichment(enrich_job_rx, enrich_tx);

    // 'n' opens the profile manager standalone, later.
    let mut nav_manager: Option<NavManager> = None;

    // first run gets the full guided sequence (welcome, controls,
    // navidrome, library paths) instead of a disconnected popup with
    // no context; a hand-edited config.toml before the app is even
    // usable once was a bad first impression.
    let mut onboarding: Option<Onboarding> = if is_first_run {
        let defaults = if cfg.library_paths.is_empty() {
            library::default_library_paths()
        } else {
            cfg.library_paths.clone()
        };
        Some(Onboarding::new(defaults))
    } else {
        None
    };

    // a restored queue goes into mpv's own playlist one blocking ipc
    // round-trip per track, so the appends wait until the first frame
    // is up (see the drain right after the first `draw`); `app.queue`
    // itself is filled here, since that's pure in-memory work.
    let mut pending_enqueue: Vec<SearchItem> = Vec::new();

    // resuming across restarts is standard in any serious media player;
    // a missing/corrupt state file just means nothing to resume, not an
    // error; see persistence.rs.
    if let Some(saved) = persistence::load() {
        if let Some(item) = saved
            .now_playing_path
            .as_deref()
            .and_then(persistence::item_from_path)
        {
            let (_, played) = load_playlist(
                &player,
                &cache_index,
                &mut play_queue,
                vec![item.clone()],
                SessionStart::New,
            );
            if played {
                if let Some(pos) = saved.position_secs {
                    let _ = player.seek(pos, false);
                }
                fire_subtitle_fetch(&cfg, &subtitle_tx, &item);
                let mut dctx = DownloadCtx {
                    cache: &cache_index,
                    download_tx: &download_tx,
                    downloading: &mut downloading,
                    day_threshold: cfg.cache_days_threshold,
                };
                note_playback_started(&item, &mut dctx, &mut notifications);
                start_tracking_playback(&cfg, &item, &mut scrobble);
            }
        }
        for queued_path in &saved.queue_paths {
            if let Some(item) = persistence::item_from_path(queued_path) {
                pending_enqueue.push(item);
            }
        }
        if let Some(vol) = saved.volume {
            let _ = player.set_volume(vol);
        }
    }
    trace::mark("playback state restored (queue deferred)");

    // only now that any restored track is already loaded: registering
    // an observer makes mpv report the property's *current* value
    // immediately, and doing it before the restore would have the first
    // `idle-active` describe an mpv that hadn't started playing yet.
    if let Err(e) = player.observe_playback() {
        notifications.push(
            Severity::Error,
            format!("mpv won't report playback position: {e}"),
        );
    }

    // the ceiling crossfade's fade-out/fade-in ramps scale against;
    // read back rather than assuming 100, since persistence may have
    // just restored something else above. `mut` because shellsong's
    // `volume(...)` moves the ceiling itself, or the next fade-in
    // would snap back to whatever it was at startup.
    let mut base_volume = player
        .get_property("volume")
        .ok()
        .and_then(|v| v.as_f64())
        .unwrap_or(100.0);
    let mut crossfade_enabled = cfg.crossfade_enabled;
    // Some(when the current track started) while fading in; mpv has
    // no notion of this, so it's tracked entirely on our side and
    // cleared once the fade finishes.
    let mut fade_in_started_at: Option<Instant> = None;
    // the between-track gap, and the last `keep-open` wim wrote. the
    // cache is what makes evaluating it every frame free: mpv only
    // hears about it when the answer actually changes.
    let mut between_tracks = BetweenTracks::new(cfg.gap_seconds);
    let mut resume_due: Option<Duration> = None;
    let mut undo_tree = wim::undo::Recorder::default();
    // shellsong's `fade(in|out)` ramp and `sleep(...)` deadline, both
    // off the same `start.elapsed()` clock `Env::now` carries so an op
    // and the tick that finishes it agree about the time. They reuse
    // the crossfade ramp's shape rather than cutting the audio dead.
    let mut shellsong_fade: Option<Ramp> = None;
    let mut sleep_due: Option<Duration> = None;
    // Which ControlsBar control most recently changed, and when;
    // render computes a decaying brightness from the elapsed time
    // rather than a one-off status sentence, so a toggle just lights
    // up the glyph it already shows permanently.
    let mut flash: Option<(Field, Instant)> = None;
    const FLASH_DURATION: Duration = Duration::from_millis(700);
    // `Focus::BottomBar`'s own selection cursor; independent of
    // `flash`, a steady cursor position rather than a transient
    // acknowledgment, but both feed `ControlsBar`'s rendering.
    let mut bottombar_selected = Field::Pause;
    // the maximized player's own cursor over the same `Field` set: one
    // alphabet, one enter dispatch, one scrub for both surfaces, but a
    // position each so moving focus doesn't drag the other's cursor
    // along with it.
    let mut player_selected = Field::Pause;
    // `Some` while enter-on-the-scrubber holds a drag open: arrows
    // move `Scrub::target_secs` locally and the absolute seek is
    // coalesced behind it, so holding one down can't spend an ipc
    // round-trip per keypress. `scrub_pending` is a target the throttle
    // hasn't sent yet; the loop flushes it before the next poll.
    let mut scrub: Option<Scrub> = None;
    let mut scrub_pending = false;
    let mut scrub_sent_at: Option<Instant> = None;
    const SCRUB_SEEK_INTERVAL: Duration = Duration::from_millis(80);
    // enter on the bottom bar's Eq field opens this instead of just
    // cycling presets like the bare 'e' key does; full-workspace, same
    // treatment as the settings/keybinds menus.
    let mut eq_menu_open = false;
    let mut undo_menu_open = false;
    let mut eq_menu_selected: usize = 0;
    // the eq menu's name editor, armed delete and message line; the
    // selection stays a loose `usize` above because the shellsong
    // `view(eq)` path writes it too.
    let mut eq_menu = ui::eq_menu::EqMenuState::default();

    let mut panels = PanelLayout::from_sizes(cfg.sidebar_width, cfg.bottom_bar_height);
    let start = Instant::now();
    let mut guard = term::TerminalGuard::new(cfg.mouse)?;
    // queries the terminal over stdio for font-cell size and graphics
    // protocol support; best-effort, 'i' (real image mode) just stays
    // unavailable if this fails rather than erroring the whole app.
    // on its own thread because the terminal may simply never answer,
    // in which case ratatui-image waits out a full second: the single
    // largest thing the first frame used to sit behind. spawned after
    // raw mode is already on, since the probe saves and restores
    // whatever termios state it finds.
    let (picker_tx, picker_rx) = mpsc::channel::<Option<Picker>>();
    if graphics_plausible() {
        thread::spawn(move || {
            let _ = picker_tx.send(Picker::from_query_stdio().ok());
            trace::mark("graphics probe answered");
        });
    } else {
        // skipped, not backgrounded: the probe reads stdin waiting for
        // an answer, so on a terminal that will never send one it sits
        // there for a full second racing the main loop for input and
        // eating whatever arrives, mouse reports included. not probing
        // at all is both faster and the only way the event loop is the
        // single reader of stdin.
        let _ = picker_tx.send(None);
        trace::mark("graphics probe skipped (terminal has no protocol)");
    }
    let mut picker: Option<Picker> = None;
    let mut picker_probed = false;
    trace::mark("terminal ready");
    // rebuilt (not re-encoded every frame) whenever the open view's
    // art or the art mode changes; `ratatui-image` owns the actual
    // resize/encode caching per protocol instance. `browse_art_key`
    // names what the protocol currently holds, cover and tone both.
    let mut browse_art = ProtocolCache::default();
    // the display's refresh rate decides how often an animation is
    // sampled (never how fast it runs, see `anim`); one subprocess on
    // a thread, applied whenever it answers, so the first frame never
    // waits on a compositor. 60hz until then.
    let frame_rx = anim::spawn_probe();
    let mut frame = anim::FrameBudget::default();
    // everything that costs an ipc round-trip per track runs on the
    // first pass through the loop instead of before it; see the drain
    // right after `draw`.
    let mut first_frame_drawn = false;
    loop {
        notifications.expire();
        // once per frame, not once per widget that wants it: the
        // scrubber in the bottom bar, the mini bar and the full player
        // all read the same position, and each call is three ipc
        // round-trips.
        // one ioctl per frame instead of one per widget that wants it:
        // five render sites asked independently, and the answer cannot
        // change inside a single frame.
        let aspect = cell_aspect();
        // no ipc here any more: these arrive as observed property
        // changes on the event channel drained just below.
        let (elapsed_secs, paused) = (playback.elapsed_secs, playback.paused);
        // re-derived every frame and never kept across one: whatever
        // mpv's last reported `playlist-pos` resolves to. Everything
        // downstream (title, mini bar, lyrics header, scrobbling)
        // reads this, so none of them can show a track mpv isn't
        // actually playing.
        let mut now_playing = play_queue.current().cloned();
        // hoisted from further down with the macros below, which
        // borrow it: `start.elapsed()` is free to read early.
        let shellsong_now = start.elapsed();
        // defined at the top of the loop body rather than beside
        // their first use: a `macro_rules!` is only in scope after
        // its definition, and the event drain above wants them too.
        // every local they borrow is declared before the loop.
        macro_rules! core_ctx {
            () => {
                CoreCtx {
                    cfg: &cfg,
                    player: &player,
                    app: &mut app,
                    notifications: &mut notifications,
                    queue: &mut play_queue,
                    previous_track: &mut previous_track,
                    cache: &cache_index,
                    download_tx: &download_tx,
                    downloading: &mut downloading,
                    scrobble: &mut scrobble,
                    subtitle_tx: &subtitle_tx,
                    console: &mut console,
                    now: shellsong_now,
                    now_playing: &now_playing,
                    favorites: &mut favorites,
                    remote_starred: &mut remote_starred,
                    lyrics_client: &lyrics_client,
                    lyrics_loader: &mut lyrics_loader,
                    star_tx: &star_tx,
                    track_resolver: &mut track_resolver,
                    base_volume: &mut base_volume,
                    crossfade_enabled: &mut crossfade_enabled,
                    overridden: &mut shellsong_overridden,
                    undo: &mut undo_tree,
                    between_tracks: &mut between_tracks,
                    fade_in_started_at: &mut fade_in_started_at,
                    fade: &mut shellsong_fade,
                    sleep_due: &mut sleep_due,
                    resume_due: &mut resume_due,
                    scan_rx: &mut scan_rx,
                    remote_search_tx: &remote_search_tx,
                    settle: &mut settle,
                    saved_queues: &mut saved_queues,
                    vars: &mut shellsong_vars,
                    bindings: &mut shellsong_bindings,
                }
            };
        }
        macro_rules! wim_surface {
            () => {
                WimSurface {
                    focus: &mut focus,
                    browse_view: &mut browse_view,
                    browse_loader: &mut browse_loader,
                    browse_art: &mut browse_art,
                    player_minimized: &mut player_minimized,
                    flash: &mut flash,
                    console_open: &mut console_open,
                    queue_selected: &mut queue_selected,
                    queue_view_open: &mut queue_view_open,
                    suggestions: &mut suggestions,
                    suggestions_selected: &mut suggestions_selected,
                    settings_menu_open: &mut settings_menu_open,
                    keybinds_menu_open: &mut keybinds_menu_open,
                    keybinds_menu_selected: &mut keybinds_menu_selected,
                    eq_menu_open: &mut eq_menu_open,
                    eq_menu_selected: &mut eq_menu_selected,
                    eq_menu: &mut eq_menu,
                    undo_menu_open: &mut undo_menu_open,
                    bottombar_selected,
                }
            };
        }

        // read before this frame's `tick` (which runs after the draw),
        // so the countdown is at most one frame behind. it is a
        // seconds-resolution readout, so that is invisible.
        let gap_left = between_tracks
            .remaining(Instant::now())
            .map(|d| d.as_secs_f64());
        // mpv misreports a generated silence's length, so the entry is
        // asked first; see `core::playing_duration`.
        let duration_secs = playing_duration(now_playing.as_ref(), playback.duration_secs);
        // take delivery of any cover that finished, ask for the
        // playing track's if it is not already in hand, and warm the
        // next queued track's. all no-ops once an album's cover has
        // arrived, so this is cheap to run every frame.
        cover_art.tick(now_playing.as_ref(), play_queue.peek_next());
        // which cover the player would draw: the open view's when the
        // playing track is one of its own (already decoded at the
        // right size), else whatever the background fetch has.
        let playing_art: Option<&image::DynamicImage> = now_playing.as_ref().and_then(|item| {
            browse_view
                .as_ref()
                .filter(|bv| {
                    let id = item.identity();
                    bv.tracks.iter().any(|t| t.identity() == id)
                })
                .and_then(|bv| bv.art.as_ref())
                .or_else(|| cover_art.get(item))
        });
        // rebuild either image protocol only when its cover or the art
        // mode changes; every frame would throw away the resize state a
        // protocol exists to hold. the mode is in the key because
        // `full-bw` encodes a greyscaled copy of the picture, so
        // switching tone is as much a new image as a new album is.
        let protocol_key = |key: &str| format!("{key}\u{1}{}", art_mode.label());
        match (&now_playing, playing_art) {
            (Some(item), Some(img)) if art_mode.is_full() => {
                player_art.ensure(
                    picker.as_mut(),
                    protocol_key(&coverart::art_key(item)),
                    || protocol_image(img, art_mode),
                );
            }
            _ => player_art.clear(),
        }
        // the browse view's own protocol, same rule. keyed here rather
        // than rebuilt wherever a view opens: the mode can change with
        // no view change at all ('i'), and four open sites could not
        // agree on that on their own.
        match &browse_view {
            Some(bv) if art_mode.is_full() => {
                // `None` while the view has no art, so a cover that
                // only arrives later is still picked up rather than
                // latched behind a key that never changed.
                match bv.art.as_ref() {
                    Some(img) => {
                        browse_art.ensure(picker.as_mut(), protocol_key(&bv.art_key()), || {
                            protocol_image(img, art_mode)
                        })
                    }
                    // cleared rather than left holding, so a cover
                    // that only arrives later is still picked up.
                    None => browse_art.clear(),
                }
            }
            _ => browse_art.clear(),
        }
        if let Ok(detected) = frame_rx.try_recv() {
            frame = detected;
        }
        // sampled once per frame rather than per widget, so everything
        // on screen animates off the same instant.
        let frame_elapsed = start.elapsed();
        // read here rather than next to the poll timeout that also uses
        // it, because the fullscreen-video decision below needs it and
        // nothing between the two touches any of these flags.
        let overlay = onboarding.is_some()
            || nav_manager.is_some()
            || help_open
            || settings_menu_open
            || keybinds_menu_open
            || eq_menu_open
            // not only for the animation gate: `overlay` also feeds
            // `video::wanted`, and without it mpv would keep drawing
            // a video straight over this modal.
            || undo_menu_open;
        // whether mpv should be drawing the video right now; the whole
        // decision is `video::wanted`, including the guard that keeps
        // music on `vo=null`.
        let want_vo = video::wanted(
            video::Screen {
                fullscreen: video_fullscreen,
                minimized: player_minimized,
                overlay,
                is_video: now_playing_is_video(&now_playing),
            },
            cfg.video_output,
            picker.map(|p| p.protocol_type()),
        );
        let size = guard
            .terminal
            .size()
            .map(|s| (s.width, s.height))
            .unwrap_or(video_geometry);
        if want_vo != video_vo {
            // the handover. nothing is seeked or reloaded: `vo` is
            // settable live and mpv keeps playing straight through it.
            if let Some(vo) = want_vo {
                let armed = player
                    .set_video_geometry(size.0, size.1)
                    .and_then(|()| player.set_video_output(vo.mpv_name()));
                match armed {
                    Ok(()) => {
                        video_vo = Some(vo);
                        video_geometry = size;
                    }
                    Err(_) => {
                        // leave the screen alone and stop asking, rather
                        // than retrying the handover every frame.
                        video_fullscreen = false;
                        notifications.push(Severity::Alert, "video: mpv refused the video output");
                    }
                }
            } else {
                let _ = player.set_video_output(video::Vo::Null.mpv_name());
                video_vo = None;
                let _ = guard.reclaim();
            }
        } else if video_vo.is_some() && size != video_geometry {
            // mpv reads the vo's size once, at init, so a resize has to
            // bounce it; wim is not drawing, so the screen it flickers
            // through belongs to nobody in between.
            if let Some(vo) = want_vo {
                let _ = player.set_video_output(video::Vo::Null.mpv_name());
                let _ = player.set_video_geometry(size.0, size.1);
                let _ = player.set_video_output(vo.mpv_name());
                video_geometry = size;
            }
        }
        // cleared here and refilled by the draw below, so a mouse
        // event is always answered by the frame the user clicked on.
        // `hit::front` resolves the tier from the key cascade's own
        // order; these are the flags that cascade tests, read in one
        // place rather than re-tested per hit.
        hits = hit::HitMap {
            modal: hit::front(hit::Tiers {
                onboarding: onboarding.is_some(),
                nav_manager: nav_manager.is_some(),
                help: help_open,
                console: console_open,
                editing_query: app.editing_query,
                browse_filter: browse_view.as_ref().is_some_and(|bv| bv.filtering),
                subtitle_menu: subtitle_menu_open,
                settings_menu: settings_menu_open,
                eq_menu: eq_menu_open,
                keybinds_menu: keybinds_menu_open,
            }),
            ..Default::default()
        };
        // skipped outright while mpv owns the screen: a repaint from
        // ratatui's buffer would overwrite the video with the ui that
        // was on screen before the handover. `hits` stays as reset
        // above, i.e. empty, which is the right answer for a click on a
        // screen holding no wim widgets at all.
        guard.draw_unless_video(video_vo.is_some(), |f| {
            let area = f.area();

            // onboarding takes the whole screen; no divider/status bar
            // competing with a guided first-run sequence.
            if let Some(ob) = &onboarding {
                match ob.step {
                    Step::Welcome => f.render_widget(WelcomeView, area),
                    Step::Controls => f.render_widget(ControlsView { keymap: &keymap }, area),
                    Step::Navidrome => {
                        f.render_widget(
                            LoginView {
                                form: &ob.login,
                                message: &ob.login_message,
                                step: Some(ui::onboarding::NAVIDROME_STEP),
                            },
                            area,
                        );
                    }
                    Step::ListenBrainz => {
                        f.render_widget(
                            ListenBrainzView {
                                input: &ob.listenbrainz_token_input,
                            },
                            area,
                        );
                    }
                    Step::LibraryPaths => {
                        f.render_widget(
                            LibraryPathsView {
                                paths: &ob.library_paths,
                                input: &ob.path_input,
                            },
                            area,
                        );
                    }
                    Step::Done => f.render_widget(DoneView, area),
                }
                f.render_widget(
                    NotificationOverlay {
                        items: notifications.visible(),
                    },
                    area,
                );
                return;
            }

            // nav manager and login are brief setup flows, same
            // full-screen-modal treatment as onboarding; they don't
            // need the sidebar visible while active.
            if let Some(nm) = &nav_manager {
                match &nm.step {
                    NavManagerStep::List => {
                        f.render_widget(
                            NavManagerListView {
                                servers: &cfg.navidrome_servers,
                                selected: nm.selected,
                                message: &nm.message,
                                connection_status: &nm.connection_status,
                            },
                            area,
                        );
                    }
                    NavManagerStep::Add(form) => {
                        f.render_widget(
                            LoginView {
                                form,
                                message: &nm.message,
                                step: None,
                            },
                            area,
                        );
                    }
                }
                f.render_widget(
                    NotificationOverlay {
                        items: notifications.visible(),
                    },
                    area,
                );
                return;
            }

            if help_open {
                let overlay = ui::help::HelpOverlay {
                    keymap: &keymap,
                    mouse: cfg.mouse,
                    scroll: help_scroll,
                };
                // recorded off the frame being drawn, same contract as
                // the hit map: the next key or notch is clamped against
                // the layout the user was actually looking at.
                help_scroll_max = overlay.max_scroll(area);
                help_scroll = help_scroll.min(help_scroll_max);
                f.render_widget(overlay, area);
                return;
            }

            // the full player replaces whatever else main_content was
            // showing; minimized just means "let the album view (if
            // any) or the banner show instead", with a thin ipod-style
            // strip taking its place right above the main bottom bar.
            let show_full_player = now_playing.is_some() && !player_minimized;
            let show_mini_bar = now_playing.is_some() && player_minimized;

            // sidebar (search + results, only horizontally resizable)
            // | main content (player / now playing / lyrics) over an
            // optional mini player strip, over a bottom playback bar
            // (only vertically resizable). resize mode highlights
            // whichever divider borders the selected panel.
            let mini_bar_height: u16 = if show_mini_bar {
                ui::miniplayer::HEIGHT
            } else {
                0
            };
            let rows = Layout::vertical([
                Constraint::Min(0),
                Constraint::Length(mini_bar_height),
                Constraint::Length(panels.bottom_bar_height),
            ])
            .split(area);
            let top = rows[0];
            let mini_bar_row = rows[1];
            let bottom = rows[2];

            let cols = Layout::horizontal([
                Constraint::Length(panels.sidebar_width),
                Constraint::Length(1),
                Constraint::Min(0),
            ])
            .split(top);
            let sidebar = cols[0];
            let vdivider = cols[1];
            let main_content = cols[2];
            // the divider column belongs to neither pane, so a click
            // on it resolves to nothing rather than to whichever side
            // a guess would round it to.
            hits.sidebar = Some(sidebar);
            // the mini player strip is drawn in this column, directly
            // under it, so it is part of the pane a click there means;
            // without it those rows belong to nothing at all.
            hits.workspace = Some(Rect {
                height: main_content.height + mini_bar_height,
                ..main_content
            });
            hits.bottom = Some(bottom);

            // only resize mode highlights a divider, and only the one
            // it has selected. highlighting on navigation focus was
            // tried twice and is gone for good: these rules are made
            // of plain '│' segments sitting right next to junction
            // glyphs that stay NEAR_BLACK, so lifting the segments to
            // DIM made one half of a continuous line a different
            // shade from the other and read as damage rather than as
            // emphasis. resize mode gets away with it because there
            // you are acting on the divider itself. focus is answered
            // instead by the bottom bar naming the pane outright and
            // by the focused pane's own row pulsing.
            let sidebar_selected =
                panels.resize_mode && panels.resize_selected == ResizePanel::Sidebar;
            let bottom_selected =
                panels.resize_mode && panels.resize_selected == ResizePanel::BottomBar;

            // search box, one blank gap row, then the results list.
            // that gap row used to be a horizontal rule, which stacked
            // a third parallel line between the search box's own
            // bottom border and the list's first section-header rule.
            let sidebar_rows = Layout::vertical([
                Constraint::Length(3),
                Constraint::Length(1),
                Constraint::Min(0),
            ])
            .split(sidebar);
            let search_rect = sidebar_rows[0];
            let results_rect = sidebar_rows[2];
            hits.boxes.push((hit::TextBox::Query, search_rect));
            // which rules reach this divider from the right depends on
            // what main content shows: the browse view's header rules,
            // the mini player's strip, nothing at all from the player,
            // lyrics or banner. the glyph per row is derived from that
            // (see `ui::junctions`): ┬/┼ where a rule continues right,
            // ┐/┤ where it doesn't.
            let mut main_content_rules = match &browse_view {
                Some(bv) if !show_full_player => {
                    ui::browse::rule_rows(main_content, bv, art_mode, aspect)
                }
                _ => Vec::new(),
            };
            // the divider used to stop at the bottom of `top`, one row
            // short of the mini player strip below it (a separate row
            // in the outer vertical split, sidebar-width columns
            // included); the real bottom bar's rule still drew a ┴
            // junction expecting a line to feed into it from above,
            // so with the mini bar showing, that line had already
            // stopped 3 rows earlier. Stretch the divider down through
            // the mini bar's own height too in that case.
            let vdivider_rect = if show_mini_bar {
                main_content_rules.push(mini_bar_row.y);
                Rect {
                    height: vdivider.height + mini_bar_row.height,
                    ..vdivider
                }
            } else {
                vdivider
            };
            let junction_rows = ui::junctions::SidebarDivider {
                search: search_rect,
                results_header_rows: &ui::results_list::header_rows(
                    &app.results,
                    app.selected,
                    results_rect,
                ),
                main_content_rule_rows: &main_content_rules,
                bottom_rule_row: bottom.y,
            }
            .junctions(shade(NEAR_BLACK));
            f.render_widget(
                VerticalRule {
                    highlighted: sidebar_selected,
                    junction_rows,
                },
                vdivider_rect,
            );
            // a small bordered hint carved out of the bottom-right
            // corner, pointing at '?' for the full keybind reference,
            // instead of permanently cramming every keybind into the
            // bottom bar's own text. its left edge needs a ┬ in the
            // rule above it, same reasoning as every other junction
            // this session: two lines meant to meet don't, unless
            // something tells them to.
            // sized from the resolved label, since a rebound help key
            // can be wider than any constant guessed here.
            let help_label = format!(
                "{} help",
                keymap::key_label(keymap.get(KeyAction::OpenHelp))
            );
            let help_box_width = ui::helpbox::width_for(&help_label).min(bottom.width);
            let help_box_x = bottom.x + bottom.width.saturating_sub(help_box_width);
            f.render_widget(
                HorizontalRule {
                    highlighted: bottom_selected,
                    junctions: vec![(vdivider.x, '┴'), (help_box_x, '┬')],
                },
                Rect {
                    height: 1,
                    ..bottom
                },
            );

            // classic terminal cursor blink, off the same wall clock
            // as every other animation (see `anim::CURSOR_BLINK`).
            let cursor_visible = anim::blink(frame_elapsed, anim::CURSOR_BLINK);
            f.render_widget(
                SearchBox {
                    query: &app.query,
                    editing: app.editing_query,
                    label: "search",
                    cursor_visible,
                    // closed by the vertical divider instead, which
                    // sits one column past this rect.
                    right_border: false,
                },
                search_rect,
            );
            hits.lists.push(hit::List {
                which: hit::Scrollable::SidebarResults,
                rect: results_rect,
                rows: ui::results_list::row_items(&app.results, app.selected, results_rect),
                len: app.results.len(),
            });
            f.render_widget(
                ResultsList {
                    items: &app.results,
                    selected: app.selected,
                    empty_message: &empty_results_message(&app),
                    favorites: &favorites,
                    remote_starred: &remote_starred,
                    focused: focus == Focus::Sidebar,
                    elapsed: frame_elapsed,
                },
                results_rect,
            );

            // a video file gets the ordinary player (title, scrubber,
            // controls) when ratatui is drawing at all. it used to get
            // its own branch rasterizing a frame pulled off mpv, which
            // **could never produce one**: `screenshot-raw` does not
            // work over json ipc, and mpv draws video with its own
            // terminal vo now. `draw_unless_video` means this only
            // runs when mpv is *not* drawing (`video_output = "off"`,
            // or mid-handover), where that branch painted an empty
            // rect instead of the player.
            if show_full_player {
                let item = now_playing.as_ref().expect("show_full_player implies Some");
                let previous_label = previous_track.as_ref().map(label);
                let next_label = play_queue.peek_next().map(label);
                // the cover for what is *playing*: an open browse view's
                // art is only that when the queue is playing out of that
                // same view (the view outlives maximizing the player, so
                // that is the common case). a track from search, or from
                // a view since closed, needs its own getCoverArt fetch
                // keyed on the song's own id; until that lands those
                // tracks show no art rather than the wrong one.
                let has_context = previous_label.is_some() || next_label.is_some();
                let player_view = PlayerView {
                    title: &label(item),
                    // the cover rect becomes a countdown for a gap,
                    // which is the only thing anyone wants from the
                    // screen while one plays. elapsed is the scrub
                    // target mid-drag, so the countdown follows a drag
                    // too rather than fighting it.
                    gap_remaining: gap_left,
                    gap: match item {
                        SearchItem::Silence { secs } => Some(Gap {
                            remaining_secs: *secs as f64
                                - scrub.map(|s| s.target_secs).or(elapsed_secs).unwrap_or(0.0),
                        }),
                        _ => None,
                    },
                    art: playing_art,
                    art_key: &coverart::art_key(item),
                    art_mode,
                    cell_aspect: aspect,
                    // mid-drag the screen draws the pending target,
                    // not mpv's position, same as the bottom bar.
                    elapsed_secs: scrub.map(|s| s.target_secs).or(elapsed_secs),
                    duration_secs,
                    paused,
                    loop_mode: app.loop_mode,
                    shuffle_on: app.shuffle,
                    speed: app.speed,
                    eq_preset: &eq_label(&app, &cfg),
                    crossfade_on: crossfade_enabled,
                    previous: previous_label.as_deref(),
                    next: next_label.as_deref(),
                    selected: (focus == Focus::Workspace).then_some(player_selected),
                    scrubbing: scrub.is_some(),
                    keymap: &keymap,
                };
                let transport = player_view.transport(main_content);
                for (field, rect) in transport.controls {
                    hits.controls.push(hit::Control {
                        transport: hit::Transport::Player,
                        field,
                        rect,
                    });
                }
                if let Some(rect) = transport.track {
                    hits.tracks.push((hit::Transport::Player, rect));
                }
                f.render_widget(player_view, main_content);
                if art_mode.is_full() {
                    // same contract as the browse view: the widget drew
                    // no ascii art, the real image goes on the exact
                    // rect its own layout picked.
                    if let (Some(protocol), Some(art_rect)) = (
                        player_art.protocol(),
                        ui::player::layout(
                            main_content,
                            playing_art.is_some(),
                            has_context,
                            aspect,
                        )
                        .and_then(|l| l.art),
                    ) {
                        f.render_stateful_widget(StatefulImage::default(), art_rect, protocol);
                    }
                }

                level_history.push(audio_level(&playback, frame_elapsed));
                let max_history = main_content.width.max(1) as usize;
                if level_history.len() > max_history {
                    level_history.drain(..level_history.len() - max_history);
                }
                if main_content.height > 2 {
                    let meter_rect = Rect {
                        y: main_content.y + main_content.height - 2,
                        height: 2,
                        ..main_content
                    };
                    f.render_widget(
                        LevelMeter {
                            history: &level_history,
                        },
                        meter_rect,
                    );
                }
            } else if let Some(bv) = &browse_view {
                let rows = ui::browse::rows(main_content, bv, art_mode, aspect);
                if rows.albums.is_some() {
                    // only the artist profile draws a filter box, and
                    // only it has an albums column; one condition.
                    let (_, list_area) = browse::layout(
                        main_content,
                        bv.art.is_some() && art_mode != browse::ArtMode::Off,
                        bv.bio.is_some(),
                        bv.is_artist,
                        aspect,
                    );
                    hits.boxes.push((
                        hit::TextBox::BrowseFilter,
                        ui::browse::artist_columns(list_area).filter,
                    ));
                }
                if let Some((rect, rows)) = rows.albums {
                    hits.lists.push(hit::List {
                        which: hit::Scrollable::BrowseAlbums,
                        rect,
                        rows,
                        len: bv.albums.len(),
                    });
                }
                let (rect, rows) = rows.tracks;
                hits.lists.push(hit::List {
                    which: hit::Scrollable::BrowseTracks,
                    rect,
                    rows,
                    // what the filter shows, not every track: `len` is
                    // the space a wheel notch is clamped against, and
                    // under a filter those are not the same number.
                    len: bv.visible_track_indices().len(),
                });
                f.render_widget(
                    BrowseListView {
                        view: bv,
                        favorites: &favorites,
                        remote_starred: &remote_starred,
                        art_mode,
                        cell_aspect: aspect,
                        cursor_visible,
                        focused: focus == Focus::Workspace,
                        elapsed: frame_elapsed,
                    },
                    main_content,
                );
                if art_mode.is_full() {
                    if let Some(protocol) = browse_art.protocol() {
                        let (art_rect, _) = browse::layout(
                            main_content,
                            bv.art.is_some(),
                            bv.bio.is_some(),
                            bv.is_artist,
                            aspect,
                        );
                        if let Some(rect) = art_rect {
                            f.render_stateful_widget(StatefulImage::default(), rect, protocol);
                        }
                    }
                }
            } else if let Some(sug) = &suggestions {
                hits.lists.push(hit::List {
                    which: hit::Scrollable::Suggestions,
                    rect: ui::suggestions::body_rect(main_content),
                    rows: ui::suggestions::row_items(sug, suggestions_selected, main_content),
                    len: ui::suggestions::count(sug),
                });
                f.render_widget(
                    ui::suggestions::SuggestionsView {
                        suggestions: sug,
                        selected: suggestions_selected,
                        focused: focus == Focus::Workspace,
                        elapsed: frame_elapsed,
                        // a thin list must not read as the whole
                        // answer while fetches are still landing.
                        enriching: warmup.is_enriching(),
                    },
                    main_content,
                );
            } else if queue_view_open {
                hits.lists.push(hit::List {
                    which: hit::Scrollable::Queue,
                    rect: ui::queue::body_rect(main_content),
                    rows: ui::queue::row_items(
                        play_queue.entries(),
                        play_queue.pos(),
                        queue_selected,
                        main_content,
                    ),
                    len: play_queue.entries().len(),
                });
                f.render_widget(
                    ui::queue::QueueView {
                        entries: play_queue.entries(),
                        pos: play_queue.pos(),
                        selected: queue_selected,
                        focused: focus == Focus::Workspace,
                        elapsed: frame_elapsed,
                    },
                    main_content,
                );
            } else if lyrics.is_open() {
                // the width is reported before the lines are taken:
                // `lines()` borrows the pane, and `saw_width` needs it
                // mutably.
                lyrics.saw_width(main_content.width);
                let borrowed: Vec<&str> = lyrics
                    .lines()
                    .unwrap_or_default()
                    .iter()
                    .map(String::as_str)
                    .collect();
                // no rows: nothing here is selectable, so the wheel
                // moves the content offset `j`/`k` already move.
                hits.lists.push(hit::List {
                    which: hit::Scrollable::Lyrics,
                    rect: ui::header::body(main_content),
                    rows: Vec::new(),
                    len: ui::lyrics_view::wrapped_len(
                        &borrowed,
                        main_content.width.max(1) as usize,
                    ),
                });
                f.render_widget(
                    LyricsView {
                        lines: &borrowed,
                        scroll: lyrics.scroll(),
                        track: lyrics.track(),
                    },
                    main_content,
                );
            } else {
                f.render_widget(
                    Banner {
                        elapsed: frame_elapsed,
                        logo_index,
                        keymap: &keymap,
                    },
                    main_content,
                );
            }

            // 'u': settings, over the whole workspace (sidebar
            // included) same as the video pane's fullscreen mode,
            // never the bottom bar; painted last for the same reason.
            if settings_menu_open {
                let art_mode_label = art_mode.label();
                let token_value = if settings_token_editing {
                    format!(
                        "{} (enter to save, esc to cancel)",
                        "*".repeat(settings_token_input.chars().count())
                    )
                } else if cfg.listenbrainz_user_token.is_some()
                    || cfg.listenbrainz_user_token_file.is_some()
                {
                    "set (enter to change)".to_string()
                } else {
                    "not set (enter to add)".to_string()
                };
                let values = [
                    format!("{art_mode_label} ('i' also cycles this)"),
                    if crossfade_enabled { "on" } else { "off" }.to_string(),
                    format!("{:.1}s", cfg.crossfade_seconds),
                    format!("{} days played", cfg.cache_days_threshold),
                    format!("{} days", cfg.cache_eviction_days),
                    format!("{:.0}%", cfg.scrobble.threshold_percent),
                    format!("{:.0}s", cfg.scrobble.threshold_seconds),
                    token_value,
                ];
                f.render_widget(
                    ui::settings_menu::SettingsMenu {
                        values: [
                            values[0].as_str(),
                            values[1].as_str(),
                            values[2].as_str(),
                            values[3].as_str(),
                            values[4].as_str(),
                            values[5].as_str(),
                            values[6].as_str(),
                            values[7].as_str(),
                        ],
                        selected: settings_menu_selected,
                        keymap: &keymap,
                    },
                    top,
                );
            }

            // 'K': same full-workspace treatment.
            if keybinds_menu_open {
                let keys: Vec<String> = keymap::ALL_ACTIONS
                    .iter()
                    .map(|a| keymap::key_label(keymap.get(*a)))
                    .collect();
                f.render_widget(
                    ui::keybinds_menu::KeybindsMenu {
                        keys: &keys,
                        selected: keybinds_menu_selected,
                        capturing: capturing_key,
                        conflict: keybind_conflict.map(|a| a.label()),
                    },
                    top,
                );
            }

            // enter on the bottom bar's Eq field, same full-workspace
            // treatment.
            if eq_menu_open {
                f.render_widget(
                    ui::eq_menu::EqMenu {
                        gains: &app.eq_gains,
                        saved: &cfg.eq_custom_presets,
                        source: app.eq_source,
                        modified: app.eq_custom,
                        selected: eq_menu_selected,
                        state: &eq_menu,
                        elapsed: frame_elapsed,
                    },
                    top,
                );
            }

            // the undo tree. drawn over the workspace like the eq
            // menu, because the structure needs the width: a fork is
            // unreadable in a sidebar-sized column.
            if undo_menu_open {
                let rows = undo_tree.rows(start.elapsed());
                f.render_widget(
                    ui::undo_tree::UndoTreeView {
                        rows: &rows,
                        elapsed: frame_elapsed,
                        focused: true,
                    },
                    top,
                );
            }

            if show_mini_bar {
                let item = now_playing.as_ref().expect("show_mini_bar implies Some");
                f.render_widget(
                    MiniPlayerBar {
                        title: &label(item),
                        paused,
                        keymap: &keymap,
                    },
                    // same y/height as the row carved out above, but
                    // main_content's x/width: the strip must not slide
                    // under the sidebar.
                    Rect {
                        x: main_content.x,
                        width: main_content.width,
                        ..mini_bar_row
                    },
                );
            }

            let resize_hint = panels
                .resize_mode
                .then_some("resize mode: a=sidebar s=bottom bar, arrows stretch, r/esc exit");
            let flash_level =
                flash.map(|(field, at)| (field, anim::decay(at.elapsed(), FLASH_DURATION)));
            let controls_rect = Rect {
                y: bottom.y + 1,
                width: bottom.width.saturating_sub(help_box_width),
                height: bottom.height.saturating_sub(1),
                ..bottom
            };
            let controls_bar = ControlsBar {
                move_mode: input_mode == InputMode::Move,
                focus_label: match focus {
                    Focus::Sidebar => "sidebar",
                    Focus::Workspace => "workspace",
                    Focus::BottomBar => "bottom bar",
                },
                focused: focus == Focus::BottomBar,
                queue_len: play_queue.upcoming_len(),
                loop_mode: app.loop_mode,
                shuffle_on: app.shuffle,
                speed: app.speed,
                // the curve's name lives in the eq menu now; the
                // bar only answers "is the eq doing anything",
                // which is a question about the gains rather than
                // about what the curve is called.
                eq_active: !eq::is_flat(&app.eq_gains),
                crossfade_on: crossfade_enabled,
                paused,
                gap_remaining: gap_left,
                // mid-drag the bar draws the pending target, not
                // mpv's position: the seek is throttled, the
                // keypress shouldn't look like it was.
                elapsed_secs: scrub.map(|s| s.target_secs).or(elapsed_secs),
                duration_secs,
                flash: flash_level,
                selected: (focus == Focus::BottomBar).then_some(bottombar_selected),
                scrubbing: scrub.is_some(),
                resize_hint,
            };
            let bar = controls_bar.layout(controls_rect);
            for (field, rect) in bar.controls {
                hits.controls.push(hit::Control {
                    transport: hit::Transport::Bar,
                    field,
                    rect,
                });
            }
            if let Some(rect) = bar.track {
                hits.tracks.push((hit::Transport::Bar, rect));
            }
            f.render_widget(controls_bar, controls_rect);
            f.render_widget(
                ui::helpbox::HelpBox { label: &help_label },
                Rect {
                    x: help_box_x,
                    y: bottom.y + 1,
                    width: help_box_width,
                    height: bottom.height.saturating_sub(1),
                },
            );
            // recorded, not just painted: these boxes own every cell
            // they cover, so a click on one must stop there instead of
            // reaching the row underneath.
            let toasts = notifications.visible();
            hits.toasts = ui::notify::boxes(toasts, area);
            f.render_widget(NotificationOverlay { items: toasts }, area);

            if subtitle_menu_open {
                let values = [
                    format!("{:+.1}s", cfg.subtitles.delay_secs),
                    format!("{}", cfg.subtitles.font_size),
                    format!("{}", cfg.subtitles.position),
                    format!("{:.1}", cfg.subtitles.outline_size),
                    format!("{:.1}", cfg.subtitles.background_opacity),
                    format!("{:.1}", cfg.subtitles.blur),
                    format!("{:.2}", cfg.subtitles.color_level),
                ];
                f.render_widget(
                    ui::subtitle_menu::SubtitleMenu {
                        values: [
                            values[0].as_str(),
                            values[1].as_str(),
                            values[2].as_str(),
                            values[3].as_str(),
                            values[4].as_str(),
                            values[5].as_str(),
                            values[6].as_str(),
                        ],
                        selected: subtitle_menu_selected,
                        keymap: &keymap,
                    },
                    area,
                );
            }

            // the console goes on absolutely last: it is a centred box
            // over everything else (same class as the subtitle menu),
            // and it claims every key while it is up, so nothing may
            // ever be drawn on top of it. It centres itself in `area`.
            if console_open {
                f.render_widget(
                    ShellsongConsole {
                        console: &console,
                        cursor_visible,
                    },
                    area,
                );
            }
        })?;

        // the ui is on screen; now hand mpv the restored queue, which
        // is one blocking ipc round-trip per track (200 of them on a
        // real saved queue) and has no business delaying the first
        // frame. one-shot, not per-frame work.
        if !first_frame_drawn {
            first_frame_drawn = true;
            trace::mark("first frame drawn");
            for item in pending_enqueue.drain(..) {
                let _ = enqueue_track(&player, &item, &cache_index, &mut play_queue, app.shuffle);
            }
            trace::mark("mpv playlist refilled");
        }

        // mpv is the source of truth for which track is playing, and
        // these are its push notifications about that (see
        // `Mpv::observe_playback`). The whole channel is drained, not
        // one event per frame: mpv emits plenty of events we ignore,
        // and taking one at a time let them delay the ones we don't.
        //
        // the old design counted `end-file` events to guess the
        // position instead, which is what showed the wrong track: a
        // `loadfile replace` (any fresh play, or back) emits an
        // end-file for the track it *evicts*, so starting something new
        // mid-playback counted a phantom advance and left the display a
        // track ahead of mpv for the rest of the session.
        let mut advanced_to: Option<SearchItem> = None;
        let mut playlist_drained = false;
        while let Ok(ev) = player.events().try_recv() {
            let mpv::Event::PropertyChange { name, value } = ev else {
                continue;
            };
            if playback.apply(&name, &value) {
                continue;
            }
            if name == mpv::PROP_PLAYLIST_POS {
                if let PosChange::Advanced(_) = play_queue.set_pos(value.as_i64().unwrap_or(-1)) {
                    advanced_to = play_queue.current().cloned();
                }
            } else if name == mpv::PROP_IDLE_ACTIVE {
                playlist_drained = value.as_bool().unwrap_or(false);
            }
        }

        if let Some(next) = advanced_to {
            // `vid` is about what mpv decodes and belongs with the
            // load, not with playback starting, so it stays here.
            set_vid_for_item(&player, &next);
            let started = {
                let mut surf = wim_surface!();
                let mut core = core_ctx!();
                let s = track_started(&next, &mut core);
                let _ = &mut surf;
                s
            };
            if !started.ops.is_empty() {
                shellsong_queued = Some(Box::new(shellsong::Program::of(started.ops)));
            }
            if let Some(bound) = started.bound {
                notifications.push(Severity::Info, bound);
            }
            previous_track = now_playing.replace(next);
        }

        // mpv went back to idling, so its playlist really did run out
        // (unlike end-of-file, which also fires for a track that just
        // got replaced). The upcoming-empty check guards against
        // acting on a stale idle report from before something loaded.
        if playlist_drained {
            // the decision is `core::drained`, shared with shellswim;
            // what stays here are wim's own track-change hooks for a
            // loop-all restart.
            match drained(&player, &cache_index, &mut play_queue, app.loop_mode) {
                Drained::Stale => {}
                Drained::Cleared => now_playing = None,
                Drained::Failed(why) => notifications.push(Severity::Error, why),
                Drained::Restarted(first) => {
                    // a loop-all restart is a track change, so it is
                    // the same hook the advance above uses.
                    let started = {
                        let mut surf = wim_surface!();
                        let mut core = core_ctx!();
                        let s = track_started(&first, &mut core);
                        let _ = &mut surf;
                        s
                    };
                    if !started.ops.is_empty() {
                        shellsong_queued = Some(Box::new(shellsong::Program::of(started.ops)));
                    }
                    if let Some(bound) = started.bound {
                        notifications.push(Severity::Info, bound);
                    }
                    previous_track = now_playing.replace(first);
                }
            }
        }

        // the fade-out approaching a track's end and the fade-in right
        // after a new one starts are mutually exclusive in time (one
        // happens near end-of-file, the other right after), so this
        // is safe to check unconditionally every frame rather than
        // only while a particular view is showing; volume should fade
        // whether or not the player view is on screen.
        // the between-track gap. mpv holds at the end of each file
        // under `keep-open` and pauses itself; this waits the gap out
        // and then advances.
        //
        // `keep-open` is toggled rather than pinned on because holding
        // also stops mpv ever going idle, and `idle-active` going
        // quiet would take the end-of-playlist and loop-all handling
        // above with it. so it is only on while a gap is wanted *and*
        // something is queued after this track.
        between_tracks.step(
            &player,
            playback.eof_reached,
            play_queue.upcoming_len() > 0,
            Instant::now(),
        );

        if crossfade_enabled && now_playing.is_some() {
            if let Some(started) = fade_in_started_at {
                let t = (started.elapsed().as_secs_f64() / cfg.crossfade_seconds).clamp(0.0, 1.0);
                // a ramp step per frame, so it is sent without waiting
                // for mpv's reply; see `mpv::set_volume_nowait`.
                let _ = player.set_volume_nowait(base_volume * t);
                if t >= 1.0 {
                    fade_in_started_at = None;
                }
            } else {
                if let (Some(elapsed), Some(duration)) = (
                    playback.elapsed_secs,
                    playing_duration(now_playing.as_ref(), playback.duration_secs),
                ) {
                    let remaining = duration - elapsed;
                    if remaining <= cfg.crossfade_seconds {
                        let t = (remaining / cfg.crossfade_seconds).clamp(0.0, 1.0);
                        let _ = player.set_volume_nowait(base_volume * t);
                    }
                }
            }
        }

        // scrobbling: fires the "single" listen exactly once per
        // track, the moment the configured threshold is crossed;
        // checked every tick regardless of which view is on screen,
        // same unconditional treatment as the crossfade ramp above.
        // a no-op (see `fire_scrobble`) if listenbrainz isn't
        // configured at all.
        if !scrobble.scrobbled {
            if let Some(item) = &now_playing {
                if let (Some(elapsed), Some(duration)) =
                    (playback.elapsed_secs, playback.duration_secs)
                {
                    if listenbrainz::should_scrobble(&cfg.scrobble, elapsed, duration) {
                        let listened_at =
                            scrobble.started_unix.unwrap_or_else(listenbrainz::now_unix);
                        fire_scrobble(&cfg, item, listened_at);
                        scrobble.scrobbled = true;
                    }
                }
            }
        }

        // the terminal's answer to the graphics probe. a `full-*` mode
        // only starts drawing from here on; until (or unless) this
        // lands it stays ascii, and a terminal with no real protocol is
        // said so out loud rather than leaving an art pane that never
        // fills. the test is the *protocol*, never `picker.is_some()`:
        // see `has_real_protocol`.
        if !picker_probed {
            match picker_rx.try_recv() {
                Ok(probed) => {
                    picker_probed = true;
                    picker = probed;
                    let real = real_graphics(picker);
                    if art_mode == art_mode_pending {
                        art_mode = art_mode_for_picker(cfg.album_art, real);
                    }
                    if !real && wants_full_art(cfg.album_art) {
                        notifications.push(
                            Severity::Alert,
                            format!(
                                "album art: terminal reported no graphics protocol, using {}",
                                art_mode_for_picker(cfg.album_art, false).label()
                            ),
                        );
                    }
                }
                Err(TryRecvError::Empty) => {}
                Err(TryRecvError::Disconnected) => picker_probed = true,
            }
        }

        // drain whatever the library scan thread read since the last
        // frame, in batches rather than per file; the channel
        // disconnecting is the scan thread signing off (it drops its
        // sender), which is what clears the "scanning" message.
        let mut scanned_batches = 0;
        loop {
            match scan_rx.try_recv() {
                Ok(batch) => {
                    app.corpus.add_local_tracks(batch);
                    scanned_batches += 1;
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    app.scanning_library = false;
                    break;
                }
            }
        }
        if scanned_batches > 0 {
            app.resync_results();
        }

        // drain whatever the background sync thread finished since the
        // last frame; only notify on an actual offline/online
        // transition, not every retry while a server stays down.
        while let Ok(sr) = sync_rx.try_recv() {
            // reported either way: a server that answered with an
            // error has still answered, so nothing is left "connecting"
            // forever.
            app.syncing_servers = false;
            match sr.result {
                Ok((catalog, starred)) => {
                    merge_catalog_into_app(&mut app, catalog);
                    remote_starred.extend(starred);
                    // a freshly indexed server brings artists and
                    // albums the last enrichment probe never saw.
                    warmup.poke();
                    if server_reachable.insert(sr.url.clone(), true) == Some(false) {
                        notifications.push(Severity::Info, format!("{} is back online", sr.url));
                    }
                }
                Err(msg) => match server_reachable.insert(sr.url.clone(), false) {
                    // a server that's been unreachable since launch was
                    // never online to go offline from; failing the
                    // first connect is its own (never silent) message.
                    None => notifications.push(Severity::Error, format!("{}: {msg}", sr.url)),
                    Some(true) => notifications
                        .push(Severity::Alert, format!("{} went offline: {msg}", sr.url)),
                    Some(false) => {}
                },
            }
        }

        // whatever an open album/artist fetched since the last frame.
        // replies for a view that's since been closed or replaced are
        // dropped inside `poll` (see docs/async-open.md); the art
        // protocol is built here because the `Picker` lives here.
        let browse_loaded = browse_loader.poll(&mut browse_view);
        if browse_loaded.art.is_some() {
            // cleared rather than built here: the per-frame block
            // above builds it *with its key*, and this site had no key
            // to give it. that mismatch is exactly the desync
            // `ProtocolCache` exists to make unrepresentable -- the
            // protocol was set here while `browse_art_key` was set
            // somewhere else.
            browse_art.clear();
        }
        for msg in browse_loaded.errors {
            notifications.push(Severity::Error, msg);
        }

        // a subtitle `fire_subtitle_fetch` landed; loading it is this
        // thread's job, it owns the mpv handle.
        while let Ok(sub) = subtitle_rx.try_recv() {
            if player.sub_add(&sub.path).is_ok() {
                notifications.push(
                    Severity::Info,
                    format!("subtitle: {} ({})", sub.release, sub.language),
                );
            }
        }

        // the server refused a star, so the optimistic flip goes back.
        // reverting rather than re-asking: the user saw it take, and a
        // retry loop against a server saying no is worse than one
        // honest line.
        while let Ok(failed) = star_rx.try_recv() {
            if failed.wanted {
                remote_starred.remove(&failed.id);
            } else {
                remote_starred.insert(failed.id.clone());
            }
            let verb = if failed.wanted { "star" } else { "unstar" };
            notifications.push(
                Severity::Error,
                format!("couldn't {verb}: {}", failed.error),
            );
        }

        // a lyrics fetch landed. the loader has already dropped any
        // reply for a track the user moved past, so whatever arrives
        // here belongs to the request still standing.
        if let Some((track, result)) = lyrics_loader.poll() {
            match result {
                Ok(Some(l)) => {
                    lyrics.open(
                        wim::core::lyrics_text(&l)
                            .lines()
                            .map(str::to_string)
                            .collect(),
                        track,
                    );
                }
                Ok(None) => notifications.push(Severity::Info, format!("no lyrics for {track}")),
                Err(e) => notifications.push(Severity::Error, format!("lyrics lookup failed: {e}")),
            }
        }

        // drain whatever the live remote-search thread answered since
        // the last frame; merged unconditionally and re-filtered
        // against whatever's actually typed right now (see
        // `spawn_background_search`'s doc for why that's safe even if
        // the query's since moved on).
        while let Ok(outcome) = remote_search_result_rx.try_recv() {
            app.corpus
                .add_subsonic_search_result(outcome.result, &outcome.client);
            app.resync_results();
            // a wider corpus can mean more artists and albums worth
            // enriching than the last probe found.
            warmup.poke();
            // an answer is in, so the results for whatever is typed are
            // as complete as a shellsong program should wait for.
            settle.answered();
        }

        // drain whatever the background enrichment fetcher answered
        // since the last frame. nothing here touches the network: it
        // folds the answers into the engine's inputs and marks the
        // menu for one re-run, however many answers arrived.
        let mut enrichment_grew = false;
        while let Ok(event) = enrich_rx.try_recv() {
            match event {
                EnrichmentEvent::ArtistInfo { id, info } => {
                    enrichment.add_artist_info(&id, &info);
                    enrichment_grew = true;
                    warmup.answered();
                }
                EnrichmentEvent::TopSongs {
                    artist,
                    songs,
                    client,
                } => {
                    suggest_catalog.ingest_songs(&songs, &client);
                    enrichment.add_top_songs(&artist, &songs);
                    enrichment_grew = true;
                    warmup.answered();
                }
                EnrichmentEvent::Album {
                    artist,
                    name,
                    songs,
                    client,
                } => {
                    // an empty tracklist means the fetch failed or the
                    // album really has no songs; recording a total of
                    // zero would blind both coverage generators to it,
                    // so mark it asked and leave the total unknown.
                    if songs.is_empty() {
                        suggest_catalog.mark_album_asked(&artist, &name);
                    } else {
                        suggest_catalog.ingest_album(&artist, &name, &songs, &client);
                        warmup.answered();
                    }
                }
                EnrichmentEvent::JobDone => {
                    warmup.job_done();
                }
            }
        }
        if enrichment_grew {
            // a small file written at most once per frame that saw an
            // answer; a failed write only ever costs a re-fetch.
            let _ = enrichment.save();
        }
        if warmup.take_stale() && suggestions.is_some() {
            {
                let starred = suggestion_starred_keys(&remote_starred, &favorites);
                let refreshed = recompute_suggestions(
                    &mut suggest_catalog,
                    &app.corpus,
                    &cache_index,
                    &starred,
                    &enrichment,
                );
                // the list can shrink as well as grow (a row dedups
                // into a stronger reason), so the cursor is clamped
                // rather than assumed still valid.
                suggestions_selected =
                    suggestions_selected.min(ui::suggestions::count(&refreshed).saturating_sub(1));
                suggestions = Some(refreshed);
            }
        }
        // warm up only while the menu is open: these are network calls
        // for one feature, nobody should pay for them unprompted, and
        // the next batch is only picked once the last one finished.
        // all of that rule lives in `Warmup::should_fetch`.
        if warmup.should_fetch(suggestions.is_some()) {
            let starred = suggestion_starred_keys(&remote_starred, &favorites);
            let guard = cache_index.lock().expect("cache mutex poisoned");
            let job = next_enrichment_job(&suggest_catalog, &guard, &starred, &enrichment);
            drop(guard);
            if !job.is_empty() {
                warmup.sent(enrich_job_tx.send(job).is_ok());
            }
        }

        if last_cache_sweep.elapsed() >= CACHE_SWEEP_INTERVAL {
            sweep_cache(&cache_index, cfg.cache_eviction_days);
            last_cache_sweep = Instant::now();
        }

        // drain whatever any in-flight download (auto or manual) has
        // reported since the last frame; `downloading` only ever
        // tracks ids currently mid-transfer, so both terminal variants
        // remove the id regardless of success/failure.
        while let Ok(event) = download_rx.try_recv() {
            match event {
                DownloadEvent::Progress {
                    id,
                    downloaded,
                    total,
                } => notifications.update_progress(&id, downloaded, total),
                DownloadEvent::Done { id, label } => {
                    downloading.remove(&id);
                    notifications.finish_progress(
                        &id,
                        Severity::Info,
                        format!("downloaded: {label}"),
                    );
                }
                DownloadEvent::Failed { id, label, error } => {
                    downloading.remove(&id);
                    notifications.finish_progress(
                        &id,
                        Severity::Error,
                        format!("download failed: {label}: {error}"),
                    );
                }
            }
        }

        // the throttled half of scrub mode: arrows only moved the
        // target, this sends at most one absolute seek per interval, so
        // a held arrow key costs one ipc round-trip per 80ms instead of
        // one per keypress. the poll below bounds the lag at a frame.
        if scrub_pending {
            match scrub {
                Some(s) if scrub_sent_at.is_none_or(|at| at.elapsed() >= SCRUB_SEEK_INTERVAL) => {
                    if let Err(e) = player.seek(s.target_secs, false) {
                        notifications.push(Severity::Error, format!("error seeking: {e}"));
                    }
                    scrub_sent_at = Some(Instant::now());
                    scrub_pending = false;
                }
                None => scrub_pending = false,
                _ => {}
            }
        }

        // shellsong's own volume ramps, ticked unconditionally like the
        // crossfade ramp above and for the same reason: volume should
        // fade whether or not the console is still on screen.
        // both halves of the seam, built from this frame's locals at
        // every use the way `DownloadCtx` already is. Macros rather
        // than functions because they are struct literals over
        // `main`'s own locals; two disjoint borrow sets, which is why
        // they can be held at once. See docs/library-seam.md.
        if let Some(ramp) = shellsong_fade {
            let _ = player.set_volume_nowait(ramp.level_at(shellsong_now));
            if ramp.finished(shellsong_now) {
                shellsong_fade = None;
            }
        }
        // `pause(30s)` letting go. the mirror of `sleep_due` below,
        // and checked first for no reason other than reading order.
        if resume_due.is_some_and(|due| shellsong_now >= due) {
            resume_due = None;
            let _ = player.set_pause(false);
        }
        if let Some(due) = sleep_due {
            let fade_over = Duration::from_secs_f64(cfg.crossfade_seconds);
            match shellsong_host::sleep_step(due, fade_over, base_volume, shellsong_now) {
                SleepStep::Waiting => {}
                SleepStep::Fading(level) => {
                    let _ = player.set_volume(level);
                }
                SleepStep::Expired => {
                    sleep_due = None;
                    if !paused {
                        let _ = player.toggle_pause();
                    }
                    // back up to the ceiling, or unpausing later would
                    // start silent.
                    let _ = player.set_volume(base_volume);
                    notifications.push(Severity::Info, "sleep timer: paused");
                    console.note("slept");
                }
            }
        }

        // drive whatever shellsong program is running: step in a loop,
        // applying each `Emit` before the next `step`, and stop for
        // this frame on `AwaitResults` or `Done`. Never blocks; a
        // command that searches resumes on a later frame, once the
        // results for its query have settled.
        // one call: `core::drive` owns the step loop, so wim and
        // shellswim cannot diverge on await handling, on when a
        // finished program is dropped, or on where a failure lands.
        let mut shellsong_quit = false;
        if shellsong_program.is_none() {
            shellsong_program = shellsong_queued.take();
        }
        if let Some(program) = shellsong_program.as_mut() {
            let mut surf = wim_surface!();
            let mut core = core_ctx!();
            match drive(program, &mut core, &mut surf, |c| c.settle.settled(c.now)) {
                Drive::Quit => {
                    shellsong_quit = true;
                    shellsong_program = None;
                }
                Drive::Done => shellsong_program = None,
                // the results a `Resolve` wants are not in yet; resume
                // next frame rather than spinning or blocking.
                Drive::Await => {}
            }
        }
        if shellsong_quit {
            break;
        }

        // whichever animations are actually live this frame set the
        // poll timeout: the display's frame budget while something is
        // moving, a long idle sleep otherwise. a static screen (no
        // selection pulse, no blinking cursor, nothing playing) must
        // not redraw at 60fps just because the display could.
        let selection_pulsing = match focus {
            Focus::Sidebar => !app.results.is_empty(),
            // every list the workspace can hold, not just the browse
            // view: the suggestions menu and the queue view both plate
            // a selected row, and leaving them out dropped their pulse
            // to the 100ms idle poll, a tenth of the display rate.
            Focus::Workspace => browse_view.is_some() || suggestions.is_some() || queue_view_open,
            Focus::BottomBar => false,
        };
        let banner_showing = now_playing.is_none() && browse_view.is_none() && !lyrics.is_open();
        let animating = !overlay
            && (selection_pulsing
                || banner_showing
                || app.editing_query
                || console_open
                // a program resumes a frame at a time, so dropping to
                // the idle poll would make an awaited search crawl.
                || shellsong_program.is_some()
                || browse_view.as_ref().is_some_and(|bv| bv.filtering)
                // an open still filling in has to redraw as its rows
                // land, whichever pane holds focus.
                || browse_view.as_ref().is_some_and(|bv| bv.loading)
                || (now_playing.is_some() && !player_minimized)
                || scrub.is_some()
                // a gap counts down on screen, and mpv is paused
                // through it, so none of the playback-driven
                // conditions above would keep the frame rate up.
                || between_tracks.holding()
                || input_mode == InputMode::Move
                || flash.is_some_and(|(_, at)| at.elapsed() < FLASH_DURATION));
        let timeout = if animating {
            frame.interval()
        } else {
            anim::IDLE_POLL
        };

        // a queue batch resolved. the resolver has already dropped any
        // belonging to a superseded request, which matters more here
        // than elsewhere: a late arrival would not just draw something
        // stale, it would append to the queue behind the user's back.
        // the undo tree's root, planted once before any key is read.
        // it cannot be planted lazily from `dispatch_action` the way
        // `apply_op` does it: `app.handle_key` has already applied
        // loop/shuffle/speed by the time an action arrives, so a
        // "before" captured there is really an "after", and undoing
        // the first keypress of a session would restore what it had
        // just left.
        if undo_tree.is_unrooted() {
            let core = core_ctx!();
            let root = wim::undo::Snapshot::capture(&core);
            let now = core.now;
            core.undo.plant(root, now);
        }

        while let Some((into, label, tracks)) = track_resolver.poll() {
            let mut surf = wim_surface!();
            let mut core = core_ctx!();
            match tracks {
                Ok(tracks) if tracks.is_empty() => core.notifications.push(
                    Severity::Info,
                    format!("nothing to {} in {label}", into.verb()),
                ),
                Ok(tracks) => match into {
                    Intent::Back => append_resolved(&tracks, &mut core),
                    Intent::Front => match front_resolved(&tracks, &mut core) {
                        Ok(n) => core
                            .notifications
                            .push(Severity::Info, format!("queued {n} tracks next")),
                        Err(why) => core.notifications.push(Severity::Error, why),
                    },
                    // unlike the queue arms, this one always reports:
                    // the fetch already said "resolving ...", so going
                    // quiet on "already cached" would leave that as the
                    // last word.
                    Intent::Download => {
                        let done = download_resolved(&tracks, &mut core);
                        core.notifications.push(Severity::Info, done.status);
                    }
                },
                Err(why) => core.notifications.push(Severity::Error, why),
            }
            let _ = &mut surf;
        }

        if event::poll(timeout)? {
            let ev = event::read()?;

            // bracketed paste delivers the whole pasted string as one
            // event, not synthesized keystrokes; enabling bracketed
            // paste mode (term.rs) without handling this event meant
            // paste did literally nothing. route it to whichever text
            // input is actually active, same priority order as key
            // handling below.
            if let Event::Paste(text) = &ev {
                if let Some(ob) = &mut onboarding {
                    match ob.step {
                        Step::Navidrome => ob.login.current_mut().push_str(text),
                        Step::ListenBrainz => ob.listenbrainz_token_input.push_str(text),
                        Step::LibraryPaths => ob.path_input.push_str(text),
                        _ => {}
                    }
                } else if let Some(nm) = &mut nav_manager {
                    if let NavManagerStep::Add(form) = &mut nm.step {
                        form.current_mut().push_str(text);
                    }
                } else if settings_menu_open && settings_token_editing {
                    // long random tokens are exactly the kind of thing
                    // worth pasting rather than typing by hand.
                    settings_token_input.push_str(text);
                } else if console_open {
                    // a bracketed paste goes into the console line
                    // literally, with no auto-pairing; same claim over
                    // paste the console has over keys.
                    console.paste(
                        text,
                        &CorpusCandidates {
                            corpus: &app.corpus,
                            saved: &saved_queues,
                            eq_presets: eq_preset_names(&cfg.eq_custom_presets),
                            vars: &shellsong_vars,
                            scripts: &shellsong_scripts,
                        },
                    );
                } else if !help_open {
                    let before = app.query.clone();
                    app.paste_into_query(text);
                    if app.query != before {
                        let fired = notify_query_changed(&app, &remote_search_tx);
                        settle.notified(fired, start.elapsed());
                    }
                }
            }

            // the mouse, answered against the map the draw above
            // recorded: `hit::act` is given the frame the user was
            // actually looking at and decides everything, and this
            // only applies the answer. a drag belongs to the focused
            // surface's scrubber, which is the only one it can have
            // been opened on (the keyboard opens one on the focused
            // pane, and a click sets focus to whatever it hit).
            if let Event::Mouse(m) = &ev {
                let dragging = scrub.is_some().then_some(match focus {
                    Focus::BottomBar => hit::Transport::Bar,
                    _ => hit::Transport::Player,
                });
                let click = hit::Click {
                    kind: mouse_kind(m.kind),
                    col: m.column,
                    row: m.row,
                };
                // The help overlay is the one screen-covering tier with
                // more content than screen, so a notch over it scrolls
                // instead of being swallowed: "the wheel does what j/k
                // do on this surface" (docs/input-precedence.md).
                // Handled before `hit::act` so `Modal::covers_screen`
                // stays a plain swallow with no wheel exception in it.
                if help_open {
                    match click.kind {
                        hit::Kind::WheelDown => {
                            help_scroll = (help_scroll + hit::WHEEL_ROWS).min(help_scroll_max);
                        }
                        hit::Kind::WheelUp => {
                            help_scroll = help_scroll.saturating_sub(hit::WHEEL_ROWS);
                        }
                        _ => {}
                    }
                    continue;
                }
                let act = hit::act(&hits, click, dragging);
                // a click lands somewhere specific, so unlike a key it
                // cannot leak into a text box; what it does instead is
                // take the keyboard out of one, the way clicking away
                // from a field does anywhere else. The screen-covering
                // tiers never get here at all (`Act::Swallowed`).
                if matches!(
                    act,
                    hit::Act::Focus(_) | hit::Act::Select(..) | hit::Act::Press(..)
                ) {
                    app.editing_query = false;
                    if let Some(bv) = &mut browse_view {
                        bv.filtering = false;
                    }
                }
                match act {
                    hit::Act::Nothing | hit::Act::Swallowed(_) => {}
                    hit::Act::Focus(pane) => focus = pane,
                    hit::Act::Edit(which) => match which {
                        hit::TextBox::Query => {
                            focus = Focus::Sidebar;
                            app.editing_query = true;
                        }
                        hit::TextBox::BrowseFilter => {
                            focus = Focus::Workspace;
                            if let Some(bv) = &mut browse_view {
                                bv.filtering = true;
                            }
                        }
                    },
                    hit::Act::Select(which, item) => {
                        focus = which.pane();
                        match which {
                            hit::Scrollable::SidebarResults => {
                                // through the one selection primitive
                                // j/k and shellsong's up()/down() use,
                                // rather than a second clamp here.
                                app.move_selection(item as isize - app.selected as isize);
                            }
                            hit::Scrollable::BrowseAlbums => {
                                if let Some(bv) = &mut browse_view {
                                    bv.albums_selected = item;
                                    bv.albums_focused = true;
                                }
                            }
                            hit::Scrollable::BrowseTracks => {
                                if let Some(bv) = &mut browse_view {
                                    bv.selected = item;
                                    bv.albums_focused = false;
                                    bv.clamp_selection();
                                }
                            }
                            hit::Scrollable::Queue => queue_selected = item,
                            hit::Scrollable::Suggestions => suggestions_selected = item,
                            // nothing in there to select; see the
                            // wheel arm below.
                            hit::Scrollable::Lyrics => {}
                        }
                    }
                    hit::Act::Press(transport, field, frac) => {
                        // the cursor moves onto what was clicked and
                        // then the one activation path runs, so a
                        // click and enter on the same glyph are the
                        // same operation (`activate_transport`).
                        let at = match frac {
                            Some(frac) => ScrubAt::Fraction(frac),
                            None => ScrubAt::Position,
                        };
                        match transport {
                            hit::Transport::Bar => {
                                focus = Focus::BottomBar;
                                bottombar_selected = field;
                            }
                            hit::Transport::Player => {
                                focus = Focus::Workspace;
                                player_selected = field;
                            }
                        }
                        let mut tctx = TransportCtx {
                            scrub: &mut scrub,
                            scrub_pending: &mut scrub_pending,
                            scrub_sent_at: &mut scrub_sent_at,
                            elapsed_secs: playback.elapsed_secs,
                            duration_secs: playing_duration(
                                now_playing.as_ref(),
                                playback.duration_secs,
                            ),
                        };
                        let mut surf = wim_surface!();
                        let mut core = core_ctx!();
                        activate_transport(field, at, &mut core, &mut surf, &mut tctx);
                    }
                    // the drag redraws from the pending target and the
                    // seek stays coalesced behind it, exactly as a
                    // held arrow key does.
                    hit::Act::ScrubTo(frac) => {
                        if let Some(s) = scrub {
                            scrub = Some(s.at_fraction(frac));
                            scrub_pending = true;
                        }
                    }
                    hit::Act::ScrubCommit(frac) => {
                        if let Some(s) = scrub {
                            commit_scrub(
                                s.at_fraction(frac),
                                &player,
                                &mut notifications,
                                &mut scrub,
                                &mut scrub_pending,
                                &mut scrub_sent_at,
                            );
                        }
                    }
                    // the wheel moves the selection, not a viewport of
                    // its own: one notch is then the same operation as
                    // one `j`, `ui::scroll`'s clamp-to-selection rule
                    // keeps the selection on screen for free, and
                    // there is no second scroll offset for the
                    // keyboard to disagree with. It does not take
                    // focus; scrolling a list is not a commitment to
                    // acting on it. See docs/input-precedence.md.
                    hit::Act::Wheel(which, down) => {
                        let len = hits
                            .lists
                            .iter()
                            .find(|l| l.which == which)
                            .map_or(0, |l| l.len);
                        match which {
                            hit::Scrollable::SidebarResults => {
                                let to = hit::wheel_select(app.selected, len, down);
                                app.move_selection(to as isize - app.selected as isize);
                            }
                            hit::Scrollable::BrowseAlbums => {
                                if let Some(bv) = &mut browse_view {
                                    bv.albums_selected =
                                        hit::wheel_select(bv.albums_selected, len, down);
                                }
                            }
                            hit::Scrollable::BrowseTracks => {
                                // the filter sits between a row and a
                                // track index, so the notch steps
                                // through the visible rows and reads
                                // the track back off them.
                                if let Some(bv) = &mut browse_view {
                                    let visible = bv.visible_track_indices();
                                    let at =
                                        visible.iter().position(|i| *i == bv.selected).unwrap_or(0);
                                    let to = hit::wheel_select(at, visible.len(), down);
                                    if let Some(idx) = visible.get(to) {
                                        bv.selected = *idx;
                                    }
                                }
                            }
                            // same display order the queue view's own
                            // j/k walks, history reversed and all.
                            hit::Scrollable::Queue => {
                                let order =
                                    ui::queue::selectable(play_queue.entries(), play_queue.pos());
                                let at =
                                    order.iter().position(|i| *i == queue_selected).unwrap_or(0);
                                let to = hit::wheel_select(at, order.len(), down);
                                if let Some(idx) = order.get(to) {
                                    queue_selected = *idx;
                                }
                            }
                            hit::Scrollable::Suggestions => {
                                suggestions_selected =
                                    hit::wheel_select(suggestions_selected, len, down);
                            }
                            hit::Scrollable::Lyrics => {
                                lyrics.set_scroll(hit::wheel_scroll(lyrics.scroll(), len, down));
                            }
                        }
                    }
                }
                continue;
            }

            if let Event::Key(key) = ev {
                // the one unconditional quit, regardless of what
                // modal/menu is open; 'q' still works too but only
                // where nothing else claims it first (see app.rs),
                // same as every other single-letter shortcut.
                if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
                    save_playback_state(&player, &play_queue);
                    break;
                }

                if let Some(ob) = &mut onboarding {
                    match ob.step {
                        Step::Welcome | Step::Controls => ob.advance(),
                        Step::Navidrome => match key.code {
                            KeyCode::Esc => ob.advance(),
                            KeyCode::Char(c) => ob.login.current_mut().push(c),
                            KeyCode::Backspace => {
                                if ob.login.current_mut().is_empty() {
                                    ob.login.prev_field();
                                } else {
                                    ob.login.current_mut().pop();
                                }
                            }
                            KeyCode::Up => ob.login.prev_field(),
                            KeyCode::Down => ob.login.next_field_clamped(),
                            KeyCode::Tab => ob.login.next_field(),
                            KeyCode::Enter if !ob.login.is_last_field() => ob.login.next_field(),
                            KeyCode::Enter => {
                                let nav = navidrome_config_from_form(&ob.login);
                                match index_navidrome_server(&mut app, &nav) {
                                    Ok(msg) => {
                                        cfg.navidrome_servers.push(nav);
                                        config::save_or_warn(&cfg, &mut notifications);
                                        let _ = servers_tx.send(cfg.navidrome_servers.clone());
                                        notifications.push(
                                            Severity::Info,
                                            format!("navidrome connected: {msg}"),
                                        );
                                        ob.advance();
                                    }
                                    Err(msg) => ob.login_message = format!("{msg} (esc to skip)"),
                                }
                            }
                            _ => {}
                        },
                        Step::ListenBrainz => match key.code {
                            KeyCode::Esc => ob.advance(),
                            KeyCode::Char(c) => ob.listenbrainz_token_input.push(c),
                            KeyCode::Backspace => {
                                ob.listenbrainz_token_input.pop();
                            }
                            KeyCode::Enter => {
                                let token = ob.listenbrainz_token_input.trim().to_string();
                                if !token.is_empty() {
                                    cfg.listenbrainz_user_token = Some(token);
                                    config::save_or_warn(&cfg, &mut notifications);
                                    notifications
                                        .push(Severity::Info, "listenbrainz scrobbling enabled");
                                }
                                ob.advance();
                            }
                            _ => {}
                        },
                        Step::LibraryPaths => match key.code {
                            KeyCode::Char(c) => ob.path_input.push(c),
                            KeyCode::Backspace => {
                                ob.path_input.pop();
                            }
                            KeyCode::Enter if !ob.path_input.trim().is_empty() => {
                                ob.library_paths.push(ob.path_input.trim().to_string());
                                ob.path_input.clear();
                            }
                            KeyCode::Enter | KeyCode::Esc => {
                                cfg.library_paths = ob.library_paths.clone();
                                config::save_or_warn(&cfg, &mut notifications);
                                app.corpus = rebuild_corpus(&cfg);
                                ob.advance();
                            }
                            _ => {}
                        },
                        Step::Done => onboarding = None,
                    }
                    continue;
                }

                if let Some(nm) = &mut nav_manager {
                    match &mut nm.step {
                        NavManagerStep::List => {
                            let entry_count = cfg.navidrome_servers.len() + 1; // +1 for "add new"
                                                                               // any key other than a second 'd' cancels
                                                                               // a pending removal, including moving off
                                                                               // the row it was armed on.
                            if key.code != KeyCode::Char('d') {
                                nm.pending_delete = None;
                            }
                            match key.code {
                                KeyCode::Esc => nav_manager = None,
                                KeyCode::Char('j') | KeyCode::Down => {
                                    nm.selected = (nm.selected + 1).min(entry_count - 1);
                                }
                                KeyCode::Char('k') | KeyCode::Up => {
                                    nm.selected = nm.selected.saturating_sub(1);
                                }
                                KeyCode::Char('d') if nm.selected < cfg.navidrome_servers.len() => {
                                    // first press arms, second on the
                                    // same row commits; this rewrites
                                    // config.toml and can't be undone.
                                    if nm.pending_delete == Some(nm.selected) {
                                        cfg.navidrome_servers.remove(nm.selected);
                                        nm.forget_status_at(nm.selected);
                                        config::save_or_warn(&cfg, &mut notifications);
                                        nm.selected = nm.selected.min(cfg.navidrome_servers.len());
                                        nm.pending_delete = None;
                                        nm.message = "profile removed (already-indexed items stay until restart)".to_string();
                                    } else {
                                        nm.pending_delete = Some(nm.selected);
                                        nm.message = format!(
                                            "press d again to remove {}",
                                            cfg.navidrome_servers[nm.selected].url
                                        );
                                    }
                                }
                                KeyCode::Enter if nm.selected == cfg.navidrome_servers.len() => {
                                    nm.step = NavManagerStep::Add(LoginForm::new());
                                }
                                KeyCode::Enter => {
                                    let nav = cfg.navidrome_servers[nm.selected].clone();
                                    let result = index_navidrome_server(&mut app, &nav);
                                    if nm.selected >= nm.connection_status.len() {
                                        nm.connection_status.resize(nm.selected + 1, None);
                                    }
                                    nm.connection_status[nm.selected] = Some(result.is_ok());
                                    nm.message = match result {
                                        Ok(msg) => format!("reconnected: {msg}"),
                                        Err(msg) => {
                                            notifications.push(
                                                Severity::Error,
                                                format!("{}: {msg}", nav.url),
                                            );
                                            msg
                                        }
                                    };
                                }
                                _ => {}
                            }
                        }
                        NavManagerStep::Add(form) => match key.code {
                            KeyCode::Esc => {
                                nm.step = NavManagerStep::List;
                                nm.message.clear();
                            }
                            KeyCode::Char(c) => form.current_mut().push(c),
                            KeyCode::Backspace => {
                                if form.current_mut().is_empty() {
                                    form.prev_field();
                                } else {
                                    form.current_mut().pop();
                                }
                            }
                            KeyCode::Up => form.prev_field(),
                            KeyCode::Down => form.next_field_clamped(),
                            KeyCode::Tab => form.next_field(),
                            KeyCode::Enter if !form.is_last_field() => form.next_field(),
                            KeyCode::Enter => {
                                let nav = navidrome_config_from_form(form);
                                match index_navidrome_server(&mut app, &nav) {
                                    Ok(msg) => {
                                        notifications.push(
                                            Severity::Info,
                                            format!("connected: {}", nav.url),
                                        );
                                        cfg.navidrome_servers.push(nav);
                                        config::save_or_warn(&cfg, &mut notifications);
                                        warn_plaintext_password(&cfg, &mut notifications);
                                        nm.connection_status.push(Some(true));
                                        nm.selected = cfg.navidrome_servers.len() - 1;
                                        nm.step = NavManagerStep::List;
                                        nm.message = format!("connected: {msg}");
                                    }
                                    Err(msg) => {
                                        notifications
                                            .push(Severity::Error, format!("{}: {msg}", nav.url));
                                        nm.message = format!("{msg} (esc to go back)");
                                    }
                                }
                            }
                            _ => {}
                        },
                    }
                    continue;
                }

                if help_open {
                    if key.code == KeyCode::Esc || key.code == keymap.get(KeyAction::OpenHelp) {
                        help_open = false;
                    } else {
                        // j/k here for the same reason they scroll the
                        // lyrics pane: the surface has a content offset
                        // rather than a selection. Clamped to the
                        // layout the last frame actually produced.
                        match key.code {
                            KeyCode::Char('j') | KeyCode::Down => {
                                help_scroll = (help_scroll + 1).min(help_scroll_max);
                            }
                            KeyCode::Char('k') | KeyCode::Up => {
                                help_scroll = help_scroll.saturating_sub(1);
                            }
                            KeyCode::Char('G') | KeyCode::End => help_scroll = help_scroll_max,
                            KeyCode::Char('g') | KeyCode::Home => help_scroll = 0,
                            _ => {}
                        }
                    }
                    continue;
                }

                // the console claims every key while it is open, at
                // the same tier the query box below does and for
                // exactly the same reason: a text input that lets keys
                // past it is how '?'/'t'/'u' ended up firing commands
                // instead of landing in the box, twice. Above
                // `editing_query` rather than below it only because
                // the two can never both be open (':' can't be pressed
                // while the query box has the keyboard).
                if console_open {
                    let cand = CorpusCandidates {
                        corpus: &app.corpus,
                        saved: &saved_queues,
                        eq_presets: eq_preset_names(&cfg.eq_custom_presets),
                        vars: &shellsong_vars,
                        scripts: &shellsong_scripts,
                    };
                    // the whole `KeyEvent`, not just `key.code`: the
                    // console reads ctrl for its accept key, which no
                    // other tier in this file needs.
                    match console.handle_key(key, &cand) {
                        ConsoleAction::None => {}
                        ConsoleAction::Close => console_open = false,
                        // a submitted line is held and stepped from the
                        // loop body, not run here: a command that
                        // searches cannot finish in one pass.
                        ConsoleAction::Run(program) => shellsong_program = Some(program),
                    }
                    continue;
                }

                // the tier order below is also `hit::TIERS`, which
                // is how a mouse click decides which surface owns a
                // cell; adding a tier here means adding it there too
                // (see docs/input-precedence.md).
                // every block below intercepts specific keys and
                // unconditionally `continue`s even on a non-match
                // (browse_view/lyrics/the subtitle+settings menus
                // especially), so this has to be the very next check
                // after the modal-overlay ones above, not somewhere
                // later where new handlers keep getting inserted
                // ahead of it (which is exactly how 't'/'u' ended up
                // leaking into a query being typed instead of landing
                // in the text box): editing a query claims every key,
                // full stop, before anything else gets a chance at it.
                if app.editing_query {
                    let before = app.query.clone();
                    let action = app.handle_key(key.code, &keymap);
                    if app.query != before {
                        let fired = notify_query_changed(&app, &remote_search_tx);
                        settle.notified(fired, start.elapsed());
                    }
                    // enter while typing commits the selection, so the
                    // action has to run through the same dispatch the
                    // normal path uses; throwing it away here is what
                    // made the first enter after a search do nothing
                    // but close the text box.
                    let mut surf = wim_surface!();
                    let mut core = core_ctx!();
                    let dispatch = dispatch_action(action, &mut core, &mut surf);

                    if dispatch == Dispatch::Quit {
                        break;
                    }
                    continue;
                }

                // after the query box, never before: `?` is an
                // ordinary character to type into a search, and
                // claiming it up here meant it could never reach the
                // text box at all.
                if key.code == keymap.get(KeyAction::OpenHelp) {
                    help_open = true;
                    // always opens at the top, so '?' twice is the same
                    // screen both times.
                    help_scroll = 0;
                    continue;
                }

                // same reasoning, same tier: ':' is an ordinary
                // character to type into a search. Hardcoded rather
                // than remappable, and listed in
                // `keymap::reserved_key_purpose` so no action can be
                // bound onto it and then silently shadowed by this.
                if key.code == shellsong::OPEN_KEY {
                    console_open = true;
                    // one disk read per open, so completion itself
                    // never touches it.
                    saved_queues = queues::list();
                    shellsong_scripts = shellsong_host::script_names();
                    continue;
                }

                // same reasoning, same tier: typing into the artist
                // profile's own song filter has to claim every key
                // too, or it's the exact same leak all over again for
                // a second text box.
                if let Some(bv) = &mut browse_view {
                    if bv.filtering {
                        match key.code {
                            KeyCode::Char(c) => {
                                bv.filter_query.push(c);
                                bv.clamp_selection();
                            }
                            KeyCode::Backspace => {
                                bv.filter_query.pop();
                                bv.clamp_selection();
                            }
                            KeyCode::Esc | KeyCode::Enter => bv.filtering = false,
                            _ => {}
                        }
                        continue;
                    }
                }

                if subtitle_menu_open {
                    if key.code == KeyCode::Esc
                        || key.code == keymap.get(KeyAction::OpenSubtitleMenu)
                    {
                        subtitle_menu_open = false;
                        config::save_or_warn(&cfg, &mut notifications);
                        continue;
                    }
                    match key.code {
                        KeyCode::Char('j') | KeyCode::Down => {
                            subtitle_menu_selected =
                                (subtitle_menu_selected + 1) % ui::subtitle_menu::FIELD_COUNT;
                        }
                        KeyCode::Char('k') | KeyCode::Up => {
                            subtitle_menu_selected =
                                (subtitle_menu_selected + ui::subtitle_menu::FIELD_COUNT - 1)
                                    % ui::subtitle_menu::FIELD_COUNT;
                        }
                        KeyCode::Char('h') | KeyCode::Left => {
                            adjust_subtitle_field(&mut cfg.subtitles, subtitle_menu_selected, -1.0);
                            player.set_subtitle_style(&cfg.subtitles);
                            subtitle_delay = cfg.subtitles.delay_secs;
                        }
                        KeyCode::Char('l') | KeyCode::Right => {
                            adjust_subtitle_field(&mut cfg.subtitles, subtitle_menu_selected, 1.0);
                            player.set_subtitle_style(&cfg.subtitles);
                            subtitle_delay = cfg.subtitles.delay_secs;
                        }
                        _ => {}
                    }
                    continue;
                }
                if key.code == keymap.get(KeyAction::OpenSubtitleMenu) {
                    subtitle_menu_open = true;
                    continue;
                }

                if settings_menu_open {
                    // the token field's own text-entry sub-mode takes
                    // every key while active, same "claim everything"
                    // priority as editing_query/the filter box above;
                    // esc here only cancels the edit, not the whole menu.
                    if settings_token_editing {
                        match key.code {
                            KeyCode::Char(c) => settings_token_input.push(c),
                            KeyCode::Backspace => {
                                settings_token_input.pop();
                            }
                            KeyCode::Esc => {
                                settings_token_editing = false;
                                settings_token_input.clear();
                            }
                            KeyCode::Enter => {
                                let token = settings_token_input.trim().to_string();
                                cfg.listenbrainz_user_token =
                                    if token.is_empty() { None } else { Some(token) };
                                config::save_or_warn(&cfg, &mut notifications);
                                settings_token_editing = false;
                                settings_token_input.clear();
                            }
                            _ => {}
                        }
                        continue;
                    }
                    if key.code == KeyCode::Esc
                        || key.code == keymap.get(KeyAction::OpenSettingsMenu)
                    {
                        settings_menu_open = false;
                        cfg.crossfade_enabled = crossfade_enabled;
                        config::save_or_warn(&cfg, &mut notifications);
                        continue;
                    }
                    match key.code {
                        KeyCode::Char('j') | KeyCode::Down => {
                            settings_menu_selected =
                                (settings_menu_selected + 1) % ui::settings_menu::FIELD_COUNT;
                        }
                        KeyCode::Char('k') | KeyCode::Up => {
                            settings_menu_selected =
                                (settings_menu_selected + ui::settings_menu::FIELD_COUNT - 1)
                                    % ui::settings_menu::FIELD_COUNT;
                        }
                        // art mode is a cycle (ascii-bw -> ascii-color
                        // -> full-bw -> full-color -> off); both
                        // directions step the same way 'i' already
                        // does, rather than adding a second
                        // reverse-cycle method just for this.
                        KeyCode::Char('h' | 'l') | KeyCode::Left | KeyCode::Right
                            if settings_menu_selected == 0 =>
                        {
                            let (next, skipped) = art_mode.cycle(real_graphics(picker));
                            art_mode = next;
                            if skipped {
                                warn_no_graphics(&mut notifications, &mut warned_no_graphics);
                            }
                        }
                        KeyCode::Char('h') | KeyCode::Left if settings_menu_selected == 1 => {
                            crossfade_enabled = false;
                            fade_in_started_at = None;
                            let _ = player.set_volume(base_volume);
                        }
                        KeyCode::Char('l') | KeyCode::Right if settings_menu_selected == 1 => {
                            crossfade_enabled = true;
                        }
                        KeyCode::Char('h') | KeyCode::Left if settings_menu_selected == 2 => {
                            cfg.crossfade_seconds = (cfg.crossfade_seconds - 0.5).clamp(1.0, 15.0);
                        }
                        KeyCode::Char('l') | KeyCode::Right if settings_menu_selected == 2 => {
                            cfg.crossfade_seconds = (cfg.crossfade_seconds + 0.5).clamp(1.0, 15.0);
                        }
                        KeyCode::Char('h') | KeyCode::Left if settings_menu_selected == 3 => {
                            cfg.cache_days_threshold =
                                cfg.cache_days_threshold.saturating_sub(1).max(1);
                        }
                        KeyCode::Char('l') | KeyCode::Right if settings_menu_selected == 3 => {
                            cfg.cache_days_threshold = (cfg.cache_days_threshold + 1).min(20);
                        }
                        KeyCode::Char('h') | KeyCode::Left if settings_menu_selected == 4 => {
                            cfg.cache_eviction_days =
                                cfg.cache_eviction_days.saturating_sub(1).max(1);
                        }
                        KeyCode::Char('l') | KeyCode::Right if settings_menu_selected == 4 => {
                            cfg.cache_eviction_days = (cfg.cache_eviction_days + 1).min(90);
                        }
                        KeyCode::Char('h') | KeyCode::Left if settings_menu_selected == 5 => {
                            cfg.scrobble.threshold_percent =
                                (cfg.scrobble.threshold_percent - 5.0).clamp(5.0, 100.0);
                        }
                        KeyCode::Char('l') | KeyCode::Right if settings_menu_selected == 5 => {
                            cfg.scrobble.threshold_percent =
                                (cfg.scrobble.threshold_percent + 5.0).clamp(5.0, 100.0);
                        }
                        KeyCode::Char('h') | KeyCode::Left if settings_menu_selected == 6 => {
                            cfg.scrobble.threshold_seconds =
                                (cfg.scrobble.threshold_seconds - 15.0).clamp(15.0, 600.0);
                        }
                        KeyCode::Char('l') | KeyCode::Right if settings_menu_selected == 6 => {
                            cfg.scrobble.threshold_seconds =
                                (cfg.scrobble.threshold_seconds + 15.0).clamp(15.0, 600.0);
                        }
                        KeyCode::Enter if settings_menu_selected == 7 => {
                            settings_token_editing = true;
                            settings_token_input.clear();
                        }
                        _ => {}
                    }
                    continue;
                }
                if key.code == keymap.get(KeyAction::OpenSettingsMenu) {
                    settings_menu_open = true;
                    continue;
                }

                // opened via enter on the bottom bar's Eq field (see
                // the `Focus::BottomBar` block further down), not a
                // dedicated keymap action of its own; esc is the only
                // way to close it.
                // the undo tree owns the keyboard while it is up, the
                // same way the eq menu does. undo and redo go through
                // `apply_op` rather than the recorder directly, so
                // there stays exactly one path that records, refuses
                // at a barrier and reports.
                if undo_menu_open {
                    if matches!(key.code, KeyCode::Esc | KeyCode::Char('q')) {
                        undo_menu_open = false;
                        continue;
                    }
                    // the hint line reads these same constants, so it
                    // cannot advertise a key this match does not act
                    // on; k/j and the arrows are undocumented
                    // synonyms, which is fine for a direction nobody
                    // has to be told.
                    let op = match key.code {
                        KeyCode::Char(ui::undo_tree::UNDO_KEY)
                        | KeyCode::Char('k')
                        | KeyCode::Up => Some(shellsong::Op::Undo),
                        KeyCode::Char(ui::undo_tree::REDO_KEY)
                        | KeyCode::Char('j')
                        | KeyCode::Down => Some(shellsong::Op::Redo),
                        _ => None,
                    };
                    if let Some(op) = op {
                        let mut surf = wim_surface!();
                        let mut core = core_ctx!();
                        apply_op(op, &mut core, &mut surf);
                        continue;
                    }
                    // h/l walk across a fork, which is the one thing
                    // that needs this view open to be usable at all.
                    let across = match key.code {
                        KeyCode::Char(ui::undo_tree::LEFT_KEY) | KeyCode::Left => Some(-1),
                        KeyCode::Char(ui::undo_tree::RIGHT_KEY) | KeyCode::Right => Some(1),
                        _ => None,
                    };
                    if let Some(delta) = across {
                        let mut surf = wim_surface!();
                        let mut core = core_ctx!();
                        match core.undo.switch_branch(delta) {
                            Some(snapshot) => {
                                if let Err(why) = snapshot.restore(&mut core) {
                                    core.notifications.push(Severity::Error, why);
                                }
                            }
                            None => core
                                .notifications
                                .push(Severity::Info, "no other branch here"),
                        }
                        let _ = &mut surf;
                        continue;
                    }
                    continue;
                }

                if eq_menu_open {
                    // the name editor owns every key while it's up, so
                    // a letter types instead of firing a menu action.
                    if eq_menu.editor.is_some() {
                        if let Some(mode) = eq_menu_editor_key(
                            key.code,
                            &mut eq_menu,
                            &mut cfg.eq_custom_presets,
                            &mut app,
                        ) {
                            persist_eq(&mut cfg, &app, &mut notifications);
                            eq_menu_selected = match mode {
                                // a save lands the cursor on the row
                                // the curve now lives on; a rename
                                // leaves it on the row it renamed.
                                ui::eq_menu::EditorMode::Save => ui::eq_menu::opening_selection(
                                    app.eq_source,
                                    cfg.eq_custom_presets.len(),
                                ),
                                ui::eq_menu::EditorMode::Rename(i) => ui::eq_menu::PRESET_COUNT + i,
                            };
                        }
                        continue;
                    }
                    let saved_len = cfg.eq_custom_presets.len();
                    let at = ui::eq_menu::cursor(eq_menu_selected, saved_len);
                    // any key other than a second delete cancels a
                    // pending one, including moving off its row; same
                    // rule the navidrome profile manager follows.
                    if key.code != ui::eq_menu::DELETE_KEY {
                        eq_menu.disarm_delete();
                    }
                    if key.code == KeyCode::Esc {
                        eq_menu_open = false;
                        eq_menu = ui::eq_menu::EqMenuState::default();
                        persist_eq(&mut cfg, &app, &mut notifications);
                        continue;
                    }
                    if key.code == ui::eq_menu::SAVE_KEY {
                        eq_menu.editor = Some(ui::eq_menu::NameEditor::save());
                        eq_menu.message.clear();
                        continue;
                    }
                    if key.code == ui::eq_menu::RENAME_KEY {
                        if let ui::eq_menu::Cursor::Saved(i) = at {
                            let current = cfg.eq_custom_presets.name_at(i).unwrap_or("");
                            eq_menu.editor = Some(ui::eq_menu::NameEditor::rename(i, current));
                            eq_menu.message.clear();
                        } else {
                            eq_menu.message = eq_saved_row_hint(&cfg, "rename");
                        }
                        continue;
                    }
                    if key.code == ui::eq_menu::DELETE_KEY {
                        let ui::eq_menu::Cursor::Saved(i) = at else {
                            eq_menu.message = eq_saved_row_hint(&cfg, "delete");
                            continue;
                        };
                        let name = cfg
                            .eq_custom_presets
                            .name_at(i)
                            .unwrap_or_default()
                            .to_string();
                        // first press arms, a second on the same row
                        // commits; this rewrites config.toml and the
                        // curve is gone.
                        if eq_menu.delete.press(i) {
                            cfg.eq_custom_presets.remove(i);
                            // the applied curve stays audible either
                            // way, it just loses its name when the
                            // preset behind it is the one deleted.
                            app.eq_source = eq::source_after_removal(app.eq_source, i);
                            app.eq_custom = app.eq_source.is_some() && app.eq_custom;
                            persist_eq(&mut cfg, &app, &mut notifications);
                            eq_menu_selected = eq_menu_selected.min(
                                ui::eq_menu::selectable_count(cfg.eq_custom_presets.len()) - 1,
                            );
                            eq_menu.message = format!("deleted '{name}'");
                        } else {
                            eq_menu.message = format!(
                                "press {} again to delete '{name}'",
                                keymap::key_label(ui::eq_menu::DELETE_KEY)
                            );
                        }
                        continue;
                    }
                    match key.code {
                        // clamped, not wrapped, and a preset is only
                        // applied on enter: landing on one with j/k
                        // used to apply it instantly, so scrolling off
                        // the last band wrapped around to "flat" and
                        // silently overwrote all ten hand-tuned gains,
                        // which esc then saved.
                        KeyCode::Char('j') | KeyCode::Down => {
                            eq_menu_selected = (eq_menu_selected + 1)
                                .min(ui::eq_menu::selectable_count(saved_len) - 1);
                        }
                        KeyCode::Char('k') | KeyCode::Up => {
                            eq_menu_selected = eq_menu_selected.saturating_sub(1);
                        }
                        KeyCode::Enter => {
                            apply_eq_row(
                                &mut app,
                                &player,
                                &mut notifications,
                                &cfg.eq_custom_presets,
                                eq_menu_selected,
                            );
                        }
                        KeyCode::Char('h') | KeyCode::Left => {
                            if let ui::eq_menu::Cursor::Band(band) = at {
                                adjust_eq_band(&mut app, &player, &mut notifications, band, -1.0);
                            }
                        }
                        KeyCode::Char('l') | KeyCode::Right => {
                            if let ui::eq_menu::Cursor::Band(band) = at {
                                adjust_eq_band(&mut app, &player, &mut notifications, band, 1.0);
                            }
                        }
                        _ => {}
                    }
                    continue;
                }

                if keybinds_menu_open {
                    if capturing_key {
                        // esc cancels the capture without changing
                        // anything, same "back out" meaning it has
                        // everywhere else; it's also the one key that
                        // can never itself be captured, since it's
                        // the only way out of a capture gone wrong.
                        if key.code != KeyCode::Esc {
                            let action = keymap::ALL_ACTIONS[keybinds_menu_selected];
                            keybind_conflict = keymap
                                .action_bound_to(key.code)
                                .filter(|owner| *owner != action);
                            keymap.set(action, key.code);
                            cfg.keybinds = keymap.overrides();
                            config::save_or_warn(&cfg, &mut notifications);
                        }
                        capturing_key = false;
                        continue;
                    }
                    match key.code {
                        KeyCode::Char('K') | KeyCode::Esc => keybinds_menu_open = false,
                        KeyCode::Char('j') | KeyCode::Down => {
                            keybinds_menu_selected =
                                (keybinds_menu_selected + 1) % keymap::ALL_ACTIONS.len();
                            keybind_conflict = None;
                        }
                        KeyCode::Char('k') | KeyCode::Up => {
                            keybinds_menu_selected =
                                (keybinds_menu_selected + keymap::ALL_ACTIONS.len() - 1)
                                    % keymap::ALL_ACTIONS.len();
                            keybind_conflict = None;
                        }
                        KeyCode::Char('r') => {
                            capturing_key = true;
                            keybind_conflict = None;
                        }
                        KeyCode::Char('0') => {
                            keymap.reset(keymap::ALL_ACTIONS[keybinds_menu_selected]);
                            cfg.keybinds = keymap.overrides();
                            config::save_or_warn(&cfg, &mut notifications);
                        }
                        KeyCode::Char('R') => {
                            keymap.reset_all();
                            cfg.keybinds = keymap.overrides();
                            config::save_or_warn(&cfg, &mut notifications);
                        }
                        _ => {}
                    }
                    continue;
                }
                if key.code == KeyCode::Char('K') {
                    keybinds_menu_open = true;
                    keybinds_menu_selected = 0;
                    continue;
                }

                // move mode owns the keyboard outright while it's
                // active: the whole point is that wasd/arrows move
                // focus and *nothing else happens*, so nothing below
                // this ever sees these keys. directional and
                // sway/i3-style, not three absolute "go to pane X"
                // bindings: d/right from the sidebar reaches the
                // workspace, a/left back, s/down from either reaches
                // the bottom bar, w/up from there always back to the
                // sidebar; anything with nowhere further to go that
                // way is a no-op rather than an arbitrary fallback.
                // placed after the modal menus (which own tab/enter
                // themselves while open) but ahead of the
                // browse/lyrics blocks, so an open album can never
                // swallow a focus move.
                if input_mode == InputMode::Move {
                    match key.code {
                        KeyCode::Tab | KeyCode::Enter | KeyCode::Esc => {
                            input_mode = InputMode::Interact;
                        }
                        code => {
                            if let Some(dir) = focus_direction_key(code) {
                                focus = focus_in_direction(focus, dir);
                            }
                        }
                    }
                    continue;
                }
                if key.code == KeyCode::Tab {
                    input_mode = InputMode::Move;
                    continue;
                }

                if panels.resize_mode {
                    if key.code == KeyCode::Esc
                        || key.code == keymap.get(KeyAction::ToggleResizeMode)
                    {
                        panels.toggle_resize_mode();
                        // persist on leaving the mode rather than on
                        // every arrow press: a resize is a handful of
                        // keystrokes, and the sizes used to reset to
                        // the defaults on every restart.
                        if cfg.sidebar_width != panels.sidebar_width
                            || cfg.bottom_bar_height != panels.bottom_bar_height
                        {
                            cfg.sidebar_width = panels.sidebar_width;
                            cfg.bottom_bar_height = panels.bottom_bar_height;
                            config::save_or_warn(&cfg, &mut notifications);
                        }
                        continue;
                    }
                    // each panel only answers to its one valid resize
                    // axis: sidebar is a vertical strip (left/right
                    // only), bottom bar is a horizontal strip (up/down
                    // only); the other axis is a no-op, not a no-op
                    // that happens to also work the "wrong" way. wasd
                    // here is fixed, not remapped: a transient mode's
                    // own panel-pick, not a named command.
                    match (panels.resize_selected, key.code) {
                        (_, KeyCode::Char('a' | 'w')) => panels.select(ResizePanel::Sidebar),
                        (_, KeyCode::Char('s' | 'd')) => panels.select(ResizePanel::BottomBar),
                        (ResizePanel::Sidebar, KeyCode::Left) => panels.shrink(),
                        (ResizePanel::Sidebar, KeyCode::Right) => panels.grow(),
                        (ResizePanel::BottomBar, KeyCode::Up) => panels.grow(),
                        (ResizePanel::BottomBar, KeyCode::Down) => panels.shrink(),
                        _ => {}
                    }
                    continue;
                }
                if key.code == keymap.get(KeyAction::ToggleResizeMode) {
                    panels.toggle_resize_mode();
                    continue;
                }

                // scrub/drag mode, entered with enter on the bottom
                // bar's scrubber. ahead of everything else below
                // (including the bottom bar's own selection keys): a
                // drag owns left/right and enter outright until it's
                // committed, so nothing can move the cursor out from
                // under an open seek.
                if let Some(s) = scrub {
                    let coarse = key.modifiers.contains(KeyModifiers::SHIFT);
                    let step = if coarse {
                        SCRUB_STEP_COARSE
                    } else {
                        SCRUB_STEP
                    };
                    match key.code {
                        KeyCode::Left | KeyCode::Char('h' | 'H') => {
                            scrub = Some(s.step(-step));
                            scrub_pending = true;
                        }
                        KeyCode::Right | KeyCode::Char('l' | 'L') => {
                            scrub = Some(s.step(step));
                            scrub_pending = true;
                        }
                        // both commit: the target was already applied
                        // on the way there, so there's nothing esc
                        // could roll back to that wouldn't itself be
                        // another seek the user didn't ask for.
                        KeyCode::Enter | KeyCode::Esc => commit_scrub(
                            s,
                            &player,
                            &mut notifications,
                            &mut scrub,
                            &mut scrub_pending,
                            &mut scrub_sent_at,
                        ),
                        _ => {}
                    }
                    continue;
                }

                // pane-local navigation for whichever surface is
                // showing the transport: the bottom bar, or the
                // maximized player, which draws the same `Field` set
                // bigger. Same "only the arrows and enter care which
                // pane has focus" rule as the browse view further
                // down: global playback controls (space/f/b/c/etc.)
                // fall through untouched below regardless of focus,
                // only these keys are intercepted here. The cursor
                // differs per pane, the dispatch below does not; see
                // `controlsbar::nav` for why only the bar gets h/l.
                let player_transport = now_playing.is_some()
                    && !player_minimized
                    && !now_playing_is_video(&now_playing)
                    && focus == Focus::Workspace;
                let transport = match focus {
                    Focus::BottomBar => Some((&mut bottombar_selected, NavKeys::BarStrip)),
                    _ if player_transport => Some((&mut player_selected, NavKeys::ArrowsOnly)),
                    _ => None,
                };
                if let Some((cursor, nav_keys)) = transport {
                    match controlsbar::nav(key.code, nav_keys) {
                        Some(Nav::Next) => {
                            *cursor = cursor.next();
                            continue;
                        }
                        Some(Nav::Prev) => {
                            *cursor = cursor.prev();
                            continue;
                        }
                        Some(Nav::Inert) => continue,
                        Some(Nav::Activate) => {
                            let field = *cursor;
                            let mut tctx = TransportCtx {
                                scrub: &mut scrub,
                                scrub_pending: &mut scrub_pending,
                                scrub_sent_at: &mut scrub_sent_at,
                                elapsed_secs: playback.elapsed_secs,
                                duration_secs: playing_duration(
                                    now_playing.as_ref(),
                                    playback.duration_secs,
                                ),
                            };
                            let mut surf = wim_surface!();
                            let mut core = core_ctx!();
                            activate_transport(
                                field,
                                ScrubAt::Position,
                                &mut core,
                                &mut surf,
                                &mut tctx,
                            );
                            continue;
                        }
                        None => {}
                    }
                }

                // same condition the render side uses to decide the
                // banner is what's actually on screen (not the full
                // player, a browse view, or lyrics). Not gated on
                // workspace focus: left/right have no other binding
                // while the banner's showing regardless of which pane
                // has focus (sidebar navigation uses j/k/up/down, not
                // left/right), and gating it on a focus state that
                // defaults to Sidebar meant the feature did nothing
                // and showed no `<`/`>` hint at all out of the box.
                let banner_showing = (now_playing.is_none() || player_minimized)
                    && browse_view.is_none()
                    && !lyrics.is_open();
                if banner_showing {
                    match key.code {
                        KeyCode::Left => {
                            logo_index = (logo_index + ui::banner::logo_count() - 1)
                                % ui::banner::logo_count();
                            cfg.logo_style = ui::banner::logo_name(logo_index).map(String::from);
                            config::save_or_warn(&cfg, &mut notifications);
                            continue;
                        }
                        KeyCode::Right => {
                            logo_index = (logo_index + 1) % ui::banner::logo_count();
                            cfg.logo_style = ui::banner::logo_name(logo_index).map(String::from);
                            config::save_or_warn(&cfg, &mut notifications);
                            continue;
                        }
                        KeyCode::Char('0') => {
                            logo_index = 0;
                            cfg.logo_style = None;
                            config::save_or_warn(&cfg, &mut notifications);
                            continue;
                        }
                        _ => {}
                    }
                }

                // closing what's open doesn't depend on which pane has
                // focus; the global playback controls below (space/f/
                // b/m/i) don't either, only pane-local navigation
                // does (see the Focus match further down).
                if browse_view.is_some() && matches!(key.code, KeyCode::Esc | KeyCode::Char('q')) {
                    browse_view = None;
                    // whatever was still fetching for it must not
                    // reopen it when it answers.
                    browse_loader.invalidate();
                    continue;
                }

                if key.code == keymap.get(KeyAction::CycleArtMode) {
                    // a full mode is drawn by overlaying an image on
                    // the rect the widget left empty, so with no
                    // graphics protocol it is not "ascii anyway", it is
                    // a blank pane; `cycle` steps past those and says
                    // so once rather than letting `i` look broken.
                    let (next, skipped) = art_mode.cycle(real_graphics(picker));
                    art_mode = next;
                    if skipped {
                        warn_no_graphics(&mut notifications, &mut warned_no_graphics);
                    }
                    continue;
                }

                // browse view intercepts navigation the same way
                // lyrics does (below): scrolling/picking a track
                // shouldn't move the underlying sidebar selection,
                // but only while the workspace actually has focus;
                // with focus on the sidebar, j/k/enter fall through
                // to `app.handle_key` instead, moving/acting on the
                // search results behind this view. enter plays from
                // the selected track to the end of the album/playlist
                // (or, with the artist profile's ALBUMS column
                // focused, opens that album instead; see `open_album`
                // below, computed here but applied after this borrow
                // of `browse_view` ends).
                let mut open_album: Option<SearchItem> = None;
                let mut open_suggestion: Option<SearchItem> = None;
                if let (Some(bv), Focus::Workspace) = (&mut browse_view, focus) {
                    if bv.is_artist && key.code == keymap.get(KeyAction::StartSearch) {
                        bv.filtering = true;
                        continue;
                    }
                    if bv.is_artist && matches!(key.code, KeyCode::Left | KeyCode::Right) {
                        bv.albums_focused = !bv.albums_focused;
                        continue;
                    }
                    // only `continue` for keys this block actually
                    // does something with; everything else has to
                    // fall through to the global handlers further
                    // down, or a focused browse view silently ate
                    // every other keybind in the app (navidrome
                    // profiles, starred songs, crossfade, skip/back,
                    // all of it) the moment an album was open, which
                    // is exactly the "pressing shift+s and nothing
                    // happens" bug this fixes.
                    let mut handled = true;
                    match key.code {
                        KeyCode::Char('j') | KeyCode::Down => bv.next(),
                        KeyCode::Char('k') | KeyCode::Up => bv.prev(),
                        KeyCode::Enter if bv.albums_focused => {
                            open_album = bv.albums.get(bv.albums_selected).cloned();
                        }
                        KeyCode::Enter => {
                            // the selected row through the end of
                            // what's actually visible, not of the
                            // whole list: a filtered artist page used
                            // to play the unfiltered tail instead.
                            let session = browse_session(bv);
                            if let Some(first) = session.first().cloned() {
                                let interrupted = play_queue.current().cloned();
                                let (s, played) = load_playlist(
                                    &player,
                                    &cache_index,
                                    &mut play_queue,
                                    session,
                                    SessionStart::New,
                                );
                                if played {
                                    fire_subtitle_fetch(&cfg, &subtitle_tx, &first);
                                    let mut dctx = DownloadCtx {
                                        cache: &cache_index,
                                        download_tx: &download_tx,
                                        downloading: &mut downloading,
                                        day_threshold: cfg.cache_days_threshold,
                                    };
                                    note_playback_started(&first, &mut dctx, &mut notifications);
                                    start_tracking_playback(&cfg, &first, &mut scrobble);
                                    previous_track = interrupted;
                                    // keep the album/playlist listing
                                    // visible rather than replacing it
                                    // with the full player.
                                    player_minimized = true;
                                } else {
                                    notifications.push(Severity::Error, s);
                                }
                            }
                        }
                        _ if key.code == keymap.get(KeyAction::ToggleStar)
                            && !bv.albums_focused =>
                        {
                            if let Some(item) = bv.tracks.get(bv.selected).cloned() {
                                // the row's own "*" marker is the
                                // feedback; only a failure needs words.
                                if let Err(msg) = toggle_star(
                                    &item,
                                    &mut favorites,
                                    &mut remote_starred,
                                    &star_tx,
                                ) {
                                    notifications.push(Severity::Error, msg);
                                }
                            }
                        }
                        // whole-album download when the artist view's
                        // ALBUMS column has focus (every song on that
                        // album), the single selected track otherwise;
                        // same albums_focused/song-list split `enter`
                        // already uses just above.
                        _ if key.code == keymap.get(KeyAction::DownloadSelected) => {
                            let target = if bv.albums_focused {
                                bv.albums.get(bv.albums_selected).cloned()
                            } else {
                                bv.tracks.get(bv.selected).cloned()
                            };
                            if let Some(item) = target {
                                let mut dctx = DownloadCtx {
                                    cache: &cache_index,
                                    download_tx: &download_tx,
                                    downloading: &mut downloading,
                                    day_threshold: cfg.cache_days_threshold,
                                };
                                download_selected(
                                    &item,
                                    &mut track_resolver,
                                    &mut dctx,
                                    &mut notifications,
                                );
                            }
                        }
                        _ => handled = false,
                    }
                    // `open_album` deliberately falls *through* to the
                    // block right below, which is what actually opens
                    // it; swallowing the key here just because it was
                    // handled meant enter on an album in the artist
                    // profile set `open_album` and then immediately
                    // skipped past the only code that reads it, so
                    // enter looked completely dead.
                    if handled && open_album.is_none() {
                        continue;
                    }
                }
                if let Some(album) = open_album {
                    open_browse(
                        &album,
                        &mut browse_loader,
                        &mut browse_view,
                        &mut browse_art,
                        &mut focus,
                    );
                    continue;
                }

                // lyrics view intercepts input directly rather than going
                // through App, since scrolling it shouldn't move the
                // underlying results selection.
                if lyrics.is_open() {
                    // same fix as the browse-view block above: only
                    // swallow keys actually handled here, or lyrics
                    // being open silently ate every other keybind.
                    let mut handled = true;
                    match key.code {
                        // clamped to the content: scrolling past the
                        // last line just parks the view on blank space.
                        KeyCode::Char('j') | KeyCode::Down => {
                            {
                                // against the *wrapped* length, which
                                // is what's actually on screen; the
                                // raw line count under-counts once
                                // long lines wrap.
                                let rows = lyrics
                                    .lines()
                                    .map(|l| {
                                        let borrowed: Vec<&str> =
                                            l.iter().map(String::as_str).collect();
                                        ui::lyrics_view::wrapped_len(&borrowed, lyrics.width())
                                    })
                                    .unwrap_or(0);
                                lyrics.set_scroll(ui::scroll::clamp_to_content(
                                    lyrics.scroll() + 1,
                                    rows,
                                ));
                            }
                        }
                        KeyCode::Char('k') | KeyCode::Up => {
                            lyrics.set_scroll(lyrics.scroll().saturating_sub(1))
                        }
                        KeyCode::Esc | KeyCode::Char('q') => {
                            lyrics.close();
                        }
                        _ if key.code == keymap.get(KeyAction::OpenLyrics) => {
                            lyrics.close();
                        }
                        _ => handled = false,
                    }
                    if handled {
                        continue;
                    }
                }

                if key.code == keymap.get(KeyAction::IndexBreakdown) {
                    let text = app
                        .corpus
                        .breakdown()
                        .iter()
                        .map(|(src, n)| format!("{src}: {n}"))
                        .collect::<Vec<_>>()
                        .join(", ");
                    notifications.push(Severity::Info, text);
                    continue;
                }

                if key.code == keymap.get(KeyAction::OpenNavManager) {
                    let mut nm = NavManager::new();
                    nm.connection_status = test_navidrome_connectivity(
                        &cfg.navidrome_servers,
                        &app.corpus.remote_clients(),
                    );
                    for (nav, result) in cfg.navidrome_servers.iter().zip(&nm.connection_status) {
                        if *result == Some(false) {
                            notifications
                                .push(Severity::Alert, format!("{} is unreachable", nav.url));
                        }
                    }
                    nav_manager = Some(nm);
                    continue;
                }

                if let (Some(sug), Focus::Workspace) = (&suggestions, focus) {
                    let total = ui::suggestions::count(sug);
                    let mut handled = true;
                    match key.code {
                        KeyCode::Char('j') | KeyCode::Down => {
                            suggestions_selected =
                                (suggestions_selected + 1).min(total.saturating_sub(1));
                        }
                        KeyCode::Char('k') | KeyCode::Up => {
                            suggestions_selected = suggestions_selected.saturating_sub(1);
                        }
                        KeyCode::Enter => {
                            // a suggestion is a plain SearchItem, so
                            // enter goes through the same dispatch a
                            // search result does; no second code path.
                            open_suggestion = sug
                                .flat()
                                .nth(suggestions_selected)
                                .map(|(_, s)| s.item.clone());
                        }
                        KeyCode::Esc => {
                            suggestions = None;
                            focus = Focus::Sidebar;
                        }
                        _ => handled = false,
                    }
                    if handled && open_suggestion.is_none() {
                        continue;
                    }
                }
                if let Some(item) = open_suggestion {
                    suggestions = None;
                    // a suggestion row is a plain SearchItem, so this is
                    // the same Action a search result's enter produces:
                    // songs play, albums and artists open their page.
                    let mut surf = wim_surface!();
                    let mut core = core_ctx!();
                    let dispatch = dispatch_action(app::Action::Play(item), &mut core, &mut surf);
                    if dispatch == Dispatch::Quit {
                        break;
                    }
                    continue;
                }

                if key.code == keymap.get(KeyAction::OpenSuggestions) {
                    if suggestions.is_some() {
                        suggestions = None;
                        focus = Focus::Sidebar;
                    } else {
                        // opens instantly off what is already known;
                        // the background fetches that make the
                        // network-fed generators fire are kicked off
                        // by the main loop's enrichment probe, and the
                        // menu refills itself as they land.
                        let starred = suggestion_starred_keys(&remote_starred, &favorites);
                        suggestions = Some(recompute_suggestions(
                            &mut suggest_catalog,
                            &app.corpus,
                            &cache_index,
                            &starred,
                            &enrichment,
                        ));
                        suggestions_selected = 0;
                        focus = Focus::Workspace;
                    }
                    continue;
                }

                // the queue view's own navigation, same shape as the
                // browse/lyrics tiers: only swallow keys it actually
                // handles, or an open view eats every other keybind.
                if queue_view_open && focus == Focus::Workspace {
                    let order = ui::queue::selectable(play_queue.entries(), play_queue.pos());
                    let at = order.iter().position(|i| *i == queue_selected).unwrap_or(0);
                    let mut handled = true;
                    match key.code {
                        KeyCode::Char('j') | KeyCode::Down => {
                            if let Some(next) = order.get(at + 1) {
                                queue_selected = *next;
                            }
                        }
                        KeyCode::Char('k') | KeyCode::Up => {
                            if at > 0 {
                                if let Some(prev) = order.get(at - 1) {
                                    queue_selected = *prev;
                                }
                            }
                        }
                        KeyCode::Enter => {
                            // jumping is a playlist-pos write, not a
                            // reload: mpv keeps the same playlist so
                            // `entries` stays index-aligned with it.
                            if order.contains(&queue_selected) {
                                if let Err(e) = player
                                    .set_property("playlist-pos", serde_json::json!(queue_selected))
                                {
                                    notifications
                                        .push(Severity::Error, format!("error jumping: {e}"));
                                }
                            }
                        }
                        KeyCode::Char('d') => {
                            if play_queue.remove(queue_selected) {
                                if let Err(e) = player.playlist_remove(queue_selected) {
                                    notifications
                                        .push(Severity::Error, format!("error removing: {e}"));
                                }
                                // the cursor keeps its position in the
                                // list rather than its index, so
                                // holding 'd' walks down the queue.
                                let left = play_queue.entries().len();
                                queue_selected = queue_selected.min(left.saturating_sub(1));
                            }
                        }
                        // reorder is upcoming-only: moving a played
                        // entry does nothing anyone wants, and in the
                        // played run the display is reversed, so J
                        // would visibly move a row the wrong way.
                        KeyCode::Char('J') | KeyCode::Char('K') => {
                            let down = key.code == KeyCode::Char('J');
                            let first_upcoming = play_queue.pos().map_or(0, |p| p + 1);
                            let last = play_queue.entries().len().saturating_sub(1);
                            let target = if down {
                                queue_selected + 1
                            } else {
                                queue_selected.saturating_sub(1)
                            };
                            if queue_selected < first_upcoming {
                                notifications
                                    .push(Severity::Info, "only upcoming tracks can be reordered");
                            } else if target >= first_upcoming
                                && target <= last
                                && target != queue_selected
                                && play_queue.move_entry(queue_selected, target)
                            {
                                if let Err(e) = player.playlist_move(queue_selected, target) {
                                    notifications
                                        .push(Severity::Error, format!("error moving: {e}"));
                                }
                                // follow the row that moved, not the
                                // slot it left.
                                queue_selected = target;
                            }
                        }
                        KeyCode::Esc => {
                            queue_view_open = false;
                            focus = Focus::Sidebar;
                        }
                        _ => handled = false,
                    }
                    if handled {
                        continue;
                    }
                }

                if key.code == keymap.get(KeyAction::OpenQueueView) {
                    queue_view_open = !queue_view_open;
                    if queue_view_open {
                        // land on what's playing, which is what you
                        // opened it to see, falling back to the top.
                        queue_selected = play_queue.pos().unwrap_or(0);
                        focus = Focus::Workspace;
                    } else {
                        focus = Focus::Sidebar;
                    }
                    continue;
                }

                if key.code == keymap.get(KeyAction::OpenStarredView) {
                    // the loader owns this view, so opening it bumps
                    // the generation and disowns anything in flight.
                    let bv = open_starred(
                        &mut browse_loader,
                        &cfg.navidrome_servers,
                        &favorites,
                        &app.corpus.remote_clients(),
                    );
                    browse_view = Some(bv);
                    // same as opening an album: the starred list draws
                    // a selection cursor, so j/k have to move *it*
                    // rather than the hidden sidebar behind it.
                    focus = Focus::Workspace;
                    continue;
                }

                if key.code == keymap.get(KeyAction::ToggleMinimize) && now_playing.is_some() {
                    player_minimized = !player_minimized;
                    continue;
                }

                if key.code == keymap.get(KeyAction::ToggleFullscreenVideo)
                    && now_playing_is_video(&now_playing)
                {
                    // `off` has no vo to hand the screen to, so the key
                    // would blank the ui and show nothing; say which
                    // option turns it on rather than doing that.
                    if cfg.video_output == config::VideoOutput::Off {
                        notifications
                            .push(Severity::Alert, "video: video_output is off in config.toml");
                    } else {
                        video_fullscreen = !video_fullscreen;
                    }
                    continue;
                }

                if key.code == keymap.get(KeyAction::ToggleCrossfade) {
                    crossfade_enabled = !crossfade_enabled;
                    if !crossfade_enabled {
                        fade_in_started_at = None;
                        let _ = player.set_volume(base_volume);
                    }
                    flash = Some((Field::Crossfade, Instant::now()));
                    continue;
                }

                // quick delay-only nudge, kept alongside the full 't'
                // menu since this is the one knob worth a dedicated
                // half-second-step shortcut while actually watching
                // something out of sync. editing_query is already
                // ruled out above.
                if key.code == keymap.get(KeyAction::SubtitleDelayDown)
                    || key.code == keymap.get(KeyAction::SubtitleDelayUp)
                {
                    subtitle_delay += if key.code == keymap.get(KeyAction::SubtitleDelayDown) {
                        -0.5
                    } else {
                        0.5
                    };
                    if let Err(e) =
                        player.set_property("sub-delay", serde_json::json!(subtitle_delay))
                    {
                        notifications.push(Severity::Error, format!("subtitle delay: {e}"));
                    }
                    notifications.push(
                        Severity::Info,
                        format!("subtitle delay: {subtitle_delay:+.1}s"),
                    );
                    continue;
                }

                // not while typing a query; editing_query is already
                // ruled out above, so this only ever sees it as a
                // transport command.
                if key.code == keymap.get(KeyAction::TogglePause) && now_playing.is_some() {
                    let _ = player.toggle_pause();
                    continue;
                }

                if key.code == keymap.get(KeyAction::SkipNext) && now_playing.is_some() {
                    let _ = player.playlist_next();
                    continue;
                }

                if key.code == keymap.get(KeyAction::SkipBack) && previous_track.is_some() {
                    let mut dctx = DownloadCtx {
                        cache: &cache_index,
                        download_tx: &download_tx,
                        downloading: &mut downloading,
                        day_threshold: cfg.cache_days_threshold,
                    };
                    let mut hooks = TrackChangeCtx {
                        dctx: &mut dctx,
                        scrobble: &mut scrobble,
                        subtitle_tx: &subtitle_tx,
                    };
                    let _ = go_to_previous(
                        &cfg,
                        &player,
                        &mut notifications,
                        &mut previous_track,
                        &mut play_queue,
                        &mut hooks,
                    );
                    continue;
                }

                if key.code == keymap.get(KeyAction::ToggleStar) {
                    // the browse-view-focused case is handled inside
                    // its own intercept above; this runs whenever
                    // nothing's being browsed, or something is but the
                    // sidebar has focus, so it's always the sidebar's
                    // own selected search result here.
                    if let Some(item) = app.results.get(app.selected).cloned() {
                        if let Err(msg) =
                            toggle_star(&item, &mut favorites, &mut remote_starred, &star_tx)
                        {
                            notifications.push(Severity::Error, msg);
                        }
                    }
                    continue;
                }

                if key.code == keymap.get(KeyAction::DownloadSelected) {
                    // the browse-view-and-focused case is handled
                    // inside its own intercept above; this runs
                    // whenever nothing's being browsed, or something
                    // is but the sidebar has focus, so it's always
                    // the sidebar's own selected search result here
                    // (same split `ToggleStar` already uses).
                    if let Some(item) = app.results.get(app.selected).cloned() {
                        let mut dctx = DownloadCtx {
                            cache: &cache_index,
                            download_tx: &download_tx,
                            downloading: &mut downloading,
                            day_threshold: cfg.cache_days_threshold,
                        };
                        download_selected(
                            &item,
                            &mut track_resolver,
                            &mut dctx,
                            &mut notifications,
                        );
                    }
                    continue;
                }

                if key.code == keymap.get(KeyAction::OpenLyrics) {
                    // only the paths that *don't* put something on
                    // screen say anything; a successful lookup opens
                    // the lyrics view, which is its own answer.
                    match &now_playing {
                        Some(item) => match lyrics_query(item) {
                            Some((artist, title, album)) => {
                                // asks and returns; the reply lands in
                                // the main loop's drain. pressing this
                                // used to block the whole ui on lrclib.
                                lyrics_loader.request(
                                    &lyrics_client,
                                    label(item),
                                    &artist,
                                    &title,
                                    &album,
                                    None,
                                );
                            }
                            None => notifications.push(Severity::Info, "no lyrics for this item"),
                        },
                        None => notifications.push(Severity::Info, "nothing playing"),
                    }
                    continue;
                }

                let action = app.handle_key(key.code, &keymap);
                let mut surf = wim_surface!();
                let mut core = core_ctx!();
                let dispatch = dispatch_action(action, &mut core, &mut surf);

                if dispatch == Dispatch::Quit {
                    break;
                }
            }
        }
    }
    // mpv has to be gone before the guard hands the terminal back, so
    // this exit path is ordered the other way round from the usual one.
    // a vo entered the alternate screen on init and mpv only leaves it
    // when the process exits, not when the vo is torn down (measured),
    // so `Mpv`'s drop running after the guard's puts that ?1049l on the
    // user's shell, where it restores a cursor position the shell never
    // saved.
    if video_vo.take().is_some() {
        let _ = player.set_video_output(video::Vo::Null.mpv_name());
        drop(player);
    }
    drop(guard);

    Ok(())
}

#[cfg(test)]
mod download_policy_tests {
    use super::*;
    use wim::core::should_auto_download;

    #[test]
    fn two_plays_on_one_day_never_trigger_a_download() {
        // "if i press enter on a song twice it downloads it, thats not
        // what i want": the play count used to be what crossed the
        // threshold, so it did.
        let mut idx = cache::CacheIndex::default();
        let days = idx.record_play("t1", "2026-03-01");
        assert!(!should_auto_download(days, 2, false, false));
        let days = idx.record_play("t1", "2026-03-01");
        assert!(!should_auto_download(days, 2, false, false));
    }

    #[test]
    fn two_distinct_days_do_trigger_a_download() {
        let mut idx = cache::CacheIndex::default();
        idx.record_play("t1", "2026-03-01");
        let days = idx.record_play("t1", "2026-03-02");
        assert!(should_auto_download(days, 2, false, false));
    }

    #[test]
    fn the_day_threshold_is_configurable() {
        let mut idx = cache::CacheIndex::default();
        idx.record_play("t1", "2026-03-01");
        let days = idx.record_play("t1", "2026-03-02");
        assert!(!should_auto_download(days, 3, false, false));
        let days = idx.record_play("t1", "2026-03-03");
        assert!(should_auto_download(days, 3, false, false));
        // a threshold of 1 (or a nonsense 0) downloads on first play.
        let mut idx = cache::CacheIndex::default();
        let days = idx.record_play("t2", "2026-03-01");
        assert!(should_auto_download(days, 1, false, false));
        assert!(should_auto_download(days, 0, false, false));
    }

    #[test]
    fn an_already_cached_or_in_flight_track_is_left_alone() {
        assert!(!should_auto_download(5, 2, true, false));
        assert!(!should_auto_download(5, 2, false, true));
    }

    #[test]
    fn a_config_with_the_old_play_count_field_still_loads() {
        // renaming the field must not hard-error someone's existing
        // config.toml; the old name is an alias of the new one, and
        // both defaults were 2, so the value carries straight over.
        let cfg: config::Config =
            toml::from_str("cache_listens_threshold = 4\n").expect("old field must still load");
        assert_eq!(cfg.cache_days_threshold, 4);
        let cfg: config::Config = toml::from_str("").expect("an empty config is all defaults");
        assert_eq!(cfg.cache_days_threshold, 2);
    }
}

#[cfg(test)]
mod shellsong_wiring_tests {
    use super::*;
    use wim::core::{album_page, is_starred, shellsong_env};

    /// Only the two readings `Env` takes from a front-end, so these
    /// tests do not need a `CoreCtx` (and so not a live mpv) to
    /// assert what a running program can see.
    struct EnvSurface {
        focus: shellsong::Focus,
        browsing: bool,
    }

    impl Surface for EnvSurface {
        fn front_end(&self) -> &'static str {
            "a test"
        }
        fn focus(&self) -> shellsong::Focus {
            self.focus
        }
        fn browsing(&self) -> bool {
            self.browsing
        }
    }
    use std::path::PathBuf;
    use wim::browse::fixtures::{test_client, test_songs, test_view};
    use wim::library::{LocalTrack, MediaKind};

    #[test]
    fn a_browse_session_runs_from_the_selected_row_to_the_end() {
        let mut bv = test_view(test_songs(5));
        bv.selected = 2;
        let session = browse_session(&bv);
        assert_eq!(session.len(), 3);
        assert_eq!(session[0].primary_name(), "song 2");
        assert_eq!(session[2].primary_name(), "song 4");
    }

    #[test]
    fn a_browse_session_only_takes_the_rows_the_filter_shows() {
        // a filtered artist page used to play the *unfiltered* tail;
        // the keyboard and `go()` share this function so neither can
        // regress to that on its own.
        let mut bv = test_view(test_songs(5));
        bv.filter_query = "song 3".to_string();
        bv.clamp_selection();
        let session = browse_session(&bv);
        assert_eq!(session.len(), 1);
        assert_eq!(session[0].primary_name(), "song 3");
    }

    #[test]
    fn an_empty_browse_view_yields_no_session() {
        assert!(browse_session(&test_view(Vec::new())).is_empty());
    }

    fn app_with(titles: &[&str]) -> app::App {
        let mut corpus = search::Corpus::new();
        corpus.add_local_tracks(titles.iter().map(|t| LocalTrack {
            path: PathBuf::from(format!("/music/{t}.flac")),
            title: (*t).to_string(),
            artist: None,
            album: None,
            duration_secs: None,
            kind: MediaKind::Music,
        }));
        app::App::new(corpus)
    }

    #[test]
    fn the_env_mirrors_the_results_the_queue_and_the_focus() {
        let mut app = app_with(&["flatline", "hex"]);
        app.set_query("flatline");
        let mut queue = PlayQueue::default();
        queue.reset(test_songs(3));
        let env = shellsong_env(
            &app,
            &queue,
            &EnvSurface {
                focus: shellsong::Focus::BottomBar,
                browsing: false,
            },
            true,
            Duration::from_secs(7),
        );
        assert_eq!(env.query, "flatline");
        assert_eq!(env.results, vec![shellsong::Kind::Song]);
        // the whole playlist, played prefix included: that is what the
        // queue view numbers, so it is what `goto(n)` means.
        assert_eq!(env.queue_len, 3);
        assert!(!env.browsing);
        assert_eq!(env.focus, shellsong::Focus::BottomBar);
        assert_eq!(env.now, Duration::from_secs(7));
        assert!(env.search_settled);
    }

    #[test]
    fn the_env_reports_an_open_browse_view_as_browsing() {
        let app = app_with(&["flatline"]);
        let env = shellsong_env(
            &app,
            &PlayQueue::default(),
            &EnvSurface {
                focus: shellsong::Focus::Workspace,
                browsing: true,
            },
            false,
            Duration::ZERO,
        );
        assert!(env.browsing);
        assert!(!env.search_settled);
    }

    #[test]
    fn every_focus_maps_one_to_one() {
        assert_eq!(shellsong_focus(Focus::Sidebar), shellsong::Focus::Sidebar);
        assert_eq!(
            shellsong_focus(Focus::Workspace),
            shellsong::Focus::Workspace
        );
        assert_eq!(
            shellsong_focus(Focus::BottomBar),
            shellsong::Focus::BottomBar
        );
    }

    #[test]
    fn starred_state_is_read_from_whichever_store_owns_the_item() {
        // `star()`/`unstar()` set a state, but `toggle_star` only
        // flips one, so the current state has to be readable first or
        // `unstar(x)` on something unstarred would star it.
        let mut favorites = favorites::Favorites::default();
        favorites.toggle("/music/hex.flac");
        let local = SearchItem::LocalMusic {
            path: PathBuf::from("/music/hex.flac"),
            title: "hex".into(),
            artist: None,
            album: None,
        };
        let other = SearchItem::LocalMusic {
            path: PathBuf::from("/music/crest.flac"),
            title: "crest".into(),
            artist: None,
            album: None,
        };
        let remote_starred: HashSet<String> = ["song-1".to_string()].into_iter().collect();
        assert!(is_starred(&local, &favorites, &remote_starred));
        assert!(!is_starred(&other, &favorites, &remote_starred));

        let songs = test_songs(3);
        assert!(is_starred(&songs[1], &favorites, &remote_starred));
        assert!(!is_starred(&songs[0], &favorites, &remote_starred));
    }

    #[test]
    fn an_album_is_never_starrable_so_it_never_reads_as_starred() {
        let album = SearchItem::Album {
            id: "a1".into(),
            name: "album".into(),
            artist: "artist".into(),
            client: test_client(),
        };
        assert!(!is_starred(
            &album,
            &favorites::Favorites::default(),
            &HashSet::new()
        ));
    }

    #[test]
    fn a_songs_page_is_its_album_when_the_corpus_knows_that_album() {
        let client = test_client();
        let mut corpus = search::Corpus::new();
        corpus.add_subsonic_search_result(
            wim::subsonic::SearchResult {
                artists: Vec::new(),
                albums: vec![wim::subsonic::Album {
                    id: "al-1".into(),
                    name: "album".into(),
                    artist: "artist".into(),
                }],
                songs: Vec::new(),
            },
            &client,
        );
        let page = album_page(&corpus, "album", &client);
        assert!(matches!(page, Some(SearchItem::Album { .. })));
        // a song whose album was never indexed has no page to open,
        // which is reported rather than faked: a `Song` carries its
        // album's name, not its id.
        assert!(album_page(&corpus, "some other album", &client).is_none());
    }
}

#[cfg(test)]
mod video_wiring_tests {
    use super::*;

    /// Mapped from the file extension at scan time, never from mpv, so
    /// the extension list is the real definition of "video" here.
    #[test]
    fn the_video_extensions_classify_as_video() {
        for ext in ["mp4", "mkv", "webm", "MKV"] {
            assert_eq!(
                library::classify(ext),
                Some(library::MediaKind::Video),
                "{ext}"
            );
        }
        assert_eq!(
            library::classify("flac"),
            Some(library::MediaKind::Music),
            "music must not classify as video"
        );
    }
}

#[cfg(test)]
mod startup_tests {
    use super::*;

    #[test]
    fn loading_message_names_whatever_is_still_in_flight() {
        assert_eq!(loading_message(true, true), Some("loading library..."));
        assert_eq!(
            loading_message(true, false),
            Some("scanning local library...")
        );
        assert_eq!(
            loading_message(false, true),
            Some("connecting to navidrome...")
        );
    }

    #[test]
    fn loading_message_is_none_once_both_sources_have_reported() {
        assert_eq!(
            loading_message(false, false),
            None,
            "with nothing pending, an empty corpus really is empty"
        );
    }

    #[test]
    fn loading_messages_fit_a_narrow_sidebar() {
        for (scanning, syncing) in [(true, true), (true, false), (false, true)] {
            let msg = loading_message(scanning, syncing).unwrap();
            assert!(msg.len() <= 30, "{msg:?} would be truncated in the sidebar");
        }
    }
}
