mod app;
mod browse;
mod config;
mod eq;
mod layout;
mod library;
mod lyrics;
mod mpv;
mod navmanager;
mod onboarding;
mod persistence;
mod search;
mod subsonic;
mod term;
mod theme;
mod ui;

use std::sync::Arc;
use std::time::{Duration, Instant};

use crossterm::event::{self, Event, KeyCode};
use ratatui::layout::{Constraint, Layout, Rect};

use app::Action;
use layout::{PanelLayout, ResizePanel};
use navmanager::{NavManager, NavManagerStep};
use onboarding::{Onboarding, Step};
use search::SearchItem;
use ui::banner::Banner;
use ui::browse::BrowseListView;
use ui::controlsbar::ControlsBar;
use ui::divider::{HorizontalRule, VerticalRule};
use ui::login::{LoginForm, LoginView};
use ui::lyrics_view::LyricsView;
use ui::nav_manager::NavManagerListView;
use ui::nowplaying::NowPlayingView;
use ui::onboarding::{ControlsView, DoneView, LibraryPathsView, WelcomeView};
use ui::results_list::{label, ResultsList};
use ui::searchbox::SearchBox;

/// Local items resolve straight to a file path mpv can load. Remote
/// (subsonic) items aren't wired into playback yet; the live loop only
/// has a local corpus this round; see ROADMAP.md.
///
/// A remote `Song` resolves straight to a stream url (mpv loads http
/// urls the same as local paths, nothing special needed). Remote
/// `Album`/`Playlist`/`Artist` do NOT resolve here: they represent many
/// tracks, not one, and get expanded into individual `Song` items by
/// `resolve_tracks` before anything ever calls this function on them.
fn playable_path(item: &SearchItem) -> Option<String> {
    match item {
        SearchItem::LocalMusic { path, .. } | SearchItem::LocalVideo { path, .. } => {
            Some(path.to_string_lossy().into_owned())
        }
        SearchItem::Song { id, client, .. } => Some(client.stream_url(id)),
        SearchItem::Artist { .. } | SearchItem::Album { .. } | SearchItem::Playlist { .. } => None,
    }
}

/// Expands anything that represents more than one track into its
/// actual tracks, so playing/queuing an album or playlist means
/// playing/queuing every song on it, not the (unplayable) album entity
/// itself. Local items and remote songs are already exactly one track,
/// so they pass through unchanged. Artist playback isn't wired up yet
/// (would mean fetching every album by that artist); see ROADMAP.md's
/// artist profile view.
fn resolve_tracks(item: &SearchItem) -> Result<Vec<SearchItem>, String> {
    match item {
        SearchItem::LocalMusic { .. } | SearchItem::LocalVideo { .. } | SearchItem::Song { .. } => {
            Ok(vec![item.clone()])
        }
        SearchItem::Album { id, client, .. } => client
            .get_album(id)
            .map(|details| browse::songs_to_items(details.songs, client))
            .map_err(|e| format!("couldn't load album tracks: {e}")),
        SearchItem::Playlist { id, client, .. } => client
            .get_playlist_songs(id)
            .map(|songs| browse::songs_to_items(songs, client))
            .map_err(|e| format!("couldn't load playlist tracks: {e}")),
        SearchItem::Artist { .. } => {
            Err("artist playback needs the profile view (not built yet); open an album instead".to_string())
        }
    }
}

/// Opens the track-listing view on an album or playlist; the Enter-key
/// response for those two kinds instead of `resolve_tracks`+immediate
/// play, so picking one from search shows its tracks first rather than
/// jumping straight into playback. `resolve_tracks` (full expand+queue,
/// no browsing) is still what the quick-queue key uses.
fn open_browse_view(item: &SearchItem) -> Result<browse::BrowseView, String> {
    match item {
        SearchItem::Album { id, name, artist, client } => {
            let details = client.get_album(id).map_err(|e| format!("couldn't open album: {e}"))?;
            Ok(browse::BrowseView {
                title: name.clone(),
                subtitle: artist.clone(),
                year: details.year,
                tracks: browse::songs_to_items(details.songs, client),
                selected: 0,
            })
        }
        SearchItem::Playlist { id, name, client } => {
            let songs = client.get_playlist_songs(id).map_err(|e| format!("couldn't open playlist: {e}"))?;
            Ok(browse::BrowseView {
                title: name.clone(),
                subtitle: String::new(),
                year: None,
                tracks: browse::songs_to_items(songs, client),
                selected: 0,
            })
        }
        _ => unreachable!("open_browse_view only called for Album/Playlist"),
    }
}

