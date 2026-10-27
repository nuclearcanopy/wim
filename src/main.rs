mod app;
mod browse;
mod config;
mod eq;
mod favorites;
mod layout;
mod library;
mod lyrics;
mod mpv;
mod navmanager;
mod notify;
mod onboarding;
mod opensubtitles;
mod persistence;
mod search;
mod subsonic;
mod term;
mod theme;
mod ui;

use std::collections::{HashMap, HashSet};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use crossterm::event::{self, Event, KeyCode};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui_image::picker::Picker;
use ratatui_image::protocol::StatefulProtocol;
use ratatui_image::StatefulImage;

use app::{Action, LoopMode};
use layout::{PanelLayout, ResizePanel};
use navmanager::{NavManager, NavManagerStep};
use notify::{Notifications, Severity};
use onboarding::{Onboarding, Step};
use search::SearchItem;
use theme::{shade, NEAR_BLACK};
use ui::banner::Banner;
use ui::browse::BrowseListView;
use ui::controlsbar::{ControlsBar, Field};
use ui::divider::{HorizontalRule, VerticalRule};
use ui::login::{LoginForm, LoginView};
use ui::lyrics_view::LyricsView;
use ui::miniplayer::MiniPlayerBar;
use ui::nav_manager::NavManagerListView;
use ui::notify::NotificationOverlay;
use ui::onboarding::{ControlsView, DoneView, LibraryPathsView, WelcomeView};
use ui::player::PlayerView;
use ui::results_list::{label, ResultsList};
use ui::searchbox::SearchBox;
use ui::visualizer::LevelMeter;

/// Local items resolve straight to a file path mpv can load. A remote
/// `Song` resolves straight to a stream url (mpv loads http urls the
/// same as local paths, nothing special needed). Remote `Album`/
/// `Playlist`/`Artist` do NOT resolve here: they represent many
/// tracks, not one, and get expanded into individual `Song` items by
/// `resolve_tracks`/`artist_tracks` before anything ever calls this
/// function on them.
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
/// actual tracks, so playing/queuing an album, playlist, or artist
/// means playing/queuing every (actual, see `artist_tracks`) song,
/// not the entity itself. Local items and remote songs are already
/// exactly one track, so they pass through unchanged.
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
        SearchItem::Artist { id, name, client } => artist_tracks(client, id, name),
    }
}

/// Every song actually credited to this artist, across every album
/// including compilations. The Global/Various-Artists fix: `getAlbum`
/// returns each song with its own track-level `artist` field, distinct
/// from the album's credited artist (a compilation like navidrome's
/// "Global"/"Various Artists" convention would otherwise hide every
/// song on it from here); filtering by the song's own field instead of
/// the album's works for any compilation, not a hardcoded special case.
fn artist_tracks(
    client: &Arc<subsonic::Client>,
    artist_id: &str,
    artist_name: &str,
) -> Result<Vec<SearchItem>, String> {
    let mut albums = client
        .get_artist(artist_id)
        .map_err(|e| format!("couldn't load artist: {e}"))?;
    // chronological, not whatever order getArtist happened to return
    // (alphabetical); unknown-year albums sort last rather than first.
    albums.sort_by_key(|a| a.year.unwrap_or(u32::MAX));
    let mut tracks = Vec::new();
    for album in albums {
        if let Ok(details) = client.get_album(&album.id) {
            tracks.extend(
                details
                    .songs
                    .into_iter()
                    .filter(|s| s.artist == artist_name)
                    .map(|s| SearchItem::Song {
                        id: s.id,
                        title: s.title,
                        artist: s.artist,
                        album: s.album,
                        client: client.clone(),
                    }),
            );
        }
    }
    Ok(tracks)
}

/// Opens the track-listing view on an album or playlist; the Enter-key
/// response for those two kinds instead of `resolve_tracks`+immediate
/// play, so picking one from search shows its tracks first rather than
/// jumping straight into playback. `resolve_tracks` (full expand+queue,
/// no browsing) is still what the quick-queue key uses.
fn open_browse_view(item: &SearchItem) -> Result<browse::BrowseView, String> {
    match item {
        SearchItem::Album {
            id,
            name,
            artist,
            client,
        } => {
            let details = client
                .get_album(id)
                .map_err(|e| format!("couldn't open album: {e}"))?;
            Ok(browse::BrowseView {
                title: name.clone(),
                subtitle: artist.clone(),
                year: details.year,
                tracks: browse::songs_to_items(details.songs, client),
                selected: 0,
                art: fetch_cover_art(client, id),
                bio: None,
            })
        }
        SearchItem::Playlist { id, name, client } => {
            let songs = client
                .get_playlist_songs(id)
                .map_err(|e| format!("couldn't open playlist: {e}"))?;
            Ok(browse::BrowseView {
                title: name.clone(),
                subtitle: String::new(),
                year: None,
                tracks: browse::songs_to_items(songs, client),
                selected: 0,
                art: None,
                bio: None,
            })
        }
        _ => unreachable!("open_browse_view only called for Album/Playlist"),
    }
}

/// The artist profile view: a bio (best-effort, `None` if navidrome
/// has nothing for this artist) and every song actually credited to
/// them across every album, chronologically. See `artist_tracks` for
/// the Global/Various-Artists filtering this relies on.
fn open_artist_view(item: &SearchItem) -> Result<browse::BrowseView, String> {
    let SearchItem::Artist { id, name, client } = item else {
        unreachable!("open_artist_view only called for Artist");
    };
    let bio = client.get_artist_bio(id).ok().flatten();
    let tracks = artist_tracks(client, id, name)?;
    Ok(browse::BrowseView {
        title: name.clone(),
        subtitle: format!("{} songs", tracks.len()),
        year: None,
        tracks,
        selected: 0,
        art: None,
        bio,
    })
}