/// (artist, title, album) for a lyrics lookup; only meaningful for
/// local music; `SearchItem` doesn't carry duration, so lookups always
/// fall through to lrclib's fuzzy `/search` rather than its exact
/// `/get`. See ROADMAP.md.
fn lyrics_query(item: &SearchItem) -> Option<(String, String, String)> {
    match item {
        SearchItem::LocalMusic { title, artist, album, .. } => Some((
            artist.clone().unwrap_or_default(),
            title.clone(),
            album.clone().unwrap_or_default(),
        )),
        _ => None,
    }
}

/// Replaces mpv's whole playlist and plays `item` immediately. Returns
/// the status line and whether it actually started playing, so the
/// caller can update `now_playing` only on success. Callers are
/// expected to have already run `item` through `resolve_tracks`, so in
/// practice `playable_path` never actually returns `None` here; that
/// arm only exists because `playable_path` is total over `SearchItem`.
fn try_play(player: &mpv::Mpv, item: &SearchItem) -> (String, bool) {
    match playable_path(item) {
        Some(path) => match player.load_file(&path) {
            Ok(()) => (format!("playing: {}", label(item)), true),
            Err(e) => (format!("error loading file: {e}"), false),
        },
        None => ("not directly playable (expand via resolve_tracks first)".to_string(), false),
    }
}

/// Appends `item` to mpv's own playlist (plays immediately if nothing
/// was already loaded, otherwise queues gaplessly; mpv auto-advances
/// its own playlist on end-of-file with none of the round-trip gap that
/// reactively reloading on our end would introduce).
fn try_enqueue(player: &mpv::Mpv, item: &SearchItem) -> String {
    match playable_path(item) {
        Some(path) => match player.playlist_append(&path) {
            Ok(()) => format!("queued: {}", label(item)),
            Err(e) => format!("error queuing file: {e}"),
        },
        None => "not directly playable (expand via resolve_tracks first)".to_string(),
    }
}

/// Resyncs mpv's upcoming playlist to `items` (e.g. after a shuffle)
/// without disturbing whatever's currently playing; mpv's own
/// `playlist-clear` semantics already preserve the current file.
fn resync_queue(player: &mpv::Mpv, items: &[SearchItem]) -> String {
    if let Err(e) = player.playlist_clear() {
        return format!("error resyncing queue: {e}");
    }
    for item in items {
        if let Some(path) = playable_path(item) {
            if let Err(e) = player.playlist_append(&path) {
                return format!("error resyncing queue: {e}");
            }
        }
    }
    "shuffled".to_string()
}

/// Snapshots now-playing/queue/position/volume for next launch to
/// resume. Best-effort: a failed save just means nothing to resume
/// next time, not worth surfacing since we're already quitting.
fn save_playback_state(player: &mpv::Mpv, now_playing: &Option<SearchItem>, queue: &[SearchItem]) {
    let state = persistence::PlaybackState {
        now_playing_path: now_playing.as_ref().and_then(playable_path),
        queue_paths: queue.iter().filter_map(playable_path).collect(),
        position_secs: player.get_property("time-pos").ok().and_then(|v| v.as_f64()),
        volume: player.get_property("volume").ok().and_then(|v| v.as_f64()),
    };
    let _ = persistence::save(&state);
}

/// The catalog shape fetched over the wire, kept separate from `App` so
/// a background thread can fetch it without touching app state (which
/// isn't `Send` across the mpv/terminal borrow it lives behind).
struct FetchedCatalog {
    client: Arc<subsonic::Client>,
    artists: Vec<subsonic::Artist>,
    albums: Vec<subsonic::Album>,
    playlists: Vec<subsonic::Playlist>,
}

impl FetchedCatalog {
    fn summary(&self) -> String {
        format!(
            "{} artists, {} albums, {} playlists indexed",
            self.artists.len(),
            self.albums.len(),
            self.playlists.len()
        )
    }
}

/// Pings before fetching; a typo'd url/password surfaces as an error
/// rather than silently returning an empty catalog. Pure network I/O,
/// safe to call from any thread.
fn fetch_navidrome_catalog(nav: &config::NavidromeConfig) -> Result<FetchedCatalog, String> {
    let client = subsonic::Client::new(nav)
        .and_then(|c| c.ping().map(|_| c))
        .map_err(|e| format!("connection failed: {e}"))?;
    // getArtists returns everything in one call, getAlbumList2 is
    // paginated (see subsonic.rs). songs stay search-on-demand via
    // search3 rather than eagerly fetched; a library this size could be
    // enormous and isn't needed up front for search to work on
    // artists/albums.
    let artists = client.all_artists().unwrap_or_default();
    let albums = client.all_albums().unwrap_or_default();
    let playlists = client.playlists().unwrap_or_default();
    Ok(FetchedCatalog { client: Arc::new(client), artists, albums, playlists })
}

/// Merges a fetched catalog into the corpus and immediately resyncs
/// whatever's displayed; the one place this happens, so every caller
/// (startup, manual reconnect, add-profile, background auto-sync) gets
/// it for free instead of having to remember it.
fn merge_catalog_into_app(app: &mut app::App, catalog: FetchedCatalog) {
    app.corpus.add_subsonic_search_result(
        subsonic::SearchResult { artists: catalog.artists, albums: catalog.albums, songs: Vec::new() },
        &catalog.client,
    );
    app.corpus.add_playlists(catalog.playlists, &catalog.client);
    app.resync_results();
}

/// Shared by onboarding, the profile manager's add flow, and reconnect
/// on an existing profile; so none of them can drift out of sync.
/// Doesn't touch `Config` itself: callers decide whether/how to
/// persist (append a new profile vs. just refreshing an existing one).
fn index_navidrome_server(app: &mut app::App, nav: &config::NavidromeConfig) -> Result<String, String> {
    let catalog = fetch_navidrome_catalog(nav)?;
    let summary = catalog.summary();
    merge_catalog_into_app(app, catalog);
    Ok(summary)
}

/// Plain connectivity check (ping only, no catalog fetch) for every
/// configured server at once, so opening the profile manager shows
/// where each one stands immediately rather than making you select
/// each one individually to find out.
fn test_navidrome_connectivity(servers: &[config::NavidromeConfig]) -> Vec<Option<bool>> {
    servers
        .iter()
        .map(|nav| Some(subsonic::Client::new(nav).and_then(|c| c.ping()).is_ok()))
        .collect()
}

fn navidrome_config_from_form(form: &LoginForm) -> config::NavidromeConfig {
    config::NavidromeConfig {
        url: form.url.clone(),
        username: form.username.clone(),
        password: Some(form.password.clone()),
        password_file: None,
    }
}

/// What the results list should say instead of rendering nothing when
/// there's nothing to show; a blank screen with no explanation is a
/// dead end, never the right default.
fn empty_results_message(app: &app::App) -> String {
    if app.corpus.is_empty() {
        "no local music/video found: check library_paths in config, or set up navidrome (n)"
            .to_string()
    } else if app.editing_query && !app.query.is_empty() {
        format!("no matches for \"{}\"", app.query)
    } else {
        format!("{} items indexed, press / to search", app.corpus.len())
    }
}

fn rebuild_corpus(cfg: &config::Config) -> search::Corpus {
    let mut corpus = search::Corpus::new();
    corpus.add_local_tracks(library::scan(&cfg.library_paths));
    corpus
}