/// Local files flip the persisted favorites set; remote songs call
/// navidrome's own star/unstar so it stays in sync with any other
/// subsonic client hitting the same server. Returns the new state.
fn toggle_star(
    item: &SearchItem,
    favorites: &mut favorites::Favorites,
    remote_starred: &mut HashSet<String>,
) -> Result<bool, String> {
    match item {
        SearchItem::LocalMusic { path, .. } | SearchItem::LocalVideo { path, .. } => {
            let now_starred = favorites.toggle(&path.to_string_lossy());
            let _ = favorites::save(favorites);
            Ok(now_starred)
        }
        SearchItem::Song { id, client, .. } => {
            if remote_starred.contains(id) {
                client
                    .unstar(id)
                    .map_err(|e| format!("couldn't unstar: {e}"))?;
                remote_starred.remove(id);
                Ok(false)
            } else {
                client.star(id).map_err(|e| format!("couldn't star: {e}"))?;
                remote_starred.insert(id.clone());
                Ok(true)
            }
        }
        SearchItem::Artist { .. } | SearchItem::Album { .. } | SearchItem::Playlist { .. } => {
            Err("only tracks can be starred".to_string())
        }
    }
}

/// Starred songs browsed the same way an album is: merges every
/// configured server's own starred list with local favorites into one
/// `BrowseView`. Fetched fresh on open rather than cached, same as
/// opening an album, so it can't go stale.
fn open_starred_view(
    servers: &[config::NavidromeConfig],
    favorites: &favorites::Favorites,
) -> browse::BrowseView {
    let mut tracks = Vec::new();
    for nav in servers {
        if let Ok(client) = subsonic::Client::new(nav) {
            if let Ok(songs) = client.get_starred_songs() {
                let client = Arc::new(client);
                tracks.extend(browse::songs_to_items(songs, &client));
            }
        }
    }
    for path in favorites.paths() {
        if let Some(item) = persistence::item_from_path(path) {
            tracks.push(item);
        }
    }
    browse::BrowseView {
        title: "starred songs".to_string(),
        subtitle: String::new(),
        year: None,
        tracks,
        selected: 0,
        art: None,
        bio: None,
    }
}

/// Best-effort: a missing/unreadable cover is not worth surfacing as
/// an error, the album view just shows no art for it.
fn fetch_cover_art(client: &subsonic::Client, album_id: &str) -> Option<image::DynamicImage> {
    let bytes = client.get_cover_art_bytes(album_id).ok()?;
    image::load_from_memory(&bytes).ok()
}

/// `Auto`/`Blocks` both start in ascii (the strictly-monochrome
/// default); `Sixel`/`Kitty`/`Iterm2` start in real-image mode since
/// configuring one specifically is an explicit ask for it. `i` still
/// cycles through all three regardless of where it started.
fn initial_art_mode(mode: config::AlbumArtMode) -> browse::ArtMode {
    match mode {
        config::AlbumArtMode::Auto | config::AlbumArtMode::Blocks => browse::ArtMode::Ascii,
        config::AlbumArtMode::Sixel
        | config::AlbumArtMode::Kitty
        | config::AlbumArtMode::Iterm2 => browse::ArtMode::Real,
        config::AlbumArtMode::Off => browse::ArtMode::Off,
    }
}

/// Best-effort, fire-and-forget from the caller's point of view: a
/// no-op whenever `item` isn't a local video, no api key is
/// configured, or the search/download fails for any reason (no
/// matches, network error, quota exhausted). Blocking on the render
/// thread, same documented tradeoff as the lyrics lookup; a brief
/// hitch right when a video starts, not a background thread.
fn maybe_fetch_subtitles(
    cfg: &config::Config,
    player: &mpv::Mpv,
    notifications: &mut Notifications,
    item: &SearchItem,
) {
    let SearchItem::LocalVideo { path, title } = item else {
        return;
    };
    let Some(api_key) = cfg.resolve_opensubtitles_api_key() else {
        return;
    };
    let client = opensubtitles::Client::new(api_key);
    let Ok(results) = client.search(title, &cfg.subtitle_language) else {
        return;
    };
    let Some(best) = results.first() else {
        return;
    };
    let Ok(bytes) = client.download(best.file_id) else {
        return;
    };
    let sub_path = path.with_extension("wim-auto.srt");
    if std::fs::write(&sub_path, bytes).is_ok()
        && player.sub_add(&sub_path.to_string_lossy()).is_ok()
    {
        notifications.push(
            Severity::Info,
            format!("subtitle: {} ({})", best.release, best.language),
        );
    }
}