fn main() -> anyhow::Result<()> {
    let is_first_run = !config::config_path()?.exists();
    let mut cfg = config::load_or_init()?;
    let corpus = rebuild_corpus(&cfg);

    let player = mpv::Mpv::spawn()?;
    // per-track normalization is the sensible default; mpv reads the
    // gain from each file's own embedded tags as it plays.
    player.set_replaygain("track")?;
    let mut app = app::App::new(corpus);
    let mut status = format!(
        "{} local items indexed | / search, j/k move, gg/G jump, enter play, a queue, L lyrics, n navidrome, q quit",
        app.corpus.len()
    );

    let mut now_playing: Option<SearchItem> = None;
    let lyrics_client = lyrics::Client::new();
    let mut lyrics_lines: Option<Vec<String>> = None;
    let mut lyrics_scroll: usize = 0;
    // opened via enter on an album/playlist; takes over main_content
    // until esc, same modal-ish precedence as lyrics but independent
    // of it (browsing doesn't require anything to already be playing).
    let mut browse_view: Option<browse::BrowseView> = None;

    // connect every configured server on startup; all of them feed
    // the same unified corpus, there's no "active server" concept to
    // pick between (consistent with search spanning everything). best
    // effort: one server being down shouldn't block startup or hide
    // the others.
    if !cfg.navidrome_servers.is_empty() {
        let mut failures = Vec::new();
        for nav in &cfg.navidrome_servers {
            if let Err(msg) = index_navidrome_server(&mut app, nav) {
                failures.push(format!("{}: {msg}", nav.url));
            }
        }
        app.resync_results();
        status = if failures.is_empty() {
            format!("{} items indexed (local + navidrome)", app.corpus.len())
        } else {
            // a failed reconnect must not be silent; the old behavior
            // only updated `status` on success, so a down server at
            // startup looked identical to "nothing configured", with
            // no hint anything had gone wrong.
            format!(
                "{} items indexed; {} navidrome server(s) failed to connect: {}",
                app.corpus.len(),
                failures.len(),
                failures.join("; ")
            )
        };
    }

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

    // resuming across restarts is standard in any serious media player;
    // a missing/corrupt state file just means nothing to resume, not an
    // error; see persistence.rs.
    if let Some(saved) = persistence::load() {
        if let Some(item) = saved.now_playing_path.as_deref().and_then(persistence::item_from_path) {
            if try_play(&player, &item).1 {
                if let Some(pos) = saved.position_secs {
                    let _ = player.seek(pos, false);
                }
                now_playing = Some(item);
            }
        }
        for queued_path in &saved.queue_paths {
            if let Some(item) = persistence::item_from_path(queued_path) {
                let _ = try_enqueue(&player, &item);
                app.queue.push(item);
            }
        }
        if let Some(vol) = saved.volume {
            let _ = player.set_volume(vol);
        }
    }

    let mut panels = PanelLayout::new();
    let start = Instant::now();
    let mut guard = term::TerminalGuard::new()?;
    loop {
        guard.terminal.draw(|f| {
            let area = f.area();

            // onboarding takes the whole screen; no divider/status bar
            // competing with a guided first-run sequence.
            if let Some(ob) = &onboarding {
                match ob.step {
                    Step::Welcome => f.render_widget(WelcomeView, area),
                    Step::Controls => f.render_widget(ControlsView, area),
                    Step::Navidrome => {
                        f.render_widget(LoginView { form: &ob.login, message: &ob.login_message }, area);
                    }
                    Step::LibraryPaths => {
                        f.render_widget(
                            LibraryPathsView { paths: &ob.library_paths, input: &ob.path_input },
                            area,
                        );
                    }
                    Step::Done => f.render_widget(DoneView, area),
                }
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
                        f.render_widget(LoginView { form, message: &nm.message }, area);
                    }
                }
                return;
            }

            // sidebar (search + results, only horizontally resizable)
            // | main content (now playing / lyrics) over a bottom
            // playback bar (only vertically resizable). resize mode
            // highlights whichever divider borders the selected panel.
            let rows = Layout::vertical([Constraint::Min(0), Constraint::Length(panels.bottom_bar_height)])
                .split(area);
            let top = rows[0];
            let bottom = rows[1];

            let cols = Layout::horizontal([
                Constraint::Length(panels.sidebar_width),
                Constraint::Length(1),
                Constraint::Min(0),
            ])
            .split(top);
            let sidebar = cols[0];
            let vdivider = cols[1];
            let main_content = cols[2];

            let sidebar_selected = panels.resize_mode && panels.resize_selected == ResizePanel::Sidebar;
            let bottom_selected = panels.resize_mode && panels.resize_selected == ResizePanel::BottomBar;

            // only the banner draws its own internal horizontal rules in
            // main_content; lyrics/now-playing/browse don't, so there's
            // nothing to join against in those cases.
            let junction_rows = if browse_view.is_none() && lyrics_lines.is_none() && now_playing.is_none() {
                ui::banner::rule_rows(main_content).to_vec()
            } else {
                Vec::new()
            };
            f.render_widget(VerticalRule { highlighted: sidebar_selected, junction_rows }, vdivider);
            f.render_widget(
                HorizontalRule { highlighted: bottom_selected, junction_x: Some(vdivider.x) },
                Rect { height: 1, ..bottom },
            );

            let sidebar_rows =
                Layout::vertical([Constraint::Length(3), Constraint::Min(0)]).split(sidebar);
            // classic terminal cursor blink: ~530ms per half-phase.
            let cursor_visible = (start.elapsed().as_millis() / 530) % 2 == 0;
            f.render_widget(
                SearchBox { query: &app.query, editing: app.editing_query, cursor_visible },
                sidebar_rows[0],
            );
            f.render_widget(
                ResultsList {
                    items: &app.results,
                    selected: app.selected,
                    empty_message: &empty_results_message(&app),
                },
                sidebar_rows[1],
            );

            match &browse_view {
                Some(bv) => f.render_widget(BrowseListView { view: bv }, main_content),
                None => match &lyrics_lines {
                    Some(lines) => {
                        let borrowed: Vec<&str> = lines.iter().map(String::as_str).collect();
                        f.render_widget(LyricsView { lines: &borrowed, scroll: lyrics_scroll }, main_content);
                    }
                    None => match &now_playing {
                        Some(item) => f.render_widget(NowPlayingView { label: &label(item) }, main_content),
                        None => {
                            let elapsed_secs = start.elapsed().as_secs_f32();
                            let phase = elapsed_secs * 0.25;
                            f.render_widget(Banner { phase, elapsed_secs }, main_content);
                        }
                    },
                },
            }

            let resize_hint = panels.resize_mode.then_some(
                "resize mode: a=sidebar s=bottom bar, arrows stretch, r/esc exit",
            );
            f.render_widget(
                ControlsBar {
                    status: &status,
                    queue_len: app.queue.len(),
                    loop_on: app.loop_file,
                    shuffle_on: app.shuffle,
                    speed: app.speed,
                    eq_preset: eq::PRESETS[app.eq_preset].name,
                    resize_hint,
                },
                Rect { y: bottom.y + 1, height: bottom.height.saturating_sub(1), ..bottom },
            );
        })?;

        // non-blocking: mpv auto-advances its own playlist on
        // end-of-file (gaplessly); this just keeps our queue *view* in
        // sync with what mpv already moved past, nothing to load here.
        if let Ok(mpv::Event::EndFile) = player.events().try_recv() {
            status = match app.pop_queue() {
                Some(next) => {
                    let s = format!("now playing: {}", label(&next));
                    now_playing = Some(next);
                    s
                }
                None => {
                    now_playing = None;
                    "queue empty, idle".to_string()
                }
            };
        }

        if event::poll(Duration::from_millis(100))? {
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
                        Step::LibraryPaths => ob.path_input.push_str(text),
                        _ => {}
                    }
                } else if let Some(nm) = &mut nav_manager {
                    if let NavManagerStep::Add(form) = &mut nm.step {
                        form.current_mut().push_str(text);
                    }
                } else {
                    app.paste_into_query(text);
                }
            }

            if let Event::Key(key) = ev {
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
                                        let _ = config::save(&cfg);
                                        status = format!("navidrome connected: {msg}");
                                        ob.advance();
                                    }
                                    Err(msg) => ob.login_message = format!("{msg} (esc to skip)"),
                                }
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
                                let _ = config::save(&cfg);
                                app.corpus = rebuild_corpus(&cfg);
                                status = format!("{} local items indexed", app.corpus.len());
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
                            match key.code {
                                KeyCode::Esc => nav_manager = None,
                                KeyCode::Char('j') | KeyCode::Down => {
                                    nm.selected = (nm.selected + 1).min(entry_count - 1);
                                }
                                KeyCode::Char('k') | KeyCode::Up => {
                                    nm.selected = nm.selected.saturating_sub(1);
                                }
                                KeyCode::Char('d') if nm.selected < cfg.navidrome_servers.len() => {
                                    cfg.navidrome_servers.remove(nm.selected);
                                    if nm.selected < nm.connection_status.len() {
                                        nm.connection_status.remove(nm.selected);
                                    }
                                    let _ = config::save(&cfg);
                                    nm.selected = nm.selected.min(cfg.navidrome_servers.len());
                                    nm.message = "profile removed (already-indexed items stay until restart)".to_string();
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
                                        Err(msg) => msg,
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
                                        cfg.navidrome_servers.push(nav);
                                        let _ = config::save(&cfg);
                                        nm.connection_status.push(Some(true));
                                        nm.selected = cfg.navidrome_servers.len() - 1;
                                        nm.step = NavManagerStep::List;
                                        nm.message = format!("connected: {msg}");
                                    }
                                    Err(msg) => nm.message = format!("{msg} (esc to go back)"),
                                }
                            }
                            _ => {}
                        },
                    }
                    continue;
                }

                if panels.resize_mode {
                    // each panel only answers to its one valid resize
                    // axis: sidebar is a vertical strip (left/right
                    // only), bottom bar is a horizontal strip (up/down
                    // only); the other axis is a no-op, not a no-op
                    // that happens to also work the "wrong" way.
                    match (panels.resize_selected, key.code) {
                        (_, KeyCode::Char('r') | KeyCode::Esc) => panels.toggle_resize_mode(),
                        (_, KeyCode::Char('a')) => panels.select(ResizePanel::Sidebar),
                        (_, KeyCode::Char('s')) => panels.select(ResizePanel::BottomBar),
                        (ResizePanel::Sidebar, KeyCode::Left) => panels.shrink(),
                        (ResizePanel::Sidebar, KeyCode::Right) => panels.grow(),
                        (ResizePanel::BottomBar, KeyCode::Up) => panels.grow(),
                        (ResizePanel::BottomBar, KeyCode::Down) => panels.shrink(),
                        _ => {}
                    }
                    continue;
                }
                if key.code == KeyCode::Char('r') {
                    panels.toggle_resize_mode();
                    continue;
                }

                // browse view intercepts input the same way lyrics does
                // (below): scrolling/picking a track shouldn't move the
                // underlying results selection. enter plays from the
                // selected track to the end of the album/playlist; esc
                // closes it, handing main_content back to whatever it'd
                // otherwise show (now-playing, lyrics, or the banner).
                if let Some(bv) = &mut browse_view {
                    match key.code {
                        KeyCode::Char('j') | KeyCode::Down => bv.next(),
                        KeyCode::Char('k') | KeyCode::Up => bv.prev(),
                        KeyCode::Enter => {
                            let mut tracks = bv.tracks[bv.selected..].to_vec().into_iter();
                            if let Some(first) = tracks.next() {
                                let (s, played) = try_play(&player, &first);
                                status = s;
                                if played {
                                    let rest: Vec<SearchItem> = tracks.collect();
                                    for t in &rest {
                                        let _ = try_enqueue(&player, t);
                                    }
                                    now_playing = Some(first);
                                    app.queue = rest;
                                }
                            }
                        }
                        KeyCode::Esc | KeyCode::Char('q') => browse_view = None,
                        _ => {}
                    }
                    continue;
                }

                // lyrics view intercepts input directly rather than going
                // through App, since scrolling it shouldn't move the
                // underlying results selection.
                if lyrics_lines.is_some() {
                    match key.code {
                        KeyCode::Char('j') | KeyCode::Down => lyrics_scroll = lyrics_scroll.saturating_add(1),
                        KeyCode::Char('k') | KeyCode::Up => lyrics_scroll = lyrics_scroll.saturating_sub(1),
                        KeyCode::Char('L') | KeyCode::Esc | KeyCode::Char('q') => {
                            lyrics_lines = None;
                            lyrics_scroll = 0;
                        }
                        _ => {}
                    }
                    continue;
                }

                if key.code == KeyCode::Char('n') {
                    let mut nm = NavManager::new();
                    nm.connection_status = test_navidrome_connectivity(&cfg.navidrome_servers);
                    nav_manager = Some(nm);
                    continue;
                }

                if key.code == KeyCode::Char('L') {
                    status = match &now_playing {
                        Some(item) => match lyrics_query(item) {
                            Some((artist, title, album)) => {
                                match lyrics_client.lookup(&artist, &title, &album, None) {
                                    Ok(Some(l)) => {
                                        let text = l
                                            .plain
                                            .or(l.synced)
                                            .unwrap_or_else(|| "(instrumental, no lyrics)".to_string());
                                        lyrics_lines =
                                            Some(text.lines().map(str::to_string).collect());
                                        "lyrics loaded (j/k scroll, L/esc close)".to_string()
                                    }
                                    Ok(None) => "no lyrics found".to_string(),
                                    Err(e) => format!("lyrics lookup failed: {e}"),
                                }
                            }
                            None => "no lyrics for this item".to_string(),
                        },
                        None => "nothing playing".to_string(),
                    };
                    continue;
                }

                match app.handle_key(key.code) {
                    Action::Quit => {
                        save_playback_state(&player, &now_playing, &app.queue);
                        break;
                    }
                    // albums/playlists open the track-listing view
                    // instead of playing immediately ("let me scroll
                    // through the songs" first); songs/local files
                    // still play straight away.
                    Action::Play(item @ SearchItem::Album { .. }) | Action::Play(item @ SearchItem::Playlist { .. }) => {
                        status = match open_browse_view(&item) {
                            Ok(bv) => {
                                let msg = format!("opened: {} ({} tracks)", bv.title, bv.tracks.len());
                                browse_view = Some(bv);
                                msg
                            }
                            Err(msg) => msg,
                        };
                    }
                    Action::Play(item) => {
                        status = match resolve_tracks(&item) {
                            Ok(tracks) if tracks.is_empty() => {
                                "nothing to play (empty album or playlist)".to_string()
                            }
                            Ok(mut tracks) => {
                                let first = tracks.remove(0);
                                let (s, played) = try_play(&player, &first);
                                if played {
                                    for t in &tracks {
                                        let _ = try_enqueue(&player, t);
                                    }
                                    let rest = tracks.len();
                                    now_playing = Some(first);
                                    app.queue = tracks;
                                    if rest == 0 { s } else { format!("{s} (+{rest} more queued)") }
                                } else {
                                    s
                                }
                            }
                            Err(msg) => msg,
                        };
                    }
                    Action::Queued(item) => {
                        status = match resolve_tracks(&item) {
                            Ok(tracks) => {
                                let mut last_status = String::new();
                                for t in &tracks {
                                    last_status = try_enqueue(&player, t);
                                }
                                let n = tracks.len();
                                app.queue.extend(tracks);
                                if n <= 1 { last_status } else { format!("queued {n} tracks") }
                            }
                            Err(msg) => msg,
                        };
                    }
                    Action::QueueReordered(items) => status = resync_queue(&player, &items),
                    Action::SetLoop(on) => {
                        status = match player.set_loop_file(if on { "inf" } else { "no" }) {
                            Ok(()) => format!("loop: {}", if on { "on" } else { "off" }),
                            Err(e) => format!("error setting loop: {e}"),
                        };
                    }
                    Action::SetSpeed(speed) => {
                        status = match player.set_speed(speed) {
                            Ok(()) => format!("speed: {speed:.2}x"),
                            Err(e) => format!("error setting speed: {e}"),
                        };
                    }
                    Action::SetEq(name, gains) => {
                        let filter = eq::build_af_filter(&gains);
                        status = match player.set_af(&filter) {
                            Ok(()) => format!("eq: {name}"),
                            Err(e) => format!("error setting eq: {e}"),
                        };
                    }
                    Action::None => {}
                }
            }
        }
    }
    drop(guard);

    Ok(())
}