/// (artist, title, album) for a lyrics lookup; only meaningful for
/// local music; `SearchItem` doesn't carry duration, so lookups always
/// fall through to lrclib's fuzzy `/search` rather than its exact
/// `/get`. See ROADMAP.md.
fn lyrics_query(item: &SearchItem) -> Option<(String, String, String)> {
    match item {
        SearchItem::LocalMusic {
            title,
            artist,
            album,
            ..
        } => Some((
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
        None => (
            "not directly playable (expand via resolve_tracks first)".to_string(),
            false,
        ),
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
        position_secs: player
            .get_property("time-pos")
            .ok()
            .and_then(|v| v.as_f64()),
        volume: player.get_property("volume").ok().and_then(|v| v.as_f64()),
    };
    let _ = persistence::save(&state);
}

/// "Back" re-plays whatever was playing before the current track,
/// putting the interrupted track at the front of the queue so it
/// picks up right where it would have anyway. Deliberately our own
/// bookkeeping rather than mpv's `playlist-prev`: that command would
/// move mpv's playlist-position pointer independently of our own
/// `app.queue`, desyncing the two; replaying `previous_track` directly
/// keeps everything flowing through the one path we already control.
/// Only ever tracks one step back, not a full history.
fn go_to_previous(
    cfg: &config::Config,
    player: &mpv::Mpv,
    notifications: &mut Notifications,
    now_playing: &mut Option<SearchItem>,
    previous_track: &mut Option<SearchItem>,
    queue: &mut Vec<SearchItem>,
) -> String {
    let Some(prev) = previous_track.take() else {
        return "no previous track".to_string();
    };
    let (status, played) = try_play(player, &prev);
    if played {
        if let Some(interrupted) = now_playing.take() {
            queue.insert(0, interrupted);
        }
        maybe_fetch_subtitles(cfg, player, notifications, &prev);
        *now_playing = Some(prev);
    } else {
        *previous_track = Some(prev);
    }
    status
}

/// `(elapsed, duration, paused)` for whatever's currently loaded;
/// queried fresh each frame rather than cached, so the mini/full
/// player's progress bar is always showing mpv's actual position.
fn playback_info(player: &mpv::Mpv) -> (Option<f64>, Option<f64>, bool) {
    let elapsed = player
        .get_property("time-pos")
        .ok()
        .and_then(|v| v.as_f64());
    let duration = player
        .get_property("duration")
        .ok()
        .and_then(|v| v.as_f64());
    let paused = player
        .get_property("pause")
        .ok()
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    (elapsed, duration, paused)
}

/// Current output level, 0..1, for the now-playing level meter.
/// Best-effort: reads the `astats` tap `build_af_filter` (eq.rs) adds
/// to the end of the af chain, via mpv's `af-metadata/<label>/...`
/// property. If that property path doesn't resolve the way expected
/// (wrong mpv version, filter not actually in the graph yet, af-
/// metadata represented differently than guessed), this falls back to
/// a calm idle sine pulse instead of silently showing a dead meter;
/// `phase` drives that fallback and is advanced by the caller.
fn audio_level(player: &mpv::Mpv, phase: f32) -> f32 {
    let key = format!(
        "af-metadata/{}/lavfi.astats.Overall.RMS_level",
        eq::LEVEL_METADATA_LABEL
    );
    if let Some(db) = player.get_property(&key).ok().and_then(|v| v.as_f64()) {
        // RMS level is dBFS, roughly -60 (near silent) to 0 (full
        // scale); map that range to 0..1 rather than guessing a
        // narrower one that'd clip real audio's dynamic range.
        return ((db as f32 + 60.0) / 60.0).clamp(0.0, 1.0);
    }
    (phase.sin() * 0.3 + 0.35).clamp(0.0, 1.0)
}

/// How often the background thread (see `spawn_background_sync`)
/// re-fetches every configured server, so navidrome stays "constantly
/// updating" without the user ever pressing a manual refresh key.
const AUTO_SYNC_INTERVAL: Duration = Duration::from_secs(45);

/// A completed background sync attempt for one server, tagged by url
/// since several servers' results arrive interleaved on the same
/// channel with no other way to tell them apart.
struct BackgroundSyncResult {
    url: String,
    result: Result<FetchedCatalog, String>,
}

/// Runs forever on its own thread, re-fetching every server's catalog
/// on a timer and reporting each attempt back over `result_tx`; the
/// main thread owns `App`/mpv/the terminal, none of which are `Send`
/// across this boundary, so this only ever does pure network I/O and
/// hands the result back rather than touching app state directly.
/// `servers_rx` lets the main thread push an updated server list (add/
/// remove a profile) without restarting the thread.
fn spawn_background_sync(
    initial_servers: Vec<config::NavidromeConfig>,
    result_tx: mpsc::Sender<BackgroundSyncResult>,
    servers_rx: mpsc::Receiver<Vec<config::NavidromeConfig>>,
) {
    thread::spawn(move || {
        let mut servers = initial_servers;
        loop {
            match servers_rx.recv_timeout(AUTO_SYNC_INTERVAL) {
                Ok(updated) => servers = updated,
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => return,
            }
            for nav in &servers {
                let result = fetch_navidrome_catalog(nav);
                if result_tx
                    .send(BackgroundSyncResult {
                        url: nav.url.clone(),
                        result,
                    })
                    .is_err()
                {
                    return; // main thread is gone (shutting down)
                }
            }
        }
    });
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
    // paginated (see subsonic.rs). songs stay fetched on demand (per
    // album/playlist/artist opened) rather than indexed eagerly; a
    // library this size could be enormous and isn't needed up front
    // for search to work on artists/albums.
    let artists = client.all_artists().unwrap_or_default();
    let albums = client.all_albums().unwrap_or_default();
    let playlists = client.playlists().unwrap_or_default();
    Ok(FetchedCatalog {
        client: Arc::new(client),
        artists,
        albums,
        playlists,
    })
}

/// Merges a fetched catalog into the corpus and immediately resyncs
/// whatever's displayed; the one place this happens, so every caller
/// (startup, manual reconnect, add-profile, background auto-sync) gets
/// it for free instead of having to remember it.
fn merge_catalog_into_app(app: &mut app::App, catalog: FetchedCatalog) {
    app.corpus.add_subsonic_search_result(
        subsonic::SearchResult {
            artists: catalog.artists,
            albums: catalog.albums,
            songs: Vec::new(),
        },
        &catalog.client,
    );
    app.corpus.add_playlists(catalog.playlists, &catalog.client);
    app.resync_results();
}

/// Shared by onboarding, the profile manager's add flow, and reconnect
/// on an existing profile; so none of them can drift out of sync.
/// Doesn't touch `Config` itself: callers decide whether/how to
/// persist (append a new profile vs. just refreshing an existing one).
fn index_navidrome_server(
    app: &mut app::App,
    nav: &config::NavidromeConfig,
) -> Result<String, String> {
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
        max_bitrate_kbps: None,
        transcode_format: None,
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
        "press / to search".to_string()
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
    // harmless to set before any video/subtitle is even loaded; mpv
    // just holds onto it for whenever one shows up.
    player.set_subtitle_style(&cfg.subtitles);
    let mut subtitle_delay = cfg.subtitles.delay_secs;
    let mut app = app::App::new(corpus);
    // the full keybind list moved to the '?' help overlay; nothing
    // worth saying here until something actually happens.
    let mut status = String::new();

    let mut favorites = favorites::load();
    // song ids this session knows are starred, across every configured
    // server; populated at startup and kept current on every manual
    // toggle (not re-fetched constantly, starring is low-frequency).
    let mut remote_starred: HashSet<String> = HashSet::new();

    let mut now_playing: Option<SearchItem> = None;
    // whatever was playing right before `now_playing` last changed;
    // purely for the player view's "previous: ..." line.
    let mut previous_track: Option<SearchItem> = None;
    // false (full player) by default for a standalone play; starting
    // playback from an open album/playlist sets this true instead, so
    // the track listing stays visible rather than getting replaced.
    // 'm' toggles it by hand either way.
    let mut player_minimized = false;
    // 'i' cycles ascii -> real -> off from here; this is just the
    // config-driven starting point.
    let mut art_mode = initial_art_mode(cfg.album_art);
    // full keybind reference, opened with '?'; takes over the whole
    // screen the same way onboarding/the nav manager do.
    let mut help_open = false;
    // rolling window for the now-playing level meter; a fixed-size
    // ring (just truncate the front) rather than growing forever.
    let mut level_history: Vec<f32> = Vec::new();
    let mut notifications = Notifications::default();
    let lyrics_client = lyrics::Client::new();
    let mut lyrics_lines: Option<Vec<String>> = None;
    let mut lyrics_scroll: usize = 0;
    // opened via enter on an album/playlist; takes over main_content
    // until esc, same modal-ish precedence as lyrics but independent
    // of it (browsing doesn't require anything to already be playing).
    let mut browse_view: Option<browse::BrowseView> = None;
    // the full track list a play session started with (first track +
    // everything that got queued alongside it), kept around purely so
    // "loop all" has something to restart from once `app.queue` drains;
    // `app.queue` itself is a live, shrinking FIFO, not a fixed list.
    let mut original_queue: Vec<SearchItem> = Vec::new();

    // connect every configured server on startup; all of them feed
    // the same unified corpus, there's no "active server" concept to
    // pick between (consistent with search spanning everything). best
    // effort: one server being down shouldn't block startup or hide
    // the others.
    // tracks the last known reachability per server url, so the
    // background sync below only notifies on an actual transition
    // (going offline, coming back), not every single retry while a
    // server stays down.
    let mut server_reachable: HashMap<String, bool> = HashMap::new();
    if !cfg.navidrome_servers.is_empty() {
        for nav in &cfg.navidrome_servers {
            let ok = match index_navidrome_server(&mut app, nav) {
                Ok(_) => true,
                Err(msg) => {
                    // a failed reconnect must not be silent; push it
                    // as its own notification rather than folding it
                    // into a summary status line nobody asked for.
                    notifications.push(Severity::Error, format!("{}: {msg}", nav.url));
                    false
                }
            };
            server_reachable.insert(nav.url.clone(), ok);
        }
        app.resync_results();
        // 'I' shows a full local/per-server breakdown on demand
        // instead of announcing a count unprompted on every startup.
    }

    // background re-sync so navidrome stays live without a manual
    // refresh key ("constantly updating" per the spec).
    let (sync_tx, sync_rx) = mpsc::channel::<BackgroundSyncResult>();
    let (servers_tx, servers_rx) = mpsc::channel::<Vec<config::NavidromeConfig>>();
    spawn_background_sync(cfg.navidrome_servers.clone(), sync_tx, servers_rx);

    for nav in &cfg.navidrome_servers {
        if let Ok(client) = subsonic::Client::new(nav) {
            if let Ok(songs) = client.get_starred_songs() {
                remote_starred.extend(songs.into_iter().map(|s| s.id));
            }
        }
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
        if let Some(item) = saved
            .now_playing_path
            .as_deref()
            .and_then(persistence::item_from_path)
        {
            if try_play(&player, &item).1 {
                if let Some(pos) = saved.position_secs {
                    let _ = player.seek(pos, false);
                }
                maybe_fetch_subtitles(&cfg, &player, &mut notifications, &item);
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

    // the ceiling crossfade's fade-out/fade-in ramps scale against;
    // read back rather than assuming 100, since persistence may have
    // just restored something else above.
    let base_volume = player
        .get_property("volume")
        .ok()
        .and_then(|v| v.as_f64())
        .unwrap_or(100.0);
    let mut crossfade_enabled = cfg.crossfade_enabled;
    // Some(when the current track started) while fading in; mpv has
    // no notion of this, so it's tracked entirely on our side and
    // cleared once the fade finishes.
    let mut fade_in_started_at: Option<Instant> = None;
    // Which ControlsBar state-line field most recently changed, and
    // when; render computes a decaying brightness from the elapsed
    // time rather than a one-off status sentence, so a toggle just
    // lights up the value it already shows permanently.
    let mut flash: Option<(Field, Instant)> = None;
    const FLASH_DURATION: Duration = Duration::from_millis(700);

    let mut panels = PanelLayout::new();
    let start = Instant::now();
    let mut guard = term::TerminalGuard::new()?;
    // queries the terminal over stdio for font-cell size and graphics
    // protocol support; best-effort, 'i' (real image mode) just stays
    // unavailable if this fails rather than erroring the whole app.
    let mut picker = Picker::from_query_stdio().ok();
    // rebuilt (not re-encoded every frame) whenever a newly-opened
    // album's art changes; `ratatui-image` owns the actual resize/
    // encode caching per protocol instance.
    let mut real_art_protocol: Option<StatefulProtocol> = None;
    loop {
        notifications.expire();
        guard.terminal.draw(|f| {
            let area = f.area();

            // onboarding takes the whole screen; no divider/status bar
            // competing with a guided first-run sequence.
            if let Some(ob) = &onboarding {
                match ob.step {
                    Step::Welcome => f.render_widget(WelcomeView, area),
                    Step::Controls => f.render_widget(ControlsView, area),
                    Step::Navidrome => {
                        f.render_widget(
                            LoginView {
                                form: &ob.login,
                                message: &ob.login_message,
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
                f.render_widget(ui::help::HelpOverlay, area);
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

            let sidebar_selected =
                panels.resize_mode && panels.resize_selected == ResizePanel::Sidebar;
            let bottom_selected =
                panels.resize_mode && panels.resize_selected == ResizePanel::BottomBar;

            let sidebar_rows =
                Layout::vertical([Constraint::Length(3), Constraint::Min(0)]).split(sidebar);
            // the search box is open on its own right edge (see
            // ui/searchbox.rs) and relies on this divider for its
            // closing line instead: ┬ where its top border meets the
            // divider (nothing above that row, the divider only
            // continues down from there), ┤ where its bottom border
            // does (the divider continues both up and down through
            // that row). the browse view's own header draws a second,
            // separate internal horizontal rule right at
            // main_content's left edge; the banner (now a
            // LazyVim-style logo+menu, no framing rules) and the
            // player/lyrics don't, so that one only applies sometimes.
            let mut junction_rows = vec![
                (sidebar_rows[0].y, '┬', shade(NEAR_BLACK)),
                (
                    sidebar_rows[0].y + sidebar_rows[0].height - 1,
                    '┤',
                    shade(NEAR_BLACK),
                ),
            ];
            if browse_view.is_some() {
                // draw_header's rule sits one row below where it's
                // asked to start, which is always main_content.y here;
                // static color matching draw_header's own rule_color.
                junction_rows.push((main_content.y + 1, '├', shade(NEAR_BLACK)));
            }
            // the divider used to stop at the bottom of `top`, one row
            // short of the mini player strip below it (a separate row
            // in the outer vertical split, sidebar-width columns
            // included); the real bottom bar's rule still drew a ┴
            // junction expecting a line to feed into it from above,
            // so with the mini bar showing, that line had already
            // stopped 3 rows earlier. Stretch the divider down through
            // the mini bar's own height too in that case, with a ┼
            // where its own internal rule (its first row) crosses.
            let vdivider_rect = if show_mini_bar {
                junction_rows.push((mini_bar_row.y, '┼', shade(NEAR_BLACK)));
                Rect {
                    height: vdivider.height + mini_bar_row.height,
                    ..vdivider
                }
            } else {
                vdivider
            };
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
            const HELP_BOX_WIDTH: u16 = 10;
            let help_box_x = bottom.x + bottom.width.saturating_sub(HELP_BOX_WIDTH);
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

            // classic terminal cursor blink: ~530ms per half-phase.
            let cursor_visible = (start.elapsed().as_millis() / 530).is_multiple_of(2);
            f.render_widget(
                SearchBox {
                    query: &app.query,
                    editing: app.editing_query,
                    cursor_visible,
                },
                sidebar_rows[0],
            );
            f.render_widget(
                ResultsList {
                    items: &app.results,
                    selected: app.selected,
                    empty_message: &empty_results_message(&app),
                    favorites: &favorites,
                    remote_starred: &remote_starred,
                },
                sidebar_rows[1],
            );

            if show_full_player {
                let item = now_playing.as_ref().expect("show_full_player implies Some");
                let (elapsed_secs, duration_secs, paused) = playback_info(&player);
                let previous_label = previous_track.as_ref().map(label);
                let next_label = app.queue.first().map(label);
                f.render_widget(
                    PlayerView {
                        title: &label(item),
                        elapsed_secs,
                        duration_secs,
                        paused,
                        loop_label: app.loop_mode.label(),
                        shuffle_on: app.shuffle,
                        speed: app.speed,
                        eq_preset: eq::PRESETS[app.eq_preset].name,
                        crossfade_on: crossfade_enabled,
                        previous: previous_label.as_deref(),
                        next: next_label.as_deref(),
                    },
                    main_content,
                );

                let phase = start.elapsed().as_secs_f32() * 2.0;
                level_history.push(audio_level(&player, phase));
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
                f.render_widget(
                    BrowseListView {
                        view: bv,
                        favorites: &favorites,
                        remote_starred: &remote_starred,
                        art_mode,
                    },
                    main_content,
                );
                if art_mode == browse::ArtMode::Real {
                    if let Some(protocol) = &mut real_art_protocol {
                        let (art_rect, _) =
                            browse::layout(main_content, bv.art.is_some(), bv.bio.is_some());
                        if let Some(rect) = art_rect {
                            f.render_stateful_widget(StatefulImage::default(), rect, protocol);
                        }
                    }
                }
            } else if let Some(lines) = &lyrics_lines {
                let borrowed: Vec<&str> = lines.iter().map(String::as_str).collect();
                f.render_widget(
                    LyricsView {
                        lines: &borrowed,
                        scroll: lyrics_scroll,
                    },
                    main_content,
                );
            } else {
                let elapsed_secs = start.elapsed().as_secs_f32();
                let phase = elapsed_secs * 0.25;
                f.render_widget(
                    Banner {
                        phase,
                        elapsed_secs,
                    },
                    main_content,
                );
            }

            if show_mini_bar {
                let item = now_playing.as_ref().expect("show_mini_bar implies Some");
                let (elapsed_secs, duration_secs, paused) = playback_info(&player);
                f.render_widget(
                    MiniPlayerBar {
                        title: &label(item),
                        elapsed_secs,
                        duration_secs,
                        paused,
                        loop_label: app.loop_mode.label(),
                        shuffle_on: app.shuffle,
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
            let flash_level = flash.map(|(field, at)| {
                let t = at.elapsed().as_secs_f32() / FLASH_DURATION.as_secs_f32();
                (field, (1.0 - t).clamp(0.0, 1.0))
            });
            f.render_widget(
                ControlsBar {
                    status: &status,
                    queue_len: app.queue.len(),
                    loop_label: app.loop_mode.label(),
                    shuffle_on: app.shuffle,
                    speed: app.speed,
                    eq_preset: eq::PRESETS[app.eq_preset].name,
                    crossfade_on: crossfade_enabled,
                    flash: flash_level,
                    resize_hint,
                },
                Rect {
                    y: bottom.y + 1,
                    width: bottom.width.saturating_sub(HELP_BOX_WIDTH),
                    height: bottom.height.saturating_sub(1),
                    ..bottom
                },
            );
            f.render_widget(
                ui::helpbox::HelpBox { label: "? help" },
                Rect {
                    x: help_box_x,
                    y: bottom.y + 1,
                    width: HELP_BOX_WIDTH,
                    height: bottom.height.saturating_sub(1),
                },
            );
            f.render_widget(
                NotificationOverlay {
                    items: notifications.visible(),
                },
                area,
            );
        })?;

        // non-blocking: mpv auto-advances its own playlist on
        // end-of-file (gaplessly); this just keeps our queue *view* in
        // sync with what mpv already moved past, nothing to load here.
        if let Ok(mpv::Event::EndFile) = player.events().try_recv() {
            status = match app.pop_queue() {
                Some(next) => {
                    let s = format!("now playing: {}", label(&next));
                    maybe_fetch_subtitles(&cfg, &player, &mut notifications, &next);
                    previous_track = now_playing.replace(next);
                    s
                }
                // mpv has no concept of "the queue we built", only its
                // own (now-empty) playlist; "loop all" replaying from
                // the top is purely our own bookkeeping via
                // `original_queue`, reloaded the same first-play-then-
                // enqueue-the-rest way a fresh play does.
                None if app.loop_mode == LoopMode::All && !original_queue.is_empty() => {
                    let mut tracks = original_queue.clone().into_iter();
                    let first = tracks.next().expect("checked non-empty above");
                    let (s, played) = try_play(&player, &first);
                    if played {
                        let rest: Vec<SearchItem> = tracks.collect();
                        for t in &rest {
                            let _ = try_enqueue(&player, t);
                        }
                        maybe_fetch_subtitles(&cfg, &player, &mut notifications, &first);
                        previous_track = now_playing.replace(first);
                        app.queue = rest;
                    }
                    s
                }
                None => {
                    now_playing = None;
                    "queue empty, idle".to_string()
                }
            };
            // a track just ended; either start the new one quiet and
            // ramp it up (crossfade on), or make sure it's at full
            // volume regardless of wherever a fade-out left things
            // (crossfade just got disabled mid-fade, say).
            if now_playing.is_some() {
                if crossfade_enabled {
                    let _ = player.set_volume(0.0);
                    fade_in_started_at = Some(Instant::now());
                } else {
                    let _ = player.set_volume(base_volume);
                }
            }
        }

        // the fade-out approaching a track's end and the fade-in right
        // after a new one starts are mutually exclusive in time (one
        // happens near end-of-file, the other right after), so this
        // is safe to check unconditionally every frame rather than
        // only while a particular view is showing; volume should fade
        // whether or not the player view is on screen.
        if crossfade_enabled && now_playing.is_some() {
            if let Some(started) = fade_in_started_at {
                let t = (started.elapsed().as_secs_f64() / cfg.crossfade_seconds).clamp(0.0, 1.0);
                let _ = player.set_volume(base_volume * t);
                if t >= 1.0 {
                    fade_in_started_at = None;
                }
            } else {
                let (elapsed, duration, _) = playback_info(&player);
                if let (Some(elapsed), Some(duration)) = (elapsed, duration) {
                    let remaining = duration - elapsed;
                    if remaining <= cfg.crossfade_seconds {
                        let t = (remaining / cfg.crossfade_seconds).clamp(0.0, 1.0);
                        let _ = player.set_volume(base_volume * t);
                    }
                }
            }
        }

        // drain whatever the background sync thread finished since the
        // last frame; only notify on an actual offline/online
        // transition, not every retry while a server stays down.
        while let Ok(sr) = sync_rx.try_recv() {
            match sr.result {
                Ok(catalog) => {
                    merge_catalog_into_app(&mut app, catalog);
                    if server_reachable.insert(sr.url.clone(), true) == Some(false) {
                        notifications.push(Severity::Info, format!("{} is back online", sr.url));
                    }
                }
                Err(msg) => {
                    if server_reachable.insert(sr.url.clone(), false) != Some(false) {
                        notifications
                            .push(Severity::Alert, format!("{} went offline: {msg}", sr.url));
                    }
                }
            }
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
                } else if !help_open {
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
                                        let _ = servers_tx.send(cfg.navidrome_servers.clone());
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
                                        let _ = config::save(&cfg);
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
                    if matches!(key.code, KeyCode::Char('?') | KeyCode::Esc) {
                        help_open = false;
                    }
                    continue;
                }
                if key.code == KeyCode::Char('?') {
                    help_open = true;
                    continue;
                }

                // every other block below intercepts specific keys and
                // unconditionally `continue`s even on a non-match
                // (browse_view/lyrics especially), so a query typed
                // while either was open got half-swallowed, half
                // misrouted into playback shortcuts instead of
                // reaching the text box. One early, unconditional gate
                // here: editing a query claims every key, full stop.
                if app.editing_query {
                    app.handle_key(key.code);
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
                        // wasd selects between panels (w/a -> sidebar,
                        // s/d -> bottom bar); only two panels exist
                        // today so this is a 2-way split of a 4-way
                        // scheme, ready for whenever a third shows up.
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
                            let session: Vec<SearchItem> = bv.tracks[bv.selected..].to_vec();
                            let mut tracks = session.clone().into_iter();
                            if let Some(first) = tracks.next() {
                                let (s, played) = try_play(&player, &first);
                                if played {
                                    let rest: Vec<SearchItem> = tracks.collect();
                                    for t in &rest {
                                        let _ = try_enqueue(&player, t);
                                    }
                                    maybe_fetch_subtitles(
                                        &cfg,
                                        &player,
                                        &mut notifications,
                                        &first,
                                    );
                                    previous_track = now_playing.replace(first);
                                    app.queue = rest;
                                    original_queue = session;
                                    // keep the album/playlist listing
                                    // visible rather than replacing it
                                    // with the full player.
                                    player_minimized = true;
                                } else {
                                    notifications.push(Severity::Error, s.clone());
                                }
                                status = s;
                            }
                        }
                        // maximizing replaces this album/playlist view
                        // with the full player; minimizing again
                        // brings it right back (not cleared, just not
                        // on top while the player's maximized).
                        KeyCode::Char('m') if now_playing.is_some() => {
                            player_minimized = !player_minimized
                        }
                        KeyCode::Char('i') => art_mode = art_mode.toggle(),
                        KeyCode::Char(' ') if now_playing.is_some() => {
                            let _ = player.toggle_pause();
                        }
                        KeyCode::Char('f') if now_playing.is_some() => {
                            let _ = player.playlist_next();
                        }
                        KeyCode::Char('b') if previous_track.is_some() => {
                            status = go_to_previous(
                                &cfg,
                                &player,
                                &mut notifications,
                                &mut now_playing,
                                &mut previous_track,
                                &mut app.queue,
                            );
                        }
                        KeyCode::Char('*') => {
                            if let Some(item) = bv.tracks.get(bv.selected).cloned() {
                                status =
                                    match toggle_star(&item, &mut favorites, &mut remote_starred) {
                                        Ok(true) => "starred".to_string(),
                                        Ok(false) => "unstarred".to_string(),
                                        Err(msg) => {
                                            notifications.push(Severity::Error, msg.clone());
                                            msg
                                        }
                                    };
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
                        KeyCode::Char('j') | KeyCode::Down => {
                            lyrics_scroll = lyrics_scroll.saturating_add(1)
                        }
                        KeyCode::Char('k') | KeyCode::Up => {
                            lyrics_scroll = lyrics_scroll.saturating_sub(1)
                        }
                        KeyCode::Char('L') | KeyCode::Esc | KeyCode::Char('q') => {
                            lyrics_lines = None;
                            lyrics_scroll = 0;
                        }
                        _ => {}
                    }
                    continue;
                }

                if key.code == KeyCode::Char('I') {
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

                if key.code == KeyCode::Char('n') {
                    let mut nm = NavManager::new();
                    nm.connection_status = test_navidrome_connectivity(&cfg.navidrome_servers);
                    for (nav, result) in cfg.navidrome_servers.iter().zip(&nm.connection_status) {
                        if *result == Some(false) {
                            notifications
                                .push(Severity::Alert, format!("{} is unreachable", nav.url));
                        }
                    }
                    nav_manager = Some(nm);
                    continue;
                }

                if key.code == KeyCode::Char('S') {
                    let bv = open_starred_view(&cfg.navidrome_servers, &favorites);
                    real_art_protocol = picker
                        .as_mut()
                        .and_then(|p| bv.art.clone().map(|img| p.new_resize_protocol(img)));
                    browse_view = Some(bv);
                    continue;
                }

                if key.code == KeyCode::Char('m') && now_playing.is_some() {
                    player_minimized = !player_minimized;
                    continue;
                }

                if key.code == KeyCode::Char('c') {
                    crossfade_enabled = !crossfade_enabled;
                    if !crossfade_enabled {
                        fade_in_started_at = None;
                        let _ = player.set_volume(base_volume);
                    }
                    flash = Some((Field::Crossfade, Instant::now()));
                    continue;
                }

                // subtitle sync is the one styling knob worth
                // adjusting live (discovered mid-playback); the rest
                // (size/position/outline/blur/color) are config-only,
                // set once at startup. editing_query is already ruled
                // out above.
                if matches!(key.code, KeyCode::Char(',') | KeyCode::Char('.')) {
                    subtitle_delay += if key.code == KeyCode::Char(',') {
                        -0.5
                    } else {
                        0.5
                    };
                    let _ = player.set_property("sub-delay", serde_json::json!(subtitle_delay));
                    status = format!("subtitle delay: {subtitle_delay:+.1}s");
                    continue;
                }

                // not while typing a query; a space there is a literal
                // character, not a transport command.
                if key.code == KeyCode::Char(' ') && now_playing.is_some() && !app.editing_query {
                    let _ = player.toggle_pause();
                    continue;
                }

                if key.code == KeyCode::Char('f') && now_playing.is_some() {
                    let _ = player.playlist_next();
                    continue;
                }

                if key.code == KeyCode::Char('b') && previous_track.is_some() {
                    status = go_to_previous(
                        &cfg,
                        &player,
                        &mut notifications,
                        &mut now_playing,
                        &mut previous_track,
                        &mut app.queue,
                    );
                    continue;
                }

                if key.code == KeyCode::Char('*') {
                    // the browse-view case is handled inside its own
                    // intercept above; this only ever runs when
                    // nothing's being browsed, so it's always the
                    // selected search result.
                    status = match app.results.get(app.selected).cloned() {
                        Some(item) => match toggle_star(&item, &mut favorites, &mut remote_starred)
                        {
                            Ok(true) => "starred".to_string(),
                            Ok(false) => "unstarred".to_string(),
                            Err(msg) => {
                                notifications.push(Severity::Error, msg.clone());
                                msg
                            }
                        },
                        None => "nothing selected".to_string(),
                    };
                    continue;
                }

                if key.code == KeyCode::Char('L') {
                    status = match &now_playing {
                        Some(item) => match lyrics_query(item) {
                            Some((artist, title, album)) => {
                                match lyrics_client.lookup(&artist, &title, &album, None) {
                                    Ok(Some(l)) => {
                                        // the api's own `instrumental`
                                        // flag, not just "plain/synced
                                        // both happened to be empty";
                                        // a real track with genuinely
                                        // missing lyrics data shouldn't
                                        // be mislabeled instrumental.
                                        let text = if l.instrumental {
                                            "(instrumental, no lyrics)".to_string()
                                        } else {
                                            l.plain.or(l.synced).unwrap_or_else(|| {
                                                "(no lyrics text available)".to_string()
                                            })
                                        };
                                        lyrics_lines =
                                            Some(text.lines().map(str::to_string).collect());
                                        "lyrics loaded (j/k scroll, L/esc close)".to_string()
                                    }
                                    Ok(None) => "no lyrics found".to_string(),
                                    Err(e) => {
                                        let msg = format!("lyrics lookup failed: {e}");
                                        notifications.push(Severity::Error, msg.clone());
                                        msg
                                    }
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
                    // albums/playlists/artists open a track-listing
                    // view instead of playing immediately ("let me
                    // scroll through the songs" first); songs/local
                    // files still play straight away.
                    Action::Play(item @ SearchItem::Album { .. })
                    | Action::Play(item @ SearchItem::Playlist { .. })
                    | Action::Play(item @ SearchItem::Artist { .. }) => {
                        // on success, the opened view itself (title +
                        // track count in its header) is the
                        // confirmation; no need to also repeat it on
                        // the status line.
                        let opened = match &item {
                            SearchItem::Artist { .. } => open_artist_view(&item),
                            _ => open_browse_view(&item),
                        };
                        match opened {
                            Ok(bv) => {
                                real_art_protocol = picker.as_mut().and_then(|p| {
                                    bv.art.clone().map(|img| p.new_resize_protocol(img))
                                });
                                browse_view = Some(bv);
                            }
                            Err(msg) => {
                                notifications.push(Severity::Error, msg.clone());
                                status = msg;
                            }
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
                                    original_queue = std::iter::once(first.clone())
                                        .chain(tracks.iter().cloned())
                                        .collect();
                                    maybe_fetch_subtitles(
                                        &cfg,
                                        &player,
                                        &mut notifications,
                                        &first,
                                    );
                                    previous_track = now_playing.replace(first);
                                    app.queue = tracks;
                                    // a standalone play (not from an
                                    // open album/playlist) takes over
                                    // the full player by default.
                                    player_minimized = false;
                                    if rest == 0 {
                                        s
                                    } else {
                                        format!("{s} (+{rest} more queued)")
                                    }
                                } else {
                                    notifications.push(Severity::Error, s.clone());
                                    s
                                }
                            }
                            Err(msg) => {
                                notifications.push(Severity::Error, msg.clone());
                                msg
                            }
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
                                if n <= 1 {
                                    last_status
                                } else {
                                    format!("queued {n} tracks")
                                }
                            }
                            Err(msg) => {
                                notifications.push(Severity::Error, msg.clone());
                                msg
                            }
                        };
                    }
                    Action::QueueReordered(items) => {
                        let msg = resync_queue(&player, &items);
                        if msg.starts_with("error") {
                            status = msg;
                        } else {
                            flash = Some((Field::Shuffle, Instant::now()));
                        }
                    }
                    Action::SetLoop(mode) => {
                        // "loop all" isn't an mpv concept; it's tracked
                        // ourselves (see `original_queue`) and acted on
                        // when the queue empties, not here. mpv only
                        // ever needs to know about single-file loop.
                        let mpv_loop = if mode == LoopMode::One { "inf" } else { "no" };
                        match player.set_loop_file(mpv_loop) {
                            Ok(()) => flash = Some((Field::Loop, Instant::now())),
                            Err(e) => status = format!("error setting loop: {e}"),
                        }
                    }
                    Action::SetSpeed(speed) => match player.set_speed(speed) {
                        Ok(()) => flash = Some((Field::Speed, Instant::now())),
                        Err(e) => status = format!("error setting speed: {e}"),
                    },
                    Action::SetEq(_name, gains) => {
                        let filter = eq::build_af_filter(&gains);
                        match player.set_af(&filter) {
                            Ok(()) => flash = Some((Field::Eq, Instant::now())),
                            Err(e) => status = format!("error setting eq: {e}"),
                        }
                    }
                    Action::None => {}
                }
            }
        }
    }
    drop(guard);

    Ok(())
}
