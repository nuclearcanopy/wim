mod anim;
mod app;
mod browse;
mod cache;
mod config;
mod download;
mod eq;
mod favorites;
mod keymap;
mod layout;
mod library;
mod listenbrainz;
mod lyrics;
mod mpv;
mod navmanager;
mod notify;
mod onboarding;
mod opensubtitles;
mod persistence;
mod playqueue;
mod queues;
mod search;
mod shellsong;
mod shellsong_host;
mod subsonic;
mod suggest;
mod term;
mod theme;
mod trace;
mod ui;

use std::collections::{HashMap, HashSet};
use std::sync::mpsc::{self, RecvTimeoutError, TryRecvError};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use crossterm::event::{self, Event, KeyCode, KeyModifiers};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui_image::picker::Picker;
use ratatui_image::protocol::StatefulProtocol;
use ratatui_image::StatefulImage;

use app::{Action, LoopMode};
use cache::CacheIndex;
use download::DownloadEvent;
use keymap::{KeyAction, Keymap};
use layout::{PanelLayout, ResizePanel};
use navmanager::{NavManager, NavManagerStep};
use notify::{Notifications, Severity};
use onboarding::{Onboarding, Step};
use playqueue::{PlayQueue, PosChange};
use search::SearchItem;
use shellsong::{Console, ConsoleAction, GoTarget, Op, Target, ViewTarget};
use shellsong_host::{CorpusCandidates, Ramp, SearchSettle, SleepStep};
use theme::{shade, NEAR_BLACK};
use ui::albumart::AsciiArt;
use ui::banner::Banner;
use ui::browse::BrowseListView;
use ui::controlsbar::{
    self, ControlsBar, Field, Nav, NavKeys, Scrub, SCRUB_STEP, SCRUB_STEP_COARSE,
};
use ui::divider::{HorizontalRule, VerticalRule};
use ui::login::{LoginForm, LoginView};
use ui::lyrics_view::LyricsView;
use ui::miniplayer::MiniPlayerBar;
use ui::nav_manager::NavManagerListView;
use ui::notify::NotificationOverlay;
use ui::onboarding::{ControlsView, DoneView, LibraryPathsView, ListenBrainzView, WelcomeView};
use ui::player::PlayerView;
use ui::results_list::{label, ResultsList};
use ui::searchbox::SearchBox;
use ui::shellsong::ShellsongConsole;
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

/// Same as `playable_path`, but checks the local cache (see `cache.rs`)
/// for a remote `Song` first: a valid cached file plays back instantly
/// with no network round-trip at all, which is the entire payoff of
/// caching in the first place. Falls through to `playable_path` (a
/// stream url) whenever nothing's cached, or for anything that was
/// never a candidate for caching to begin with.
fn resolve_playable(item: &SearchItem, cache: &Arc<Mutex<CacheIndex>>) -> Option<String> {
    if let SearchItem::Song { id, client, .. } = item {
        if let Some(path) = cache.lock().ok().and_then(|idx| idx.cached_path(id)) {
            return Some(path.to_string_lossy().into_owned());
        }
        return Some(client.stream_url(id));
    }
    playable_path(item)
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
        SearchItem::Artist { id, name, client } => {
            artist_tracks(client, id, name).map(|(tracks, _albums)| tracks)
        }
    }
}

/// Every song actually credited to this artist, across every album
/// including compilations. The Global/Various-Artists fix: `getAlbum`
/// returns each song with its own track-level `artist` field, distinct
/// from the album's credited artist (a compilation like navidrome's
/// "Global"/"Various Artists" convention would otherwise hide every
/// song on it from here); filtering by the song's own field instead of
/// the album's works for any compilation, not a hardcoded special case.
/// Every song actually credited to this artist (see the module doc
/// above for the Global/Various-Artists filtering), plus the
/// album list itself for the artist profile's own ALBUMS column.
fn artist_tracks(
    client: &Arc<subsonic::Client>,
    artist_id: &str,
    artist_name: &str,
) -> Result<(Vec<SearchItem>, Vec<SearchItem>), String> {
    let mut artist_albums = client
        .get_artist(artist_id)
        .map_err(|e| format!("couldn't load artist: {e}"))?;
    // chronological, not whatever order getArtist happened to return
    // (alphabetical); unknown-year albums sort last rather than first.
    artist_albums.sort_by_key(|a| a.year.unwrap_or(u32::MAX));

    // one getAlbum round-trip per album, done one at a time, was the
    // whole reason opening a prolific artist's profile felt so slow
    // (dozens of sequential network calls, each paying its own
    // latency); they're entirely independent requests, so fetch them
    // all in parallel instead. `Client` wraps a `ureq::Agent`, which
    // is explicitly safe to share and use concurrently across
    // threads for exactly this.
    let details: Vec<Option<subsonic::AlbumDetails>> = thread::scope(|scope| {
        artist_albums
            .iter()
            .map(|album| scope.spawn(|| client.get_album(&album.id).ok()))
            .collect::<Vec<_>>()
            .into_iter()
            .map(|handle| handle.join().unwrap_or(None))
            .collect()
    });

    let mut tracks = Vec::new();
    let mut albums = Vec::new();
    for (artist_album, details) in artist_albums.into_iter().zip(details) {
        let Some(details) = details else { continue };
        // AlbumDetails itself carries no name (see subsonic.rs); each
        // of its own songs does, and they all share one.
        let album_name = details
            .songs
            .first()
            .map(|s| s.album.clone())
            .unwrap_or_default();
        albums.push(SearchItem::Album {
            id: artist_album.id,
            name: album_name,
            artist: artist_name.to_string(),
            client: client.clone(),
        });
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
    Ok((tracks, albums))
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
                is_artist: false,
                albums: Vec::new(),
                albums_selected: 0,
                albums_focused: false,
                filter_query: String::new(),
                filtering: false,
                show_track_artist: false,
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
                is_artist: false,
                albums: Vec::new(),
                albums_selected: 0,
                albums_focused: false,
                filter_query: String::new(),
                filtering: false,
                show_track_artist: true,
            })
        }
        _ => unreachable!("open_browse_view only called for Album/Playlist"),
    }
}

/// The artist profile view: a bio (best-effort, `None` if navidrome
/// has nothing for this artist), an ALBUMS column, and a filterable
/// SONGS column (every song actually credited to them across every
/// album, chronologically). See `artist_tracks` for the Global/
/// Various-Artists filtering this relies on.
fn open_artist_view(item: &SearchItem) -> Result<browse::BrowseView, String> {
    let SearchItem::Artist { id, name, client } = item else {
        unreachable!("open_artist_view only called for Artist");
    };
    let bio = client.get_artist_bio(id).ok().flatten();
    let (tracks, albums) = artist_tracks(client, id, name)?;
    Ok(browse::BrowseView {
        title: name.clone(),
        subtitle: format!("{} songs", tracks.len()),
        year: None,
        tracks,
        selected: 0,
        art: None,
        bio,
        is_artist: true,
        albums,
        albums_selected: 0,
        albums_focused: false,
        filter_query: String::new(),
        filtering: false,
        show_track_artist: false,
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
    let per_server: Vec<Vec<SearchItem>> = thread::scope(|scope| {
        servers
            .iter()
            .map(|nav| {
                scope.spawn(|| {
                    let client = Arc::new(subsonic::Client::new(nav).ok()?);
                    let songs = client.get_starred_songs().ok()?;
                    Some(browse::songs_to_items(songs, &client))
                })
            })
            .collect::<Vec<_>>()
            .into_iter()
            .map(|handle| handle.join().unwrap_or(None).unwrap_or_default())
            .collect()
    });
    let mut tracks: Vec<SearchItem> = per_server.into_iter().flatten().collect();
    for path in favorites.paths() {
        if let Some(item) = persistence::item_from_path(path) {
            tracks.push(item);
        }
    }
    let mut view = browse::BrowseView {
        title: "starred songs".to_string(),
        subtitle: String::new(),
        year: None,
        tracks: Vec::new(),
        selected: 0,
        art: None,
        bio: None,
        is_artist: false,
        albums: Vec::new(),
        albums_selected: 0,
        albums_focused: false,
        filter_query: String::new(),
        filtering: false,
        show_track_artist: true,
    };
    // through `set_tracks`, not a bare assignment: it re-clamps
    // `selected` with the list it's an index into.
    view.set_tracks(tracks);
    view
}

/// Best-effort: a missing/unreadable cover is not worth surfacing as
/// an error, the album view just shows no art for it.
fn fetch_cover_art(client: &subsonic::Client, album_id: &str) -> Option<image::DynamicImage> {
    let bytes = client.get_cover_art_bytes(album_id).ok()?;
    image::load_from_memory(&bytes).ok()
}

/// Whether the config names a graphics protocol outright, which is an
/// explicit ask for real raster art; `auto` isn't one of those (it
/// documents ascii as its starting point, with 'i' to switch).
fn wants_real_art(mode: config::AlbumArtMode) -> bool {
    matches!(
        mode,
        config::AlbumArtMode::Sixel | config::AlbumArtMode::Kitty | config::AlbumArtMode::Iterm2
    )
}

/// `Auto`/`Blocks` both start in ascii (the strictly-monochrome
/// default); a named protocol starts in real-image mode, but only once
/// the terminal has actually answered the graphics probe (see the
/// `picker_rx` drain). Real mode with no `Picker` renders no art at
/// all, so claiming it before the answer is in would mean an empty
/// pane rather than the ascii fallback. `i` still cycles through all
/// three regardless of where it started.
fn art_mode_for_picker(mode: config::AlbumArtMode, protocol_detected: bool) -> browse::ArtMode {
    match mode {
        config::AlbumArtMode::Off => browse::ArtMode::Off,
        _ if wants_real_art(mode) && protocol_detected => browse::ArtMode::Real,
        _ => browse::ArtMode::Ascii,
    }
}

/// Real cell height:width ratio from the terminal's own reported
/// pixel size, rather than assuming a fixed 2:1; most terminals that
/// matter here (kitty, alacritty, foot, wezterm, ...) answer this via
/// the same ioctl crossterm uses, and a fixed guess is exactly what
/// made album art look slightly too tall at some sizes but not
/// others, depending how close the real font metrics happened to
/// land to "2". Falls back to 2.0 when a terminal reports zero pixel
/// dims (no pixel-size support at all).
fn cell_aspect() -> f32 {
    match crossterm::terminal::window_size() {
        Ok(ws) if ws.width > 0 && ws.height > 0 && ws.columns > 0 && ws.rows > 0 => {
            let cell_w = ws.width as f32 / ws.columns as f32;
            let cell_h = ws.height as f32 / ws.rows as f32;
            (cell_h / cell_w).clamp(1.0, 4.0)
        }
        _ => 2.0,
    }
}

/// A subtitle file the background fetch wrote to disk. `sub_add` isn't
/// called there: the `Mpv` handle isn't `Send`, so the main thread
/// loads it when this lands, same division of labor as every other
/// background worker in this file.
struct SubtitleReady {
    path: String,
    release: String,
    language: String,
}

/// Starts an opensubtitles search + download on its own thread, same
/// fire-and-forget contract as `fire_playing_now`: a no-op whenever
/// `item` isn't a local video, no api key is configured, or the
/// search/download fails for any reason (no matches, network error,
/// quota exhausted). Two network round-trips, so it never runs inline;
/// it used to, which on startup meant holding the first frame behind
/// opensubtitles' response time.
fn fire_subtitle_fetch(
    cfg: &config::Config,
    subtitle_tx: &mpsc::Sender<SubtitleReady>,
    item: &SearchItem,
) {
    let SearchItem::LocalVideo { path, title } = item else {
        return;
    };
    let Some(api_key) = cfg.resolve_opensubtitles_api_key() else {
        return;
    };
    let language = cfg.subtitle_language.clone();
    let title = title.clone();
    let sub_path = path.with_extension("wim-auto.srt");
    let subtitle_tx = subtitle_tx.clone();
    thread::spawn(move || {
        let client = opensubtitles::Client::new(api_key);
        let Ok(results) = client.search(&title, &language) else {
            return;
        };
        let Some(best) = results.first() else {
            return;
        };
        let Ok(bytes) = client.download(best.file_id) else {
            return;
        };
        if std::fs::write(&sub_path, bytes).is_err() {
            return;
        }
        let _ = subtitle_tx.send(SubtitleReady {
            path: sub_path.to_string_lossy().into_owned(),
            release: best.release.clone(),
            language: best.language.clone(),
        });
    });
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

/// `None` for anything that isn't a single track with a known artist:
/// local video has no artist field at all, and remote browse entries
/// (artist/album/playlist) never reach `now_playing` directly, they're
/// always expanded into `Song`s first via `resolve_tracks`.
fn scrobble_metadata(item: &SearchItem) -> Option<listenbrainz::TrackMetadata> {
    match item {
        SearchItem::LocalMusic {
            title,
            artist,
            album,
            ..
        } => Some(listenbrainz::TrackMetadata {
            artist_name: artist.clone()?,
            track_name: title.clone(),
            release_name: album.clone(),
        }),
        SearchItem::Song {
            title,
            artist,
            album,
            ..
        } => Some(listenbrainz::TrackMetadata {
            artist_name: artist.clone(),
            track_name: title.clone(),
            release_name: if album.is_empty() {
                None
            } else {
                Some(album.clone())
            },
        }),
        SearchItem::LocalVideo { .. }
        | SearchItem::Artist { .. }
        | SearchItem::Album { .. }
        | SearchItem::Playlist { .. } => None,
    }
}

/// Fires a "playing_now" listen on a background thread; fire-and-
/// forget, a slow/failed ListenBrainz request must never block
/// playback or the ui. A no-op if listenbrainz isn't configured (no
/// token) or `item` has no scrobbleable metadata.
fn fire_playing_now(cfg: &config::Config, item: &SearchItem) {
    let Some(meta) = scrobble_metadata(item) else {
        return;
    };
    let Some(client) = listenbrainz::Client::from_config(cfg) else {
        return;
    };
    thread::spawn(move || {
        let _ = client.submit_playing_now(&meta);
    });
}

/// Fires a "single" (scrobble) listen on a background thread; same
/// fire-and-forget contract as `fire_playing_now`.
fn fire_scrobble(cfg: &config::Config, item: &SearchItem, listened_at: i64) {
    let Some(meta) = scrobble_metadata(item) else {
        return;
    };
    let Some(client) = listenbrainz::Client::from_config(cfg) else {
        return;
    };
    thread::spawn(move || {
        let _ = client.submit_single(&meta, listened_at);
    });
}

/// Per-track scrobble bookkeeping, bundled into one struct (rather than
/// two loose locals threaded through every playback function) so
/// `go_to_previous` doesn't trip clippy's argument-count limit.
/// `start_tracking_playback` resets this every time `now_playing`
/// changes to a new track.
#[derive(Default)]
struct ScrobbleState {
    /// Set once the "single" listen has fired for the current track,
    /// so the per-tick threshold check (main loop) only ever fires it
    /// once, not every tick after the threshold is crossed.
    scrobbled: bool,
    /// The current track's own start time, not whenever a scrobble
    /// happens to fire; used as the `single` submission's
    /// `listened_at`.
    started_unix: Option<i64>,
}

/// Called from every site that sets `now_playing` to a new `Some(..)`:
/// resets the per-track scrobble bookkeeping and fires "playing_now".
/// Centralized here so each call site is one line instead of
/// repeating all three steps (easy to silently miss one otherwise).
fn start_tracking_playback(cfg: &config::Config, item: &SearchItem, scrobble: &mut ScrobbleState) {
    scrobble.scrobbled = false;
    scrobble.started_unix = Some(listenbrainz::now_unix());
    fire_playing_now(cfg, item);
}

/// Replaces mpv's whole playlist and plays `item` immediately. Returns
/// the status line and whether it actually started playing, so the
/// caller can update `now_playing` only on success. Callers are
/// expected to have already run `item` through `resolve_tracks`, so in
/// practice `playable_path` never actually returns `None` here; that
/// arm only exists because `playable_path` is total over `SearchItem`.
fn try_play(
    player: &mpv::Mpv,
    item: &SearchItem,
    cache: &Arc<Mutex<CacheIndex>>,
) -> (String, bool) {
    match resolve_playable(item, cache) {
        Some(path) => match player.load_file(&path) {
            Ok(()) => {
                set_vid_for_item(player, item);
                (format!("playing: {}", label(item)), true)
            }
            Err(e) => (format!("error loading file: {e}"), false),
        },
        None => (
            "not directly playable (expand via resolve_tracks first)".to_string(),
            false,
        ),
    }
}

/// mpv defaults to opening its own native window for any file with a
/// video track, including showing embedded cover art as a static
/// "video" for plain audio files; explicitly disabling it for
/// anything that isn't actually `LocalVideo` stops a window from
/// popping up for ordinary music playback. Properly embedding actual
/// video in the workspace is separate, larger work; this is the
/// narrow fix for audio never deserving a window at all.
fn set_vid_for_item(player: &mpv::Mpv, item: &SearchItem) {
    let vid = if matches!(item, SearchItem::LocalVideo { .. }) {
        serde_json::json!("auto")
    } else {
        serde_json::json!("no")
    };
    let _ = player.set_property("vid", vid);
}

/// Whether a playlist rewrite starts a fresh session (so "loop all"
/// restarts from it) or just re-lays-out the session already running.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SessionStart {
    New,
    Keep,
}

/// The only way mpv's playlist is ever replaced wholesale: `items[0]`
/// takes over from whatever was loaded, the rest are appended for
/// gapless auto-advance, and `queue` is re-pointed at that exact list
/// so index `i` resolves to `items[i]`. Writing both sides here (and in
/// `enqueue_track`/`resync_upcoming`) is what keeps the index map
/// honest; the caller still fires its own track-change hooks for
/// `items[0]`, since `queue` already knows it is at position 0 and will
/// report mpv's confirming `playlist-pos` as `Unchanged`.
fn load_playlist(
    player: &mpv::Mpv,
    cache: &Arc<Mutex<CacheIndex>>,
    queue: &mut PlayQueue,
    items: Vec<SearchItem>,
    session: SessionStart,
) -> (String, bool) {
    let Some(first) = items.first() else {
        return ("nothing to play".to_string(), false);
    };
    let (status, played) = try_play(player, first, cache);
    if !played {
        return (status, false);
    }
    for item in &items[1..] {
        if let Some(path) = resolve_playable(item, cache) {
            let _ = player.playlist_append(&path);
        }
    }
    match session {
        SessionStart::New => queue.reset(items),
        SessionStart::Keep => queue.rebuild(items),
    }
    (status, true)
}

/// Appends `item` to mpv's own playlist and to the index map together
/// (plays immediately if nothing was loaded, otherwise queues
/// gaplessly; mpv auto-advances its own playlist with none of the
/// round-trip gap reloading on our end would introduce). While shuffle
/// is on the new entry is then moved into a random upcoming slot, which
/// `App::enqueue` used to do to its own queue copy alone, desyncing it
/// from mpv's strictly-appended playlist on the spot.
fn enqueue_track(
    player: &mpv::Mpv,
    item: &SearchItem,
    cache: &Arc<Mutex<CacheIndex>>,
    queue: &mut PlayQueue,
    shuffle: bool,
) -> String {
    let Some(path) = resolve_playable(item, cache) else {
        return "not directly playable (expand via resolve_tracks first)".to_string();
    };
    if let Err(e) = player.playlist_append(&path) {
        return format!("error queuing file: {e}");
    }
    let appended = queue.append(item.clone(), shuffle);
    if let Some(to) = appended.moved_to {
        let _ = player.playlist_move(appended.appended_at, to);
    }
    format!("queued: {}", label(item))
}

/// Resyncs mpv's upcoming playlist to `items` (after a shuffle)
/// without disturbing whatever's currently playing; mpv's own
/// `playlist-clear` keeps the current file and drops the rest, so both
/// sides end up as `[current] + items` at position 0.
fn resync_upcoming(
    player: &mpv::Mpv,
    cache: &Arc<Mutex<CacheIndex>>,
    queue: &mut PlayQueue,
    items: Vec<SearchItem>,
) -> String {
    if let Err(e) = player.playlist_clear() {
        return format!("error resyncing queue: {e}");
    }
    for item in &items {
        if let Some(path) = resolve_playable(item, cache) {
            if let Err(e) = player.playlist_append(&path) {
                return format!("error resyncing queue: {e}");
            }
        }
    }
    queue.replace_upcoming(items);
    "shuffled".to_string()
}

/// Snapshots now-playing/queue/position/volume for next launch to
/// resume. Best-effort: a failed save just means nothing to resume
/// next time, not worth surfacing since we're already quitting.
fn save_playback_state(player: &mpv::Mpv, queue: &PlayQueue) {
    let state = persistence::PlaybackState {
        now_playing_path: queue.current().and_then(playable_path),
        queue_paths: queue.upcoming().iter().filter_map(playable_path).collect(),
        position_secs: player
            .get_property("time-pos")
            .ok()
            .and_then(|v| v.as_f64()),
        volume: player.get_property("volume").ok().and_then(|v| v.as_f64()),
    };
    let _ = persistence::save(&state);
}

/// Bundles everything the auto/manual download paths need to touch:
/// the shared cache index, the channel to report progress back on,
/// and the set of track ids currently mid-transfer. One struct instead
/// of four separate parameters, mostly so `go_to_previous` (which
/// already takes half a dozen other params) doesn't trip clippy's
/// too-many-arguments lint; see `note_playback_started`/
/// `trigger_downloads`, which take the same bundle.
struct DownloadCtx<'a> {
    cache: &'a Arc<Mutex<CacheIndex>>,
    download_tx: &'a mpsc::Sender<DownloadEvent>,
    downloading: &'a mut HashSet<String>,
    /// Distinct play days a track needs before it auto-downloads; see
    /// `config::Config::cache_days_threshold`.
    day_threshold: u32,
}

/// Bundles the two independent "a track just started" trackers
/// (download-on-repeat-listen, scrobble-on-threshold) into one
/// argument, purely to keep `go_to_previous` under clippy's
/// too-many-arguments limit; each one is otherwise unrelated to the
/// other and gets built/used on its own everywhere else.
struct TrackChangeCtx<'a, 'b> {
    dctx: &'a mut DownloadCtx<'b>,
    scrobble: &'a mut ScrobbleState,
    /// where `fire_subtitle_fetch` reports back; bundled here for the
    /// same argument-count reason as everything else in this struct.
    subtitle_tx: &'a mpsc::Sender<SubtitleReady>,
}

/// "Back" re-plays whatever was playing before the current track,
/// putting the interrupted track right behind it so playback picks up
/// where it would have anyway. Deliberately our own bookkeeping rather
/// than mpv's `playlist-prev`: the already-played prefix of mpv's
/// playlist is dropped by every `playlist-clear`, so prev isn't
/// reliably still there to step back onto. Rebuilding the playlist as
/// `[prev, interrupted, ...upcoming]` through `load_playlist` keeps
/// mpv and the index map written together, which inserting into a
/// separate queue copy (while mpv's own playlist got replaced out from
/// under it) did not. Only ever one step back, not a full history.
fn go_to_previous(
    cfg: &config::Config,
    player: &mpv::Mpv,
    notifications: &mut Notifications,
    previous_track: &mut Option<SearchItem>,
    queue: &mut PlayQueue,
    hooks: &mut TrackChangeCtx,
) -> String {
    let Some(prev) = previous_track.take() else {
        return "no previous track".to_string();
    };
    let mut items = vec![prev.clone()];
    items.extend(queue.current().cloned());
    items.extend(queue.upcoming().to_vec());
    let (status, played) =
        load_playlist(player, hooks.dctx.cache, queue, items, SessionStart::Keep);
    if played {
        fire_subtitle_fetch(cfg, hooks.subtitle_tx, &prev);
        note_playback_started(&prev, hooks.dctx, notifications);
        start_tracking_playback(cfg, &prev, hooks.scrobble);
    } else {
        *previous_track = Some(prev);
    }
    status
}

/// What the main loop should do once an action has been handled.
/// `Action::Quit` has to break that loop, which a called function
/// can't do for its caller, so it reports back instead.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Dispatch {
    Continue,
    Quit,
}

/// Every bit of main-loop state an `Action` can touch, bundled into
/// one argument to stay under clippy's too-many-arguments limit (same
/// reasoning as `TrackChangeCtx`). `DownloadCtx` is rebuilt from
/// these fields per use rather than held, so `notifications` stays
/// borrowable alongside it.
struct ActionCtx<'a> {
    cfg: &'a config::Config,
    player: &'a mpv::Mpv,
    app: &'a mut app::App,
    notifications: &'a mut Notifications,
    /// mpv's playlist as an index -> item map; the only queue there is,
    /// and the only thing "what's playing" is ever read out of.
    queue: &'a mut PlayQueue,
    previous_track: &'a mut Option<SearchItem>,
    player_minimized: &'a mut bool,
    browse_view: &'a mut Option<browse::BrowseView>,
    real_art_protocol: &'a mut Option<StatefulProtocol>,
    picker: &'a mut Option<Picker>,
    focus: &'a mut Focus,
    flash: &'a mut Option<(Field, Instant)>,
    cache: &'a Arc<Mutex<CacheIndex>>,
    download_tx: &'a mpsc::Sender<DownloadEvent>,
    downloading: &'a mut HashSet<String>,
    scrobble: &'a mut ScrobbleState,
    /// Where a backgrounded subtitle fetch reports back; see
    /// `fire_subtitle_fetch`, which no longer blocks the input path.
    subtitle_tx: &'a mpsc::Sender<SubtitleReady>,
}

/// The one place an `Action` from `app.handle_key` is acted on. Both
/// key paths route through here (ordinary keys, and a query being
/// typed); the typing path used to discard its action, which is why
/// the first enter after a search only closed the text box.
fn dispatch_action(action: Action, ctx: &mut ActionCtx) -> Dispatch {
    match action {
        Action::Quit => {
            save_playback_state(ctx.player, ctx.queue);
            return Dispatch::Quit;
        }
        // albums/playlists/artists open a track-listing view instead
        // of playing immediately ("let me scroll through the songs"
        // first); songs/local files still play straight away.
        Action::Play(item @ SearchItem::Album { .. })
        | Action::Play(item @ SearchItem::Playlist { .. })
        | Action::Play(item @ SearchItem::Artist { .. }) => {
            // on success, the opened view itself (title + track count
            // in its header) is the confirmation; no need to also
            // repeat it on the status line.
            let opened = match &item {
                SearchItem::Artist { .. } => open_artist_view(&item),
                _ => open_browse_view(&item),
            };
            match opened {
                Ok(bv) => {
                    *ctx.real_art_protocol = ctx
                        .picker
                        .as_mut()
                        .and_then(|p| bv.art.clone().map(|img| p.new_resize_protocol(img)));
                    *ctx.browse_view = Some(bv);
                    // opening something to browse and then still
                    // needing a separate keypress just to be able to
                    // scroll it read as "enter doesn't do anything";
                    // land with it already focused instead.
                    *ctx.focus = Focus::Workspace;
                }
                Err(msg) => ctx.notifications.push(Severity::Error, msg),
            };
        }
        Action::Play(item) => {
            match resolve_tracks(&item) {
                Ok(tracks) if tracks.is_empty() => ctx
                    .notifications
                    .push(Severity::Info, "nothing to play (empty album or playlist)"),
                Ok(tracks) => {
                    let first = tracks[0].clone();
                    let interrupted = ctx.queue.current().cloned();
                    let (s, played) =
                        load_playlist(ctx.player, ctx.cache, ctx.queue, tracks, SessionStart::New);
                    if played {
                        fire_subtitle_fetch(ctx.cfg, ctx.subtitle_tx, &first);
                        let mut dctx = DownloadCtx {
                            cache: ctx.cache,
                            download_tx: ctx.download_tx,
                            downloading: ctx.downloading,
                            day_threshold: ctx.cfg.cache_days_threshold,
                        };
                        note_playback_started(&first, &mut dctx, ctx.notifications);
                        start_tracking_playback(ctx.cfg, &first, ctx.scrobble);
                        *ctx.previous_track = interrupted;
                        // a standalone play (not from an open
                        // album/playlist) takes over the full player
                        // by default.
                        *ctx.player_minimized = false;
                    } else {
                        ctx.notifications.push(Severity::Error, s);
                    }
                }
                Err(msg) => ctx.notifications.push(Severity::Error, msg),
            }
        }
        Action::Queued(item) => {
            // queueing has no on-screen consequence of its own (the
            // queue count on the bottom bar moves, but that's easy to
            // miss), so this is one of the few things worth an actual
            // toast.
            match resolve_tracks(&item) {
                Ok(tracks) => {
                    for t in &tracks {
                        let s = enqueue_track(ctx.player, t, ctx.cache, ctx.queue, ctx.app.shuffle);
                        if s.starts_with("error") {
                            ctx.notifications.push(Severity::Error, s);
                        }
                    }
                    let n = tracks.len();
                    ctx.notifications.push(
                        Severity::Info,
                        if n == 1 {
                            "queued 1 track".to_string()
                        } else {
                            format!("queued {n} tracks")
                        },
                    );
                }
                Err(msg) => ctx.notifications.push(Severity::Error, msg),
            }
        }
        Action::ShuffleToggled(on) => {
            // turning it off leaves the order alone (nothing to put
            // back); turning it on reshuffles what's still upcoming so
            // it takes effect now, not only on later additions.
            if on {
                let shuffled = ctx.queue.shuffled_upcoming();
                let msg = resync_upcoming(ctx.player, ctx.cache, ctx.queue, shuffled);
                if msg.starts_with("error") {
                    ctx.notifications.push(Severity::Error, msg);
                    return Dispatch::Continue;
                }
            }
            *ctx.flash = Some((Field::Shuffle, Instant::now()));
        }
        Action::SetLoop(mode) => {
            // "loop all" isn't an mpv concept; it's tracked ourselves
            // (see `PlayQueue::session`) and acted on once mpv's
            // playlist drains, not here. mpv only ever needs to know
            // about single-file loop.
            let mpv_loop = if mode == LoopMode::One { "inf" } else { "no" };
            match ctx.player.set_loop_file(mpv_loop) {
                Ok(()) => *ctx.flash = Some((Field::Loop, Instant::now())),
                Err(e) => ctx
                    .notifications
                    .push(Severity::Error, format!("error setting loop: {e}")),
            }
        }
        Action::SetSpeed(speed) => match ctx.player.set_speed(speed) {
            Ok(()) => *ctx.flash = Some((Field::Speed, Instant::now())),
            Err(e) => ctx
                .notifications
                .push(Severity::Error, format!("error setting speed: {e}")),
        },
        Action::SetEq(_name, gains) => {
            let filter = eq::build_af_filter(&gains);
            match ctx.player.set_af(&filter) {
                Ok(()) => *ctx.flash = Some((Field::Eq, Instant::now())),
                Err(e) => ctx
                    .notifications
                    .push(Severity::Error, format!("error setting eq: {e}")),
            }
        }
        Action::None => {}
    }
    Dispatch::Continue
}

// ------------------------------------------------------ shellsong

/// The tracks enter on a browse row plays: the selected one through
/// the end of what the filter actually shows, not of the whole list (a
/// filtered artist page used to play the unfiltered tail). Shared by
/// the browse view's own enter and shellsong's `go()`, so the two
/// cannot pick different tracks.
fn browse_session(bv: &browse::BrowseView) -> Vec<SearchItem> {
    let visible = bv.visible_track_indices();
    let from = visible.iter().position(|&i| i == bv.selected).unwrap_or(0);
    visible[from..]
        .iter()
        .filter_map(|&i| bv.tracks.get(i).cloned())
        .collect()
}

/// Starts a freshly built session: mpv's playlist is replaced and
/// every "a track just started" hook fires for the first track, the
/// same set `Action::Play` fires. One place, so a shellsong-built
/// session can't quietly skip a hook the keyboard runs.
fn start_session(
    items: Vec<SearchItem>,
    minimize: bool,
    ctx: &mut ActionCtx,
) -> Result<String, String> {
    let Some(first) = items.first().cloned() else {
        return Err("nothing to play".to_string());
    };
    let interrupted = ctx.queue.current().cloned();
    let (status, played) =
        load_playlist(ctx.player, ctx.cache, ctx.queue, items, SessionStart::New);
    if !played {
        return Err(status);
    }
    fire_subtitle_fetch(ctx.cfg, ctx.subtitle_tx, &first);
    let mut dctx = DownloadCtx {
        cache: ctx.cache,
        download_tx: ctx.download_tx,
        downloading: ctx.downloading,
        day_threshold: ctx.cfg.cache_days_threshold,
    };
    note_playback_started(&first, &mut dctx, ctx.notifications);
    start_tracking_playback(ctx.cfg, &first, ctx.scrobble);
    *ctx.previous_track = interrupted;
    *ctx.player_minimized = minimize;
    Ok(status)
}

/// `main::Focus` is the host's, `shellsong::Focus` the language's
/// mirror of it (kept separate so the language depends on nothing in
/// the ui). One to one.
fn shellsong_focus(focus: Focus) -> shellsong::Focus {
    match focus {
        Focus::Sidebar => shellsong::Focus::Sidebar,
        Focus::Workspace => shellsong::Focus::Workspace,
        Focus::BottomBar => shellsong::Focus::BottomBar,
    }
}

/// Everything a running program knows about the world, rebuilt from
/// live state before every `step`. `queue_len` counts the whole
/// playlist, played prefix included, because that is what the queue
/// view numbers and therefore what `goto(n)`/`remove(n)` mean.
fn shellsong_env(
    app: &app::App,
    queue: &PlayQueue,
    browse_view: &Option<browse::BrowseView>,
    focus: Focus,
    settled: bool,
    now: Duration,
) -> shellsong::Env {
    shellsong::Env {
        now,
        query: app.query.clone(),
        results: app.results.iter().map(shellsong::Kind::of).collect(),
        search_settled: settled,
        queue_len: queue.entries().len(),
        browsing: browse_view.is_some(),
        focus: shellsong_focus(focus),
    }
}

/// mpv's current volume, which `volume(+10%)` nudges from and a fade
/// ramps away from. `None` only if the ipc read fails.
fn current_volume(player: &mpv::Mpv) -> Option<f64> {
    player.get_property("volume").ok().and_then(|v| v.as_f64())
}

/// Whether `item` is starred right now, so `star()`/`unstar()` can set
/// a state rather than flip one (`toggle_star` only flips).
fn is_starred(
    item: &SearchItem,
    favorites: &favorites::Favorites,
    remote_starred: &HashSet<String>,
) -> bool {
    match item {
        SearchItem::LocalMusic { path, .. } | SearchItem::LocalVideo { path, .. } => {
            favorites.is_starred(&path.to_string_lossy())
        }
        SearchItem::Song { id, .. } => remote_starred.contains(id),
        SearchItem::Artist { .. } | SearchItem::Album { .. } | SearchItem::Playlist { .. } => false,
    }
}

/// The album page a song belongs to. A `Song` carries its album's
/// *name*, not its id, so this can only find an album the corpus has
/// already indexed; `view(q, ...)` landing on the album itself is the
/// direct route.
fn album_page(
    corpus: &search::Corpus,
    album: &str,
    client: &Arc<subsonic::Client>,
) -> Option<SearchItem> {
    corpus
        .items()
        .find(|item| match item {
            SearchItem::Album {
                name, client: from, ..
            } => name == album && from.base_url() == client.base_url(),
            _ => false,
        })
        .cloned()
}

/// `add_to_queue(front, ...)`: append, then move the new entry in
/// right behind what is playing. `enqueue_track` still writes both
/// sides (mpv's playlist and the index map); this only reorders, with
/// the same `move_entry` + `playlist_move` pairing the queue view's
/// reorder uses. Appended unshuffled on purpose: `front` means front.
fn enqueue_next(item: &SearchItem, ctx: &mut ActionCtx) -> Result<(), String> {
    let status = enqueue_track(ctx.player, item, ctx.cache, ctx.queue, false);
    if status.starts_with("error") || status.starts_with("not ") {
        return Err(status);
    }
    let from = ctx.queue.entries().len().saturating_sub(1);
    let to = ctx.queue.pos().map_or(0, |p| p + 1);
    if to < from && ctx.queue.move_entry(from, to) {
        if let Err(e) = ctx.player.playlist_move(from, to) {
            return Err(format!("error moving: {e}"));
        }
    }
    Ok(())
}

/// What one op reported into the scrollback. `Described` takes
/// `Op::describe()`, which is usually the right line; the host
/// overrides with `Note` when it knows a better one (a track title, a
/// count) and with `Failed` when the op could not be applied. Every op
/// produces exactly one of these: nothing in shellsong is silent
/// (docs/shellsong.md R9).
enum Report {
    Described,
    Note(String),
    Failed(String),
}

fn op_done() -> (Dispatch, Report) {
    (Dispatch::Continue, Report::Described)
}

fn op_note(text: impl Into<String>) -> (Dispatch, Report) {
    (Dispatch::Continue, Report::Note(text.into()))
}

fn op_fail(text: impl Into<String>) -> (Dispatch, Report) {
    (Dispatch::Continue, Report::Failed(text.into()))
}

/// One ipc write, reported either way; mpv failing is the one thing
/// most of the transport ops can do wrong.
fn op_ipc(result: anyhow::Result<()>) -> (Dispatch, Report) {
    match result {
        Ok(()) => op_done(),
        Err(e) => op_fail(e.to_string()),
    }
}

/// Which way a selection op moves. `up()`/`down()` carry a delta,
/// `top()`/`bottom()` are ends.
enum SelectStep {
    By(i32),
    Top,
    Bottom,
}

/// Every bit of main-loop state a shellsong `Op` can touch that an
/// `Action` cannot, bundled into one argument the way `ActionCtx` and
/// `DownloadCtx` are. `actx` is held rather than copied so every op
/// with an `Action` behind it goes through `dispatch_action`, which is
/// what keeps a shellsong command and a keypress from drifting apart.
struct ShellsongCtx<'a, 'b> {
    actx: &'a mut ActionCtx<'b>,
    console: &'a mut Console,
    /// Cleared by anything `Op::closes_console` says takes over the
    /// screen; the console is a centred overlay and would sit on top
    /// of whatever just opened.
    console_open: &'a mut bool,
    /// The same monotonic reading `Env::now` carries this frame, so a
    /// ramp started by an op and ticked next frame share one clock.
    now: Duration,
    now_playing: &'a Option<SearchItem>,
    favorites: &'a mut favorites::Favorites,
    remote_starred: &'a mut HashSet<String>,
    // `view(...)` targets: each one is the same main-loop local its
    // keybind already writes, never a parallel copy.
    queue_view_open: &'a mut bool,
    queue_selected: &'a mut usize,
    suggestions: &'a mut Option<suggest::Suggestions>,
    suggestions_selected: &'a mut usize,
    settings_menu_open: &'a mut bool,
    keybinds_menu_open: &'a mut bool,
    keybinds_menu_selected: &'a mut usize,
    eq_menu_open: &'a mut bool,
    eq_menu_selected: &'a mut usize,
    lyrics_lines: &'a mut Option<Vec<String>>,
    lyrics_track: &'a mut String,
    lyrics_scroll: &'a mut usize,
    lyrics_client: &'a lyrics::Client,
    // volume, and everything that ramps it.
    base_volume: &'a mut f64,
    crossfade_enabled: &'a mut bool,
    fade_in_started_at: &'a mut Option<Instant>,
    /// `fade(in|out)`'s ramp; `sleep_due` is its deadline-driven
    /// cousin. Both reuse the crossfade ramp's shape rather than
    /// cutting the audio dead.
    fade: &'a mut Option<Ramp>,
    sleep_due: &'a mut Option<Duration>,
    bottombar_selected: Field,
    /// Replaced wholesale by `rescan()`, which needs a fresh channel:
    /// the scan thread signs off by dropping its sender.
    scan_rx: &'a mut mpsc::Receiver<library::ScanBatch>,
    remote_search_tx: &'a mpsc::Sender<RemoteSearchQuery>,
    settle: &'a mut SearchSettle,
    /// Snapshot behind `load_queue`'s completion; refreshed on open
    /// and after a save rather than read off disk per keystroke.
    saved_queues: &'a mut Vec<String>,
}

/// What a term-taking op acts on. `Index` is a row of the results
/// list; `Selection` is whatever is highlighted, resolved exactly the
/// way the keyboard's star/download keys resolve it (a focused browse
/// view owns the highlight, otherwise the sidebar does).
fn shellsong_item(target: Target, ctx: &ShellsongCtx) -> Option<SearchItem> {
    match target {
        Target::Index(i) => ctx.actx.app.results.get(i).cloned(),
        Target::Selection => {
            if let (Some(bv), Focus::Workspace) = (ctx.actx.browse_view.as_ref(), *ctx.actx.focus) {
                return if bv.albums_focused {
                    bv.albums.get(bv.albums_selected).cloned()
                } else {
                    bv.tracks.get(bv.selected).cloned()
                };
            }
            ctx.actx.app.results.get(ctx.actx.app.selected).cloned()
        }
    }
}

/// The selection `up()`/`down()`/`top()`/`bottom()` move is whichever
/// pane owns the keyboard, exactly as j/k and gg/G do: a focused
/// browse view, suggestions menu or queue view owns its own cursor,
/// otherwise it is the sidebar's results list.
fn move_shellsong_selection(step: SelectStep, ctx: &mut ShellsongCtx) -> (Dispatch, Report) {
    if *ctx.actx.focus == Focus::Workspace {
        if let Some(bv) = ctx.actx.browse_view.as_mut() {
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
        if let Some(sug) = ctx.suggestions.as_ref() {
            let last = ui::suggestions::count(sug).saturating_sub(1) as i64;
            let at = *ctx.suggestions_selected as i64;
            let to = match step {
                SelectStep::By(d) => (at + i64::from(d)).clamp(0, last),
                SelectStep::Top => 0,
                SelectStep::Bottom => last,
            };
            *ctx.suggestions_selected = to as usize;
            return op_done();
        }
        if *ctx.queue_view_open {
            let order = ui::queue::selectable(ctx.actx.queue.entries(), ctx.actx.queue.pos());
            if order.is_empty() {
                return op_fail("the queue is empty");
            }
            let at = order
                .iter()
                .position(|i| *i == *ctx.queue_selected)
                .unwrap_or(0) as i64;
            let last = order.len() as i64 - 1;
            let to = match step {
                SelectStep::By(d) => (at + i64::from(d)).clamp(0, last),
                SelectStep::Top => 0,
                SelectStep::Bottom => last,
            };
            *ctx.queue_selected = order[to as usize];
            return op_done();
        }
    }
    match step {
        SelectStep::By(d) => ctx.actx.app.move_selection(d as isize),
        SelectStep::Top => ctx.actx.app.select_top(),
        SelectStep::Bottom => ctx.actx.app.select_bottom(),
    }
    op_done()
}

/// `view(target)`: the one screen-opening op. Every target writes the
/// same state its own keybind writes; the four listings and `help`
/// print into the scrollback instead and leave the console up (see
/// `ViewTarget::is_screen`).
fn open_shellsong_view(target: ViewTarget, ctx: &mut ShellsongCtx) -> (Dispatch, Report) {
    match target {
        ViewTarget::Now => {
            if ctx.now_playing.is_none() {
                return op_fail("nothing is playing");
            }
            *ctx.actx.player_minimized = false;
            op_done()
        }
        ViewTarget::Suggestions => {
            let starred = suggestion_starred_keys(ctx.remote_starred, ctx.favorites);
            let catalog = suggest::Catalog::from_corpus(&ctx.actx.app.corpus);
            let guard = ctx.actx.cache.lock().expect("cache mutex poisoned");
            let suggestions = suggest::suggest(&suggest::Request::new(
                &catalog,
                &guard,
                &starred,
                cache::now_secs(),
            ));
            drop(guard);
            let total = ui::suggestions::count(&suggestions);
            *ctx.suggestions = Some(suggestions);
            *ctx.suggestions_selected = 0;
            *ctx.actx.focus = Focus::Workspace;
            op_note(format!("{total} suggestions"))
        }
        ViewTarget::Settings => {
            *ctx.settings_menu_open = true;
            op_done()
        }
        ViewTarget::Keybinds => {
            *ctx.keybinds_menu_open = true;
            *ctx.keybinds_menu_selected = 0;
            op_done()
        }
        ViewTarget::Eq => {
            *ctx.eq_menu_open = true;
            *ctx.eq_menu_selected = if ctx.actx.app.eq_custom {
                ui::eq_menu::PRESET_COUNT
            } else {
                ctx.actx.app.eq_preset
            };
            op_done()
        }
        ViewTarget::Lyrics => open_shellsong_lyrics(ctx),
        ViewTarget::Starred => {
            let bv = open_starred_view(&ctx.actx.cfg.navidrome_servers, ctx.favorites);
            let count = bv.tracks.len();
            *ctx.actx.real_art_protocol = ctx
                .actx
                .picker
                .as_mut()
                .and_then(|p| bv.art.clone().map(|img| p.new_resize_protocol(img)));
            *ctx.actx.browse_view = Some(bv);
            // same as opening an album: the listing draws its own
            // cursor, so j/k have to move it rather than the hidden
            // sidebar behind it.
            *ctx.actx.focus = Focus::Workspace;
            op_note(format!("{count} starred"))
        }
        ViewTarget::Queue => {
            let pos = ctx.actx.queue.pos();
            let lines: Vec<String> = ctx
                .actx
                .queue
                .entries()
                .iter()
                .enumerate()
                .map(|(i, item)| {
                    let here = if Some(i) == pos { '>' } else { ' ' };
                    format!("{here}{:>3}. {}", i + 1, label(item))
                })
                .collect();
            if lines.is_empty() {
                return op_note("the queue is empty");
            }
            let count = lines.len();
            for line in lines {
                ctx.console.note(line);
            }
            op_note(format!("{count} in the queue"))
        }
        ViewTarget::Index => {
            let rows = ctx.actx.app.corpus.breakdown();
            for (source, count) in &rows {
                ctx.console.note(format!("{source}: {count}"));
            }
            op_note(format!("{} sources indexed", rows.len()))
        }
        ViewTarget::History => {
            // cloned first: the scrollback is being pushed into, and
            // the history lives on the same `Console`.
            let lines: Vec<String> = ctx.console.history().to_vec();
            if lines.is_empty() {
                return op_note("no history yet");
            }
            let count = lines.len();
            for line in lines {
                ctx.console.note(format!(": {line}"));
            }
            op_note(format!("{count} past commands"))
        }
        ViewTarget::Playlists => {
            *ctx.saved_queues = queues::list();
            if ctx.saved_queues.is_empty() {
                return op_note("no saved queues; save_queue(name) makes one");
            }
            let names = ctx.saved_queues.clone();
            let count = names.len();
            for name in names {
                ctx.console.note(name);
            }
            op_note(format!("{count} saved queues"))
        }
        // generated by the language itself (so `help()` and
        // `view(help)` can't drift), lowered to notes rather than to
        // an op; handled anyway rather than left to a wildcard.
        ViewTarget::Help => {
            for line in shellsong::help_lines() {
                ctx.console.note(line);
            }
            op_done()
        }
    }
}

/// `view(lyrics)`: the same blocking lookup the lyrics keybind does,
/// reporting into the console instead of a toast.
fn open_shellsong_lyrics(ctx: &mut ShellsongCtx) -> (Dispatch, Report) {
    let Some(item) = ctx.now_playing.clone() else {
        return op_fail("nothing is playing");
    };
    let Some((artist, title, album)) = lyrics_query(&item) else {
        return op_fail("no lyrics for this item");
    };
    match ctx.lyrics_client.lookup(&artist, &title, &album, None) {
        Ok(Some(l)) => {
            // the api's own `instrumental` flag, not "both fields
            // happened to be empty".
            let text = if l.instrumental {
                "(instrumental, no lyrics)".to_string()
            } else {
                l.plain
                    .or(l.synced)
                    .unwrap_or_else(|| "(no lyrics text available)".to_string())
            };
            *ctx.lyrics_lines = Some(text.lines().map(str::to_string).collect());
            *ctx.lyrics_track = label(&item);
            *ctx.lyrics_scroll = 0;
            op_done()
        }
        Ok(None) => op_fail("no lyrics found"),
        Err(e) => op_fail(format!("lyrics lookup failed: {e}")),
    }
}

/// Applies one op and reports exactly one line into the scrollback.
/// The report is `Op::describe()` unless the host knew something
/// better; `Op::closes_console` is consulted here, in one place,
/// rather than by each view-opener remembering to.
fn apply_shellsong_op(op: Op, ctx: &mut ShellsongCtx) -> Dispatch {
    let described = op.describe();
    let closes = op.closes_console();
    let (dispatch, report) = run_shellsong_op(op, ctx);
    // only on success: closing the console to make room for a screen
    // that then failed to open would take the error message with it.
    if closes && !matches!(report, Report::Failed(_)) {
        *ctx.console_open = false;
    }
    match report {
        Report::Described => ctx.console.note(described),
        Report::Note(text) => ctx.console.note(text),
        Report::Failed(text) => ctx.console.fail(text),
    }
    dispatch
}

/// The `Op` -> host table. Every arm goes through the call the matching
/// keypress already makes (`dispatch_action` wherever an `Action`
/// exists), so a shellsong command and a keybind can never diverge in
/// behaviour; see docs/shellsong.md.
fn run_shellsong_op(op: Op, ctx: &mut ShellsongCtx) -> (Dispatch, Report) {
    match op {
        Op::TogglePause => {
            if ctx.now_playing.is_none() {
                return op_fail("nothing is playing");
            }
            op_ipc(ctx.actx.player.toggle_pause())
        }
        Op::SkipNext => {
            if ctx.now_playing.is_none() {
                return op_fail("nothing is playing");
            }
            op_ipc(ctx.actx.player.playlist_next())
        }
        Op::SkipBack => {
            if ctx.actx.previous_track.is_none() {
                return op_fail("no previous track");
            }
            let mut dctx = DownloadCtx {
                cache: ctx.actx.cache,
                download_tx: ctx.actx.download_tx,
                downloading: ctx.actx.downloading,
                day_threshold: ctx.actx.cfg.cache_days_threshold,
            };
            let mut hooks = TrackChangeCtx {
                dctx: &mut dctx,
                scrobble: ctx.actx.scrobble,
                subtitle_tx: ctx.actx.subtitle_tx,
            };
            let status = go_to_previous(
                ctx.actx.cfg,
                ctx.actx.player,
                ctx.actx.notifications,
                ctx.actx.previous_track,
                ctx.actx.queue,
                &mut hooks,
            );
            op_note(status)
        }
        Op::SeekAbsolute(pos) => {
            // asked for directly rather than read from the observed
            // cache: this runs once per command, not per frame, and a
            // percent target must resolve against the duration as of
            // now rather than whatever the last pushed change said.
            let duration = ctx
                .actx
                .player
                .get_property(mpv::PROP_DURATION)
                .ok()
                .and_then(|v| v.as_f64());
            match shellsong_host::seek_target(pos, duration) {
                Ok(secs) => match ctx.actx.player.seek(secs, false) {
                    Ok(()) => op_note(format!("moved to {secs:.0}s")),
                    Err(e) => op_fail(e.to_string()),
                },
                Err(why) => op_fail(why),
            }
        }
        Op::SeekRelative(delta) => op_ipc(ctx.actx.player.seek(delta, true)),
        Op::SetVolume(level) => {
            let current = current_volume(ctx.actx.player).unwrap_or(*ctx.base_volume);
            let target = shellsong_host::volume_target(level, current);
            match ctx.actx.player.set_volume(target) {
                // the new level becomes the ceiling the crossfade and
                // fade ramps scale against, or the next fade-in would
                // snap the volume back to whatever it was at startup.
                Ok(()) => {
                    *ctx.base_volume = target;
                    op_note(format!("volume: {target:.0}%"))
                }
                Err(e) => op_fail(format!("error setting volume: {e}")),
            }
        }
        // mpv's own `mute`, not volume 0: it is reversible without
        // having to remember the level, and `volume(...)` stays the one
        // thing that moves the level.
        Op::Mute(on) => op_ipc(ctx.actx.player.set_property("mute", serde_json::json!(on))),
        Op::CycleLoop => {
            let mode = ctx.actx.app.cycle_loop();
            let dispatch = dispatch_action(Action::SetLoop(mode), ctx.actx);
            (dispatch, Report::Note(format!("loop: {}", mode.label())))
        }
        Op::SetLoop(mode) => {
            ctx.actx.app.loop_mode = mode;
            let dispatch = dispatch_action(Action::SetLoop(mode), ctx.actx);
            (dispatch, Report::Described)
        }
        Op::ToggleShuffle => {
            let on = ctx.actx.app.toggle_shuffle();
            let dispatch = dispatch_action(Action::ShuffleToggled(on), ctx.actx);
            (
                dispatch,
                Report::Note(format!("shuffle: {}", if on { "on" } else { "off" })),
            )
        }
        Op::SetShuffle(want) => {
            if ctx.actx.app.shuffle == want {
                return op_note(format!(
                    "shuffle is already {}",
                    if want { "on" } else { "off" }
                ));
            }
            let on = ctx.actx.app.toggle_shuffle();
            let dispatch = dispatch_action(Action::ShuffleToggled(on), ctx.actx);
            (dispatch, Report::Described)
        }
        Op::SetSpeed(speed) => {
            ctx.actx.app.speed = speed;
            let dispatch = dispatch_action(Action::SetSpeed(speed), ctx.actx);
            (dispatch, Report::Described)
        }
        Op::SpeedUp | Op::SpeedDown => {
            let dir = if matches!(op, Op::SpeedUp) { 1.0 } else { -1.0 };
            let speed = ctx.actx.app.bump_speed(dir);
            let dispatch = dispatch_action(Action::SetSpeed(speed), ctx.actx);
            (dispatch, Report::Note(format!("speed: {speed}x")))
        }
        Op::SetEqPreset(index) => {
            // `apply_eq_preset_row` takes an eq-menu row, and rows
            // below `PRESET_COUNT` are exactly the presets in order.
            if index >= ui::eq_menu::PRESET_COUNT {
                return op_fail(format!("no eq preset {index}"));
            }
            apply_eq_preset_row(ctx.actx.app, ctx.actx.player, ctx.actx.notifications, index);
            op_done()
        }
        Op::ToggleCrossfade => {
            *ctx.crossfade_enabled = !*ctx.crossfade_enabled;
            if !*ctx.crossfade_enabled {
                *ctx.fade_in_started_at = None;
                let _ = ctx.actx.player.set_volume(*ctx.base_volume);
            }
            *ctx.actx.flash = Some((Field::Crossfade, Instant::now()));
            op_note(format!(
                "crossfade: {}",
                if *ctx.crossfade_enabled { "on" } else { "off" }
            ))
        }
        Op::Search(query) => {
            ctx.actx.app.set_query(&query);
            let fired = notify_query_changed(ctx.actx.app, ctx.remote_search_tx);
            ctx.settle.notified(fired, ctx.now);
            op_note(format!(
                "search: {query} ({} results so far)",
                ctx.actx.app.results.len()
            ))
        }
        Op::MoveSelection(delta) => move_shellsong_selection(SelectStep::By(delta), ctx),
        Op::SelectTop => move_shellsong_selection(SelectStep::Top, ctx),
        Op::SelectBottom => move_shellsong_selection(SelectStep::Bottom, ctx),
        Op::Go(GoTarget::Result) => {
            let Some(item) = ctx.actx.app.results.get(ctx.actx.app.selected).cloned() else {
                return op_fail("nothing is selected");
            };
            let name = label(&item);
            let dispatch = dispatch_action(Action::Play(item), ctx.actx);
            (dispatch, Report::Note(format!("enter on {name}")))
        }
        Op::Go(GoTarget::BrowseRow) => {
            let session = ctx
                .actx
                .browse_view
                .as_ref()
                .map(browse_session)
                .unwrap_or_default();
            match start_session(session, true, ctx.actx) {
                Ok(status) => op_note(status),
                Err(why) => op_fail(why),
            }
        }
        Op::Go(GoTarget::BottomBarField) => {
            // enter on the bottom bar tweaks whichever field is
            // selected, and every one of those fields already has its
            // own command; reuse them rather than keeping a second
            // copy of the bar's own match.
            let field_op = match ctx.bottombar_selected {
                Field::Pause => Op::TogglePause,
                Field::Loop => Op::CycleLoop,
                Field::Shuffle => Op::ToggleShuffle,
                Field::Speed => Op::SpeedUp,
                Field::Eq => Op::View(ViewTarget::Eq),
                Field::Crossfade => Op::ToggleCrossfade,
                // enter on the scrubber opens a keyboard drag, which
                // the open console would swallow every key of; the
                // language has real seek commands instead.
                Field::Progress => {
                    return op_fail("use moveto(1m20s) or seek(+30s) to scrub from here")
                }
            };
            let closes = field_op.closes_console();
            let result = run_shellsong_op(field_op, ctx);
            if closes && !matches!(result.1, Report::Failed(_)) {
                *ctx.console_open = false;
            }
            result
        }
        Op::Play(target) => {
            // `go()`'s browse-row reading, for the same reason: with a
            // browse view focused, the selection *is* a row of it, and
            // playing one plays through to the end of the listing.
            if target == Target::Selection
                && *ctx.actx.focus == Focus::Workspace
                && ctx.actx.browse_view.is_some()
            {
                return run_shellsong_op(Op::Go(GoTarget::BrowseRow), ctx);
            }
            let Some(item) = shellsong_item(target, ctx) else {
                return op_fail("nothing is selected");
            };
            let name = label(&item);
            let dispatch = dispatch_action(Action::Play(item), ctx.actx);
            (dispatch, Report::Note(format!("playing: {name}")))
        }
        Op::Enqueue { target, front } => {
            let Some(item) = shellsong_item(target, ctx) else {
                return op_fail("nothing is selected");
            };
            if !front {
                // `Action::Queued` expands an album/playlist itself and
                // reports its own count; resolving here as well would
                // cost a second fetch of the very same track list.
                let dispatch = dispatch_action(Action::Queued(item), ctx.actx);
                return (dispatch, Report::Described);
            }
            let tracks = match resolve_tracks(&item) {
                Ok(tracks) => tracks,
                Err(why) => return op_fail(why),
            };
            if tracks.is_empty() {
                return op_fail("nothing to queue (empty album or playlist)");
            }
            // reversed so the batch keeps its own order: each one is
            // inserted directly behind what is playing.
            for track in tracks.iter().rev() {
                if let Err(why) = enqueue_next(track, ctx.actx) {
                    return op_fail(why);
                }
            }
            op_note(format!("queued {} tracks next", tracks.len()))
        }
        Op::Star { target, on } => {
            let Some(item) = shellsong_item(target, ctx) else {
                return op_fail("nothing is selected");
            };
            let verb = if on { "starred" } else { "unstarred" };
            if is_starred(&item, ctx.favorites, ctx.remote_starred) == on {
                return op_note(format!("'{}' is already {verb}", item.primary_name()));
            }
            match toggle_star(&item, ctx.favorites, ctx.remote_starred) {
                Ok(_) => op_note(format!("{verb} '{}'", item.primary_name())),
                Err(why) => op_fail(why),
            }
        }
        Op::Download(target) => {
            let Some(item) = shellsong_item(target, ctx) else {
                return op_fail("nothing is selected");
            };
            let mut dctx = DownloadCtx {
                cache: ctx.actx.cache,
                download_tx: ctx.actx.download_tx,
                downloading: ctx.actx.downloading,
                day_threshold: ctx.actx.cfg.cache_days_threshold,
            };
            let status = trigger_downloads(&item, &mut dctx, ctx.actx.notifications);
            op_note(status)
        }
        Op::OpenPage(target) => {
            let Some(item) = shellsong_item(target, ctx) else {
                return op_fail("nothing is selected");
            };
            // an album/artist/playlist *is* a page, and
            // `Action::Play` on one opens it rather than playing it; a
            // song has no page of its own, so the page it belongs to
            // is its album's. A local file has neither.
            let page = match &item {
                SearchItem::Album { .. }
                | SearchItem::Artist { .. }
                | SearchItem::Playlist { .. } => Some(item.clone()),
                SearchItem::Song { album, client, .. } => {
                    album_page(&ctx.actx.app.corpus, album, client)
                }
                SearchItem::LocalMusic { .. } | SearchItem::LocalVideo { .. } => None,
            };
            match page {
                Some(page) => {
                    let name = label(&page);
                    let dispatch = dispatch_action(Action::Play(page), ctx.actx);
                    (dispatch, Report::Note(format!("opened {name}")))
                }
                None => op_fail(format!("no page for '{}'", item.primary_name())),
            }
        }
        Op::ClearQueue => {
            // mpv's own `playlist-clear` keeps the file that is
            // playing and drops the rest, which is exactly what
            // clearing the queue means; `entries` only mirrors it.
            match ctx.actx.player.playlist_clear() {
                Ok(()) => {
                    ctx.actx.queue.replace_upcoming(Vec::new());
                    *ctx.queue_selected = 0;
                    op_done()
                }
                Err(e) => op_fail(format!("error clearing the queue: {e}")),
            }
        }
        Op::RemoveFromQueue(positions) => {
            // 1-based and descending, so removing one can't reindex
            // the next; the same `remove` + `playlist_remove` pairing
            // the queue view's 'd' uses.
            let mut removed = 0;
            for position in &positions {
                let index = position.saturating_sub(1);
                if ctx.actx.queue.remove(index) {
                    if let Err(e) = ctx.actx.player.playlist_remove(index) {
                        return op_fail(format!("error removing: {e}"));
                    }
                    removed += 1;
                }
            }
            let left = ctx.actx.queue.entries().len();
            *ctx.queue_selected = (*ctx.queue_selected).min(left.saturating_sub(1));
            op_note(format!("removed {removed} from the queue"))
        }
        Op::GotoQueue(position) => {
            // a `playlist-pos` write, not a reload: mpv keeps the same
            // playlist so `entries` stays index-aligned with it (same
            // as enter in the queue view).
            op_ipc(ctx.actx.player.set_property(
                "playlist-pos",
                serde_json::json!(position.saturating_sub(1)),
            ))
        }
        Op::ShuffleQueue => {
            let shuffled = ctx.actx.queue.shuffled_upcoming();
            let count = shuffled.len();
            let status = resync_upcoming(ctx.actx.player, ctx.actx.cache, ctx.actx.queue, shuffled);
            if status.starts_with("error") {
                return op_fail(status);
            }
            op_note(format!("shuffled {count} upcoming"))
        }
        Op::SaveQueue(name) => match queues::save(&name, ctx.actx.queue.entries()) {
            Ok(0) => op_fail("nothing in the queue to save"),
            Ok(count) => {
                *ctx.saved_queues = queues::list();
                op_note(format!("saved {count} tracks as '{}'", queues::slug(&name)))
            }
            Err(e) => op_fail(format!("couldn't save '{name}': {e}")),
        },
        Op::LoadQueue(name) => {
            let clients = shellsong_host::clients_of(&ctx.actx.app.corpus);
            match queues::load(&name, &clients) {
                None => op_fail(format!("no saved queue called '{name}'")),
                Some(items) if items.is_empty() => {
                    op_fail(format!("nothing in '{name}' could be found any more"))
                }
                Some(items) => {
                    let count = items.len();
                    match start_session(items, false, ctx.actx) {
                        Ok(_) => op_note(format!("loaded '{name}': {count} tracks")),
                        Err(why) => op_fail(why),
                    }
                }
            }
        }
        Op::View(target) => open_shellsong_view(target, ctx),
        Op::Rescan => {
            // a fresh channel and thread, same as startup: the scan
            // signs off by dropping its sender, so the old receiver
            // can't be reused. Only the local half of the corpus is
            // dropped, or a rescan would also wipe the navidrome
            // catalog until the next sync landed.
            let (tx, rx) = mpsc::channel::<library::ScanBatch>();
            library::spawn_scan(ctx.actx.cfg.library_paths.clone(), tx);
            *ctx.scan_rx = rx;
            ctx.actx.app.corpus.drop_local();
            ctx.actx.app.scanning_library = true;
            ctx.actx.app.resync_results();
            op_done()
        }
        Op::Sleep(Some(secs)) => {
            *ctx.sleep_due = Some(ctx.now + Duration::from_secs_f64(secs.max(0.0)));
            op_done()
        }
        Op::Sleep(None) => {
            let was_set = ctx.sleep_due.take().is_some();
            if !was_set {
                return op_note("no sleep timer was set");
            }
            // the ramp may already have taken the volume down.
            let _ = ctx.actx.player.set_volume(*ctx.base_volume);
            op_done()
        }
        Op::Fade { into, secs } => {
            let from = current_volume(ctx.actx.player).unwrap_or(*ctx.base_volume);
            let to = if into { *ctx.base_volume } else { 0.0 };
            *ctx.fade = Some(Ramp::new(
                from,
                to,
                ctx.now,
                Duration::from_secs_f64(secs.max(0.0)),
            ));
            op_done()
        }
        Op::Quit => {
            let dispatch = dispatch_action(Action::Quit, ctx.actx);
            (dispatch, Report::Described)
        }
    }
}

/// Whether a play that just happened should kick off a background
/// download. Pure, and kept apart from the clock and the cache file so
/// the policy itself is testable: `days_played` is the number of
/// *distinct calendar days* the track has been played on, never the
/// raw play count, which is what made pressing enter twice in one
/// sitting download a track nobody asked to cache.
fn should_auto_download(
    days_played: u32,
    day_threshold: u32,
    already_cached: bool,
    in_flight: bool,
) -> bool {
    days_played >= day_threshold.max(1) && !already_cached && !in_flight
}

/// Called every time a remote song actually starts playing (every
/// track-change call site threads this through): records the play
/// against today's date and, once the track has been played on
/// `dctx.day_threshold` distinct days, kicks off a background download
/// so it's cached for next time (see `cache.rs`/`download.rs`). A
/// no-op for local items (never cached at all) and for anything
/// already cached or already mid-download.
///
/// `today` is read off the clock here and nowhere deeper; everything
/// that actually decides anything takes the day as an argument.
///
/// This is also the one call site a future scrobble submission
/// (ListenBrainz, separate work happening elsewhere) would hook into:
/// by the time this runs, the track has definitely started playing.
fn note_playback_started(
    item: &SearchItem,
    dctx: &mut DownloadCtx,
    notifications: &mut Notifications,
) {
    let SearchItem::Song {
        id,
        title,
        artist,
        client,
        ..
    } = item
    else {
        return;
    };
    let today = cache::today();
    let (days, already_cached) = match dctx.cache.lock() {
        Ok(mut idx) => {
            let days = idx.record_play(id, &today);
            let _ = idx.save();
            (days, idx.cached_path(id).is_some())
        }
        Err(_) => return,
    };
    if !should_auto_download(
        days,
        dctx.day_threshold,
        already_cached,
        dctx.downloading.contains(id),
    ) {
        return;
    }
    dctx.downloading.insert(id.clone());
    let track_label = format!("{title} · {artist}");
    notifications.push_progress(id.clone(), track_label.clone());
    download::spawn_tracks(
        vec![(id.clone(), track_label, client.stream_url(id))],
        dctx.cache.clone(),
        dctx.download_tx.clone(),
    );
}

/// Backs the manual download keybind (`KeyAction::DownloadSelected`):
/// expands `item` the same way playing it would (`resolve_tracks`, so
/// an album/playlist/artist downloads every song in it), downloads
/// every remote song in the result concurrently (one thread each, see
/// `download::spawn_tracks`), and skips anything already cached,
/// already downloading, or not actually a remote track (local files
/// are already local, nothing to cache). Deliberately ignores
/// `day_threshold`: asking for a download outright is explicit, and
/// has no business waiting on a listening-habit heuristic.
fn trigger_downloads(
    item: &SearchItem,
    dctx: &mut DownloadCtx,
    notifications: &mut Notifications,
) -> String {
    let tracks = match resolve_tracks(item) {
        Ok(t) => t,
        Err(msg) => return msg,
    };
    let mut batch = Vec::new();
    let mut already_cached = 0;
    let mut in_flight = 0;
    for t in &tracks {
        let SearchItem::Song {
            id,
            title,
            artist,
            client,
            ..
        } = t
        else {
            continue;
        };
        let cached = dctx
            .cache
            .lock()
            .ok()
            .is_some_and(|idx| idx.cached_path(id).is_some());
        if cached {
            already_cached += 1;
            continue;
        }
        if dctx.downloading.contains(id) {
            in_flight += 1;
            continue;
        }
        dctx.downloading.insert(id.clone());
        let track_label = format!("{title} · {artist}");
        notifications.push_progress(id.clone(), track_label.clone());
        batch.push((id.clone(), track_label, client.stream_url(id)));
    }
    let queued = batch.len();
    if queued > 0 {
        download::spawn_tracks(batch, dctx.cache.clone(), dctx.download_tx.clone());
    }
    if queued > 0 {
        format!("downloading {queued} track(s)")
    } else if already_cached > 0 {
        "already cached".to_string()
    } else if in_flight > 0 {
        "already downloading".to_string()
    } else {
        "nothing to download (local files are already local)".to_string()
    }
}

/// Current output level, 0..1, for the now-playing level meter.
/// Best-effort: reads the `astats` tap `build_af_filter` (eq.rs) adds
/// to the end of the af chain, via mpv's `af-metadata/<label>/...`
/// property. If that property path doesn't resolve the way expected
/// (wrong mpv version, filter not actually in the graph yet, af-
/// metadata represented differently than guessed), this falls back to
/// a calm idle breathe instead of silently showing a dead meter,
/// driven off `elapsed` (time since app start) like every other
/// animation.
fn audio_level(player: &mpv::Mpv, elapsed: Duration) -> f32 {
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
    (anim::pulse(elapsed, anim::LEVEL_FALLBACK_PERIOD) * 0.6 + 0.05).clamp(0.0, 1.0)
}

/// How often the background thread (see `spawn_background_sync`)
/// re-fetches every configured server, so navidrome stays "constantly
/// updating" without the user ever pressing a manual refresh key.
const AUTO_SYNC_INTERVAL: Duration = Duration::from_secs(45);

/// How often the main loop re-sweeps the download cache for stale
/// (unplayed past `cfg.cache_eviction_days`) files, on top of the
/// sweep that always runs once at startup; cheap local filesystem
/// work, not worth its own background thread the way network sync is.
const CACHE_SWEEP_INTERVAL: Duration = Duration::from_secs(3600);

/// Deletes every cached file past `cfg.cache_eviction_days` unplayed;
/// best-effort, same as every other cache operation (a failed save
/// just means the eviction is re-attempted next sweep).
fn sweep_cache(cache: &Arc<Mutex<CacheIndex>>, eviction_days: u64) {
    if let Ok(mut idx) = cache.lock() {
        if idx.sweep_stale(eviction_days.saturating_mul(24 * 3600)) > 0 {
            let _ = idx.save();
        }
    }
}

/// How often the embedded video pane pulls a fresh frame from mpv;
/// each pull is an IPC round-trip plus a base64 decode, not free, and
/// the ascii/halfblock rendering it feeds was never going to look
/// smooth at real video frame rates anyway.
const VIDEO_FRAME_INTERVAL: Duration = Duration::from_millis(150);

fn now_playing_is_video(now_playing: &Option<SearchItem>) -> bool {
    matches!(now_playing, Some(SearchItem::LocalVideo { .. }))
}

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

/// The one starred set `suggest::Request` wants: remote song ids and
/// local favorite paths together, matching `suggest::Track::key`.
fn suggestion_starred_keys(
    remote_starred: &HashSet<String>,
    favorites: &favorites::Favorites,
) -> HashSet<String> {
    remote_starred
        .iter()
        .cloned()
        .chain(favorites.paths().cloned())
        .collect()
}

/// Persists config, surfacing a failure instead of swallowing it: a
/// read-only config dir or a full disk used to mean every setting
/// silently reverted on the next launch with nothing said about it.
fn save_config(cfg: &config::Config, notifications: &mut Notifications) {
    if let Err(e) = config::save(cfg) {
        notifications.push(Severity::Error, format!("couldn't save settings: {e}"));
    }
}

/// Enter on a preset row of the eq menu applies it, same effect as
/// pressing the bare `CycleEq` key would have for that one preset; a
/// no-op when `row` is a band row instead, since adjusting those is
/// `adjust_eq_band`'s job. Deliberately not wired to j/k: this
/// overwrites all ten gains, which is not something plain navigation
/// should ever do.
fn apply_eq_preset_row(
    app: &mut app::App,
    player: &mpv::Mpv,
    notifications: &mut Notifications,
    row: usize,
) {
    if row >= eq::PRESETS.len() {
        return;
    }
    app.eq_preset = row;
    app.eq_custom = false;
    app.eq_gains = eq::PRESETS[row].gains;
    apply_eq_gains(app, player, notifications);
}

/// Pushes `app.eq_gains` to mpv, reporting a failure: the menu paths
/// used to discard the error while the global cycle-eq key reported
/// it, so the same failure was visible from one route and silent from
/// the other.
fn apply_eq_gains(app: &app::App, player: &mpv::Mpv, notifications: &mut Notifications) {
    if let Err(e) = player.set_af(&eq::build_af_filter(&app.eq_gains)) {
        notifications.push(Severity::Error, format!("error setting eq: {e}"));
    }
}

/// One h/l nudge (`dir`: -1.0 or 1.0) on band `band`, 1 dB per step,
/// clamped to a sane range; touching any band diverges from whatever
/// named preset was active into a "custom" setting.
fn adjust_eq_band(
    app: &mut app::App,
    player: &mpv::Mpv,
    notifications: &mut Notifications,
    band: usize,
    dir: f64,
) {
    app.eq_gains[band] = (app.eq_gains[band] + dir).clamp(-12.0, 12.0);
    app.eq_custom = true;
    apply_eq_gains(app, player, notifications);
}

/// A completed background sync attempt for one server, tagged by url
/// since several servers' results arrive interleaved on the same
/// channel with no other way to tell them apart. Carries the starred
/// song ids too: they used to be fetched synchronously before the
/// first frame, and they ride the connection this thread is already
/// making rather than needing one of their own.
struct BackgroundSyncResult {
    url: String,
    result: NavidromeEverything,
}

/// How long to let rapid keystrokes settle before actually firing a
/// `search3` request, so a burst of typing costs one network round-
/// trip instead of one per character; see `spawn_background_search`.
const REMOTE_SEARCH_DEBOUNCE: Duration = Duration::from_millis(300);

/// One query-changed event, carrying the servers to search against at
/// the time it was sent; `Corpus::remote_clients` (already-indexed
/// servers only) is cheap to call per-keystroke on the main thread, so
/// the snapshot travels with the query rather than the background
/// thread needing its own separate copy of the server list to keep in
/// sync.
struct RemoteSearchQuery {
    query: String,
    clients: Vec<Arc<subsonic::Client>>,
}

/// One server's `search3` result for whatever query it was fired for;
/// not tagged with the query itself; see `spawn_background_search`'s
/// doc for why that's fine.
struct RemoteSearchOutcome {
    client: Arc<subsonic::Client>,
    result: subsonic::SearchResult,
}

/// Runs forever on its own thread, debouncing query-changed events
/// from the main thread and firing Subsonic's `search3` against every
/// already-indexed navidrome server once typing settles. This is the
/// live counterpart to `fetch_navidrome_catalog`'s eager indexing:
/// song titles are deliberately never indexed up front (a library's
/// songs could be enormous; see that function's own comment), so
/// without this, searching a song by title only ever worked by
/// coincidence. Same shape as `spawn_background_sync`: pure network
/// I/O on its own thread, result handed back over a channel, main
/// thread owns all the `!Send` state; just triggered by query changes
/// instead of a fixed timer.
///
/// Results aren't tagged with the query they answer and are merged
/// into the corpus unconditionally rather than discarded if the query
/// has since changed: `Corpus::add_subsonic_search_result` already
/// dedups against what's indexed, so a stale answer is harmless to
/// merge in (it simply enriches the corpus for whatever's searched
/// next) and the subsequent `resync_results()` re-filters against
/// whatever the *current* query actually is, so a stale result can
/// never flash on screen for the wrong query.
fn spawn_background_search(
    query_rx: mpsc::Receiver<RemoteSearchQuery>,
    result_tx: mpsc::Sender<RemoteSearchOutcome>,
) {
    thread::spawn(move || loop {
        let mut pending = match query_rx.recv() {
            Ok(q) => q,
            Err(_) => return, // main thread gone (shutting down)
        };
        // debounce: keep absorbing newer queries until typing settles
        // for a full window, so a burst of keystrokes fires exactly
        // one request rather than one per character.
        loop {
            match query_rx.recv_timeout(REMOTE_SEARCH_DEBOUNCE) {
                Ok(newer) => pending = newer,
                Err(RecvTimeoutError::Timeout) => break,
                Err(RecvTimeoutError::Disconnected) => return,
            }
        }
        if pending.query.trim().is_empty() || pending.clients.is_empty() {
            continue;
        }
        // independent per-server requests, same reasoning as every
        // other multi-server fetch in this file: run them
        // concurrently rather than making every server wait its turn.
        let outcomes: Vec<RemoteSearchOutcome> = thread::scope(|scope| {
            pending
                .clients
                .iter()
                .map(|client| {
                    let query = pending.query.clone();
                    let client = client.clone();
                    scope.spawn(move || (client.clone(), client.search3(&query)))
                })
                .collect::<Vec<_>>()
                .into_iter()
                .filter_map(|handle| {
                    let (client, result) = handle.join().ok()?;
                    Some(RemoteSearchOutcome {
                        client,
                        result: result.ok()?,
                    })
                })
                .collect()
        });
        for outcome in outcomes {
            if result_tx.send(outcome).is_err() {
                return; // main thread is gone (shutting down)
            }
        }
    });
}

/// Runs forever on its own thread, re-fetching every server's catalog
/// on a timer and reporting each attempt back over `result_tx`; the
/// main thread owns `App`/mpv/the terminal, none of which are `Send`
/// across this boundary, so this only ever does pure network I/O and
/// hands the result back rather than touching app state directly.
/// `servers_rx` lets the main thread push an updated server list (add/
/// remove a profile) without restarting the thread. This is also the
/// initial connect: it fetches first and waits afterwards, so a
/// configured server is reached as fast as the network allows without
/// anything on the main thread blocking on it.
fn spawn_background_sync(
    initial_servers: Vec<config::NavidromeConfig>,
    result_tx: mpsc::Sender<BackgroundSyncResult>,
    servers_rx: mpsc::Receiver<Vec<config::NavidromeConfig>>,
) {
    thread::spawn(move || {
        let mut servers = initial_servers;
        let mut first_pass = true;
        loop {
            // independent per-server requests, so run them
            // concurrently rather than making every server wait for
            // the one before it.
            let results: Vec<BackgroundSyncResult> = thread::scope(|scope| {
                servers
                    .iter()
                    .map(|nav| {
                        (
                            nav.url.clone(),
                            scope.spawn(|| fetch_navidrome_everything(nav)),
                        )
                    })
                    .collect::<Vec<_>>()
                    .into_iter()
                    .map(|(url, handle)| BackgroundSyncResult {
                        url,
                        result: handle
                            .join()
                            .unwrap_or_else(|_| Err("panicked while syncing".to_string())),
                    })
                    .collect()
            });
            if first_pass && !servers.is_empty() {
                trace::mark("navidrome: first sync landed");
                first_pass = false;
            }
            for result in results {
                if result_tx.send(result).is_err() {
                    return; // main thread is gone (shutting down)
                }
            }
            match servers_rx.recv_timeout(AUTO_SYNC_INTERVAL) {
                Ok(updated) => servers = updated,
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => return,
            }
        }
    });
}

/// Pushes the current query to the background `search3` thread (see
/// `spawn_background_search`) whenever typing/pasting actually changed
/// it; a no-op while the query's empty or no server has been indexed
/// yet, same as the background thread's own guard, just avoiding the
/// channel send entirely in the common "nothing to search remotely"
/// case.
///
/// Reports whether a request actually went out, which is exactly what
/// `SearchSettle` needs: with nothing fired there is nothing to wait
/// for, so the results for the new query are already as complete as
/// they will get.
fn notify_query_changed(
    app: &app::App,
    remote_search_tx: &mpsc::Sender<RemoteSearchQuery>,
) -> bool {
    if app.query.trim().is_empty() {
        return false;
    }
    let clients = app.corpus.remote_clients();
    if clients.is_empty() {
        return false;
    }
    remote_search_tx
        .send(RemoteSearchQuery {
            query: app.query.clone(),
            clients,
        })
        .is_ok()
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
    // for search to work on artists/albums. these three are
    // independent requests; fetching them concurrently instead of one
    // after another is most of the difference between this feeling
    // instant and feeling like three round-trips stacked up.
    let (artists, albums, playlists) = thread::scope(|scope| {
        let artists = scope.spawn(|| client.all_artists().unwrap_or_default());
        let albums = scope.spawn(|| client.all_albums().unwrap_or_default());
        let playlists = scope.spawn(|| client.playlists().unwrap_or_default());
        (
            artists.join().unwrap_or_default(),
            albums.join().unwrap_or_default(),
            playlists.join().unwrap_or_default(),
        )
    });
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

type NavidromeEverything = Result<(FetchedCatalog, Vec<String>), String>;

/// Catalog + starred-song-ids for one server, fetched together so
/// the startup multi-server pass (see `main`) only connects once per
/// server rather than once for the catalog and again for starred
/// songs. Pure network I/O, safe to call from any thread.
fn fetch_navidrome_everything(nav: &config::NavidromeConfig) -> NavidromeEverything {
    let catalog = fetch_navidrome_catalog(nav)?;
    let starred = catalog
        .client
        .get_starred_songs()
        .map(|songs| songs.into_iter().map(|s| s.id).collect())
        .unwrap_or_default();
    Ok((catalog, starred))
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
    thread::scope(|scope| {
        servers
            .iter()
            .map(|nav| scope.spawn(|| subsonic::Client::new(nav).and_then(|c| c.ping()).is_ok()))
            .collect::<Vec<_>>()
            .into_iter()
            .map(|handle| Some(handle.join().unwrap_or(false)))
            .collect()
    })
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

/// Which pane plain navigation (j/k/arrows/enter) currently targets.
/// See where `focus` is declared in `main` for the full reasoning.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Focus {
    Sidebar,
    Workspace,
    BottomBar,
}

/// shift+<direction>'s sway/i3-style transition: which neighboring
/// pane (if any) lies in that direction from `current`. Every
/// direction/state combination not listed here has nowhere further
/// to go that way and is deliberately a no-op (returns `current`
/// unchanged), not an arbitrary fallback to some other pane.
/// Whether the keyboard is currently moving focus between panes, or
/// acting on whatever pane already has it. Two explicit modes rather
/// than one modifier-key shortcut: a chord that silently moves focus
/// gives you no way to tell, before pressing anything else, which
/// pane the next keypress is going to land in. In `Move` nothing
/// except focus responds at all (no scrolling, no playback, no
/// keybinds), and both the bottom bar's indicator and the focused
/// pane's own borders say so the whole time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InputMode {
    Move,
    Interact,
}

/// wasd and the arrow keys both mean the same directional move;
/// normalizes either into the arrow key `focus_in_direction` matches
/// on, or `None` for a key that isn't a direction at all. wasd
/// because that's what sway is configured with for exactly this job
/// (focus the neighboring window), arrows because they're the obvious
/// guess if you've never read the keybind list. Only consulted in
/// `InputMode::Move`, so these stay free for their normal meaning
/// everywhere else.
fn focus_direction_key(code: KeyCode) -> Option<KeyCode> {
    match code {
        KeyCode::Left | KeyCode::Right | KeyCode::Up | KeyCode::Down => Some(code),
        // shift+w arrives as an uppercase char, but match both cases
        // rather than trusting every terminal to agree on that.
        KeyCode::Char(c) => match c.to_ascii_lowercase() {
            'a' => Some(KeyCode::Left),
            'd' => Some(KeyCode::Right),
            'w' => Some(KeyCode::Up),
            's' => Some(KeyCode::Down),
            _ => None,
        },
        _ => None,
    }
}

fn focus_in_direction(current: Focus, direction: KeyCode) -> Focus {
    match (current, direction) {
        (Focus::Sidebar, KeyCode::Right) => Focus::Workspace,
        (Focus::Workspace, KeyCode::Left) => Focus::Sidebar,
        (Focus::Sidebar | Focus::Workspace, KeyCode::Down) => Focus::BottomBar,
        (Focus::BottomBar, KeyCode::Up) => Focus::Sidebar,
        _ => current,
    }
}

/// "flat"/a preset name, or "custom" once any band's been hand-tuned
/// away from whatever preset was last applied; shared by the player
/// view and the bottom bar so neither keeps crediting a preset that
/// no longer matches `app.eq_gains`.
fn eq_label(app: &app::App) -> &'static str {
    if app.eq_custom {
        "custom"
    } else {
        eq::PRESETS[app.eq_preset].name
    }
}

fn main() -> anyhow::Result<()> {
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
    let mut app = app::App::new(search::Corpus::new());
    // so the first frame says "scanning" rather than "no local music
    // found"; cleared when the scan thread drops its sender. the
    // server equivalent is set below.
    app.scanning_library = true;
    // restores whichever eq state was last active (a named preset, or
    // a hand-tuned custom set) by name rather than a raw index, same
    // reasoning as `logo_style`; applied to mpv immediately so a
    // persisted eq (and the level meter's `astats` tap, see
    // `audio_level`) is live from the first frame, not only after the
    // first manual 'e' press.
    match cfg.eq_preset.as_deref().and_then(eq::index_of) {
        Some(idx) => {
            app.eq_preset = idx;
            app.eq_custom = false;
            app.eq_gains = eq::PRESETS[idx].gains;
        }
        None => {
            app.eq_custom = true;
            app.eq_gains = cfg.eq_gains;
        }
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
    // whatever was playing right before `now_playing` last changed;
    // purely for the player view's "previous: ..." line.
    let mut previous_track: Option<SearchItem> = None;
    let mut scrobble = ScrobbleState::default();
    // false (full player) by default for a standalone play; starting
    // playback from an open album/playlist sets this true instead, so
    // the track listing stays visible rather than getting replaced.
    // 'm' toggles it by hand either way.
    let mut player_minimized = false;
    // 'i' cycles ascii -> real -> off from here; this is just the
    // config-driven starting point, with the graphics probe still out
    // (so never Real yet, see `art_mode_for_picker`). kept around so
    // the probe landing can only upgrade a mode the user hasn't since
    // cycled by hand.
    let art_mode_pending = art_mode_for_picker(cfg.album_art, false);
    let mut art_mode = art_mode_pending;
    // latest captured frame while playing a local video, rendered
    // through the same ascii pipeline as album art instead of mpv
    // opening its own window; `None` whenever nothing video is
    // playing or a capture hasn't landed yet. 'v' expands its pane
    // from main_content to the whole workspace (sidebar included),
    // never the bottom bar.
    let mut video_frame: Option<image::DynamicImage> = None;
    let mut video_fullscreen = false;
    // throttles screenshot-raw calls (an IPC round-trip each time) to
    // a modest rate rather than once per render tick.
    let mut last_video_frame_at = Instant::now() - VIDEO_FRAME_INTERVAL;
    // full keybind reference, opened with '?'; takes over the whole
    // screen the same way onboarding/the nav manager do.
    let mut help_open = false;
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
    // backs `Env::search_settled`; see `shellsong_host::SearchSettle`.
    let mut settle = SearchSettle::default();
    // the queue view owns main_content like the browse/lyrics views do;
    // its cursor is separate from playback, so scrolling it never moves
    // what mpv is playing.
    let mut queue_view_open = false;
    let mut queue_selected: usize = 0;
    // computed once when the menu opens rather than per frame: the
    // engine walks the whole corpus and the play history, which is
    // not something to redo sixty times a second for a static list.
    let mut suggestions: Option<suggest::Suggestions> = None;
    let mut suggestions_selected: usize = 0;
    let mut lyrics_lines: Option<Vec<String>> = None;
    let mut lyrics_scroll: usize = 0;
    // which track the open lyrics belong to; shown in their header so
    // a pane left open across a track change isn't ambiguous.
    let mut lyrics_track = String::new();
    // the workspace width the lyrics were last drawn at; wrapping (and
    // therefore how far they can scroll) depends on it, and only the
    // draw pass knows it.
    let mut lyrics_width: u16 = 0;
    // opened via enter on an album/playlist; takes over main_content
    // until esc, same modal-ish precedence as lyrics but independent
    // of it (browsing doesn't require anything to already be playing).
    let mut browse_view: Option<browse::BrowseView> = None;

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
    let mut eq_menu_selected: usize = 0;

    let mut panels = PanelLayout::from_sizes(cfg.sidebar_width, cfg.bottom_bar_height);
    let start = Instant::now();
    let mut guard = term::TerminalGuard::new()?;
    // queries the terminal over stdio for font-cell size and graphics
    // protocol support; best-effort, 'i' (real image mode) just stays
    // unavailable if this fails rather than erroring the whole app.
    // on its own thread because the terminal may simply never answer,
    // in which case ratatui-image waits out a full second: the single
    // largest thing the first frame used to sit behind. spawned after
    // raw mode is already on, since the probe saves and restores
    // whatever termios state it finds.
    let (picker_tx, picker_rx) = mpsc::channel::<Option<Picker>>();
    thread::spawn(move || {
        let _ = picker_tx.send(Picker::from_query_stdio().ok());
        trace::mark("graphics probe answered");
    });
    let mut picker: Option<Picker> = None;
    let mut picker_probed = false;
    trace::mark("terminal ready");
    // rebuilt (not re-encoded every frame) whenever a newly-opened
    // album's art changes; `ratatui-image` owns the actual resize/
    // encode caching per protocol instance.
    let mut real_art_protocol: Option<StatefulProtocol> = None;
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
        let (elapsed_secs, duration_secs, paused) = (
            playback.elapsed_secs,
            playback.duration_secs,
            playback.paused,
        );
        // re-derived every frame and never kept across one: whatever
        // mpv's last reported `playlist-pos` resolves to. Everything
        // downstream (title, mini bar, lyrics header, scrobbling)
        // reads this, so none of them can show a track mpv isn't
        // actually playing.
        let mut now_playing = play_queue.current().cloned();
        if let Ok(detected) = frame_rx.try_recv() {
            frame = detected;
        }
        // sampled once per frame rather than per widget, so everything
        // on screen animates off the same instant.
        let frame_elapsed = start.elapsed();
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
                f.render_widget(ui::help::HelpOverlay { keymap: &keymap }, area);
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

            if show_full_player && now_playing_is_video(&now_playing) {
                // the video pane itself: rendered through the same
                // ascii/halfblock pipeline as album art rather than
                // leaving it to mpv's own window (see
                // `mpv::screenshot_raw`/`VIDEO_FRAME_INTERVAL` above).
                // no level meter here; it'd just be noise over a
                // picture that's already moving.
                f.render_widget(
                    AsciiArt {
                        image: video_frame.as_ref(),
                        cell_aspect: aspect,
                    },
                    main_content,
                );
            } else if show_full_player {
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
                let playing_art = browse_view
                    .as_ref()
                    .filter(|bv| {
                        let id = item.identity();
                        bv.tracks.iter().any(|t| t.identity() == id)
                    })
                    .and_then(|bv| bv.art.as_ref());
                let has_context = previous_label.is_some() || next_label.is_some();
                f.render_widget(
                    PlayerView {
                        title: &label(item),
                        art: playing_art,
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
                        eq_preset: eq_label(&app),
                        crossfade_on: crossfade_enabled,
                        previous: previous_label.as_deref(),
                        next: next_label.as_deref(),
                        selected: (focus == Focus::Workspace).then_some(player_selected),
                        scrubbing: scrub.is_some(),
                        keymap: &keymap,
                    },
                    main_content,
                );
                if art_mode == browse::ArtMode::Real {
                    // same contract as the browse view: the widget drew
                    // no ascii art, the real image goes on the exact
                    // rect its own layout picked.
                    if let (Some(protocol), Some(art_rect)) = (
                        &mut real_art_protocol,
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

                level_history.push(audio_level(&player, frame_elapsed));
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
                        cell_aspect: aspect,
                        cursor_visible,
                        focused: focus == Focus::Workspace,
                        elapsed: frame_elapsed,
                    },
                    main_content,
                );
                if art_mode == browse::ArtMode::Real {
                    if let Some(protocol) = &mut real_art_protocol {
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
                f.render_widget(
                    ui::suggestions::SuggestionsView {
                        suggestions: sug,
                        selected: suggestions_selected,
                        focused: focus == Focus::Workspace,
                        elapsed: frame_elapsed,
                        // the network-backed generators are a later
                        // slice; nothing is in flight yet.
                        enriching: false,
                    },
                    main_content,
                );
            } else if queue_view_open {
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
            } else if let Some(lines) = &lyrics_lines {
                lyrics_width = main_content.width;
                let borrowed: Vec<&str> = lines.iter().map(String::as_str).collect();
                f.render_widget(
                    LyricsView {
                        lines: &borrowed,
                        scroll: lyrics_scroll,
                        track: &lyrics_track,
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

            // 'v': expands the video pane from main_content to cover
            // the whole workspace (sidebar included), painted last so
            // it simply overwrites whatever the sidebar/divider/
            // results list already drew in `top` rather than needing
            // to skip rendering them in the first place. never covers
            // `bottom`, which sits outside `top` entirely.
            if video_fullscreen && show_full_player && now_playing_is_video(&now_playing) {
                f.render_widget(
                    AsciiArt {
                        image: video_frame.as_ref(),
                        cell_aspect: aspect,
                    },
                    top,
                );
            }

            // 'u': settings, over the whole workspace (sidebar
            // included) same as the video pane's fullscreen mode,
            // never the bottom bar; painted last for the same reason.
            if settings_menu_open {
                let art_mode_label = match art_mode {
                    browse::ArtMode::Ascii => "ascii",
                    browse::ArtMode::Real => "real",
                    browse::ArtMode::Off => "off",
                };
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
                        custom: app.eq_custom,
                        active_preset: app.eq_preset,
                        selected: eq_menu_selected,
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
            f.render_widget(
                ControlsBar {
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
                    // the preset's name lives in the eq menu now; the
                    // bar only answers "is the eq doing anything".
                    eq_active: eq_label(&app) != "flat",
                    crossfade_on: crossfade_enabled,
                    paused,
                    // mid-drag the bar draws the pending target, not
                    // mpv's position: the seek is throttled, the
                    // keypress shouldn't look like it was.
                    elapsed_secs: scrub.map(|s| s.target_secs).or(elapsed_secs),
                    duration_secs,
                    flash: flash_level,
                    selected: (focus == Focus::BottomBar).then_some(bottombar_selected),
                    scrubbing: scrub.is_some(),
                    resize_hint,
                },
                Rect {
                    y: bottom.y + 1,
                    width: bottom.width.saturating_sub(help_box_width),
                    height: bottom.height.saturating_sub(1),
                    ..bottom
                },
            );
            f.render_widget(
                ui::helpbox::HelpBox { label: &help_label },
                Rect {
                    x: help_box_x,
                    y: bottom.y + 1,
                    width: help_box_width,
                    height: bottom.height.saturating_sub(1),
                },
            );
            f.render_widget(
                NotificationOverlay {
                    items: notifications.visible(),
                },
                area,
            );

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
            set_vid_for_item(&player, &next);
            fire_subtitle_fetch(&cfg, &subtitle_tx, &next);
            let mut dctx = DownloadCtx {
                cache: &cache_index,
                download_tx: &download_tx,
                downloading: &mut downloading,
                day_threshold: cfg.cache_days_threshold,
            };
            note_playback_started(&next, &mut dctx, &mut notifications);
            start_tracking_playback(&cfg, &next, &mut scrobble);
            previous_track = now_playing.replace(next);
            // a track just started: either start it quiet and ramp up
            // (crossfade on), or make sure it's at full volume
            // regardless of wherever a fade-out left things (crossfade
            // just got disabled mid-fade, say).
            if crossfade_enabled {
                let _ = player.set_volume(0.0);
                fade_in_started_at = Some(Instant::now());
            } else {
                let _ = player.set_volume(base_volume);
            }
        }

        // mpv went back to idling, so its playlist really did run out
        // (unlike end-of-file, which also fires for a track that just
        // got replaced). The upcoming-empty check guards against
        // acting on a stale idle report from before something loaded.
        if playlist_drained && play_queue.upcoming_len() == 0 {
            // mpv has no concept of "the session we built", only its
            // own now-empty playlist; "loop all" replaying from the top
            // is purely our own bookkeeping (`PlayQueue::session`).
            let session = play_queue.session().to_vec();
            if app.loop_mode == LoopMode::All && !session.is_empty() {
                let first = session[0].clone();
                let (s, played) = load_playlist(
                    &player,
                    &cache_index,
                    &mut play_queue,
                    session,
                    SessionStart::Keep,
                );
                if !played {
                    notifications.push(Severity::Error, s);
                } else {
                    fire_subtitle_fetch(&cfg, &subtitle_tx, &first);
                    let mut dctx = DownloadCtx {
                        cache: &cache_index,
                        download_tx: &download_tx,
                        downloading: &mut downloading,
                        day_threshold: cfg.cache_days_threshold,
                    };
                    note_playback_started(&first, &mut dctx, &mut notifications);
                    start_tracking_playback(&cfg, &first, &mut scrobble);
                    previous_track = now_playing.replace(first);
                    if crossfade_enabled {
                        let _ = player.set_volume(0.0);
                        fade_in_started_at = Some(Instant::now());
                    } else {
                        let _ = player.set_volume(base_volume);
                    }
                }
            } else {
                play_queue.clear();
                now_playing = None;
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
                if let (Some(elapsed), Some(duration)) =
                    (playback.elapsed_secs, playback.duration_secs)
                {
                    let remaining = duration - elapsed;
                    if remaining <= cfg.crossfade_seconds {
                        let t = (remaining / cfg.crossfade_seconds).clamp(0.0, 1.0);
                        let _ = player.set_volume(base_volume * t);
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

        // pulling a frame regardless of whether the video pane is
        // actually on screen right now would be wasted IPC/decode
        // work the moment it's covered by the browse view or help
        // overlay; only bother while it'd actually be visible.
        if now_playing_is_video(&now_playing) && !help_open {
            if last_video_frame_at.elapsed() >= VIDEO_FRAME_INTERVAL {
                if let Ok(frame) = player.screenshot_raw() {
                    video_frame = frame.into_image().map(image::DynamicImage::ImageRgb8);
                }
                last_video_frame_at = Instant::now();
            }
        } else {
            video_frame = None;
        }

        // the terminal's answer to the graphics probe, or `None` if it
        // never gave one. a config that named a protocol only gets
        // real art from here on; until (or unless) this lands it stays
        // ascii, and a terminal that stayed silent is said so out loud
        // rather than leaving an art pane that never fills.
        if !picker_probed {
            match picker_rx.try_recv() {
                Ok(probed) => {
                    picker_probed = true;
                    picker = probed;
                    if art_mode == art_mode_pending {
                        art_mode = art_mode_for_picker(cfg.album_art, picker.is_some());
                    }
                    if picker.is_none() && wants_real_art(cfg.album_art) {
                        notifications.push(
                            Severity::Alert,
                            "album art: terminal reported no graphics protocol, using ascii",
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

        // drain whatever the live remote-search thread answered since
        // the last frame; merged unconditionally and re-filtered
        // against whatever's actually typed right now (see
        // `spawn_background_search`'s doc for why that's safe even if
        // the query's since moved on).
        while let Ok(outcome) = remote_search_result_rx.try_recv() {
            app.corpus
                .add_subsonic_search_result(outcome.result, &outcome.client);
            app.resync_results();
            // an answer is in, so the results for whatever is typed are
            // as complete as a shellsong program should wait for.
            settle.answered();
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
        let shellsong_now = start.elapsed();
        if let Some(ramp) = shellsong_fade {
            let _ = player.set_volume(ramp.level_at(shellsong_now));
            if ramp.finished(shellsong_now) {
                shellsong_fade = None;
            }
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
        let mut shellsong_quit = false;
        while let Some(program) = shellsong_program.as_mut() {
            // recomputed every step, not once per frame: `search(...)`
            // makes the query unsettled again, and a `Resolve` reading
            // a stale `true` would act on the previous query's results,
            // which is the exact bug `search_settled` exists to stop.
            let settled = settle.settled(shellsong_now);
            let env = shellsong_env(
                &app,
                &play_queue,
                &browse_view,
                focus,
                settled,
                shellsong_now,
            );
            match program.step(&env) {
                shellsong::Step::Emit(op) => {
                    let mut actx = ActionCtx {
                        cfg: &cfg,
                        player: &player,
                        app: &mut app,
                        notifications: &mut notifications,
                        queue: &mut play_queue,
                        previous_track: &mut previous_track,
                        player_minimized: &mut player_minimized,
                        browse_view: &mut browse_view,
                        real_art_protocol: &mut real_art_protocol,
                        picker: &mut picker,
                        focus: &mut focus,
                        flash: &mut flash,
                        cache: &cache_index,
                        download_tx: &download_tx,
                        downloading: &mut downloading,
                        scrobble: &mut scrobble,
                        subtitle_tx: &subtitle_tx,
                    };
                    let dispatch = apply_shellsong_op(
                        op,
                        &mut ShellsongCtx {
                            actx: &mut actx,
                            console: &mut console,
                            console_open: &mut console_open,
                            now: shellsong_now,
                            now_playing: &now_playing,
                            favorites: &mut favorites,
                            remote_starred: &mut remote_starred,
                            queue_view_open: &mut queue_view_open,
                            queue_selected: &mut queue_selected,
                            suggestions: &mut suggestions,
                            suggestions_selected: &mut suggestions_selected,
                            settings_menu_open: &mut settings_menu_open,
                            keybinds_menu_open: &mut keybinds_menu_open,
                            keybinds_menu_selected: &mut keybinds_menu_selected,
                            eq_menu_open: &mut eq_menu_open,
                            eq_menu_selected: &mut eq_menu_selected,
                            lyrics_lines: &mut lyrics_lines,
                            lyrics_track: &mut lyrics_track,
                            lyrics_scroll: &mut lyrics_scroll,
                            lyrics_client: &lyrics_client,
                            base_volume: &mut base_volume,
                            crossfade_enabled: &mut crossfade_enabled,
                            fade_in_started_at: &mut fade_in_started_at,
                            fade: &mut shellsong_fade,
                            sleep_due: &mut sleep_due,
                            bottombar_selected,
                            scan_rx: &mut scan_rx,
                            remote_search_tx: &remote_search_tx,
                            settle: &mut settle,
                            saved_queues: &mut saved_queues,
                        },
                    );
                    if dispatch == Dispatch::Quit {
                        shellsong_quit = true;
                        shellsong_program = None;
                        break;
                    }
                    // the last op of the last statement: drop it now
                    // rather than spending another step to be told.
                    if shellsong_program.as_ref().is_some_and(|p| p.is_done()) {
                        shellsong_program = None;
                        break;
                    }
                }
                shellsong::Step::Note(text) => console.note(text),
                // the results this is waiting on are not in yet; resume
                // next frame rather than spinning or blocking.
                shellsong::Step::AwaitResults { .. } => break,
                shellsong::Step::Failed(e) => {
                    console.fail(e.to_string());
                    shellsong_program = None;
                    break;
                }
                shellsong::Step::Done => {
                    shellsong_program = None;
                    break;
                }
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
        let overlay = onboarding.is_some()
            || nav_manager.is_some()
            || help_open
            || settings_menu_open
            || keybinds_menu_open
            || eq_menu_open;
        let selection_pulsing = match focus {
            Focus::Sidebar => !app.results.is_empty(),
            Focus::Workspace => browse_view.is_some(),
            Focus::BottomBar => false,
        };
        let banner_showing =
            now_playing.is_none() && browse_view.is_none() && lyrics_lines.is_none();
        let animating = !overlay
            && (selection_pulsing
                || banner_showing
                || app.editing_query
                || console_open
                // a program resumes a frame at a time, so dropping to
                // the idle poll would make an awaited search crawl.
                || shellsong_program.is_some()
                || browse_view.as_ref().is_some_and(|bv| bv.filtering)
                || (now_playing.is_some() && !player_minimized)
                || scrub.is_some()
                || input_mode == InputMode::Move
                || flash.is_some_and(|(_, at)| at.elapsed() < FLASH_DURATION));
        let timeout = if animating {
            frame.interval()
        } else {
            anim::IDLE_POLL
        };

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
                                        save_config(&cfg, &mut notifications);
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
                                    save_config(&cfg, &mut notifications);
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
                                save_config(&cfg, &mut notifications);
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
                                        save_config(&cfg, &mut notifications);
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
                                        save_config(&cfg, &mut notifications);
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
                    let dispatch = dispatch_action(
                        action,
                        &mut ActionCtx {
                            cfg: &cfg,
                            player: &player,
                            app: &mut app,
                            notifications: &mut notifications,
                            queue: &mut play_queue,
                            previous_track: &mut previous_track,
                            player_minimized: &mut player_minimized,
                            browse_view: &mut browse_view,
                            real_art_protocol: &mut real_art_protocol,
                            picker: &mut picker,
                            focus: &mut focus,
                            flash: &mut flash,
                            cache: &cache_index,
                            download_tx: &download_tx,
                            downloading: &mut downloading,
                            scrobble: &mut scrobble,
                            subtitle_tx: &subtitle_tx,
                        },
                    );
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
                        save_config(&cfg, &mut notifications);
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
                                save_config(&cfg, &mut notifications);
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
                        save_config(&cfg, &mut notifications);
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
                        // art mode is a 3-state cycle (ascii -> real
                        // -> off); both directions step the same way
                        // 'i' already does, rather than adding a
                        // second reverse-cycle method just for this.
                        KeyCode::Char('h' | 'l') | KeyCode::Left | KeyCode::Right
                            if settings_menu_selected == 0 =>
                        {
                            art_mode = art_mode.toggle();
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
                if eq_menu_open {
                    if key.code == KeyCode::Esc {
                        eq_menu_open = false;
                        cfg.eq_preset = if app.eq_custom {
                            None
                        } else {
                            Some(eq::PRESETS[app.eq_preset].name.to_string())
                        };
                        cfg.eq_gains = app.eq_gains;
                        save_config(&cfg, &mut notifications);
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
                            eq_menu_selected =
                                (eq_menu_selected + 1).min(ui::eq_menu::ROW_COUNT - 1);
                        }
                        KeyCode::Char('k') | KeyCode::Up => {
                            eq_menu_selected = eq_menu_selected.saturating_sub(1);
                        }
                        KeyCode::Enter => {
                            apply_eq_preset_row(
                                &mut app,
                                &player,
                                &mut notifications,
                                eq_menu_selected,
                            );
                        }
                        KeyCode::Char('h') | KeyCode::Left
                            if eq_menu_selected >= ui::eq_menu::PRESET_COUNT =>
                        {
                            adjust_eq_band(
                                &mut app,
                                &player,
                                &mut notifications,
                                eq_menu_selected - ui::eq_menu::PRESET_COUNT,
                                -1.0,
                            );
                        }
                        KeyCode::Char('l') | KeyCode::Right
                            if eq_menu_selected >= ui::eq_menu::PRESET_COUNT =>
                        {
                            adjust_eq_band(
                                &mut app,
                                &player,
                                &mut notifications,
                                eq_menu_selected - ui::eq_menu::PRESET_COUNT,
                                1.0,
                            );
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
                            save_config(&cfg, &mut notifications);
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
                            save_config(&cfg, &mut notifications);
                        }
                        KeyCode::Char('R') => {
                            keymap.reset_all();
                            cfg.keybinds = keymap.overrides();
                            save_config(&cfg, &mut notifications);
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
                            save_config(&cfg, &mut notifications);
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
                        KeyCode::Enter | KeyCode::Esc => {
                            if let Err(e) = player.seek(s.target_secs, false) {
                                notifications.push(Severity::Error, format!("error seeking: {e}"));
                            }
                            scrub = None;
                            scrub_pending = false;
                            scrub_sent_at = None;
                        }
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
                            match *cursor {
                                Field::Pause => match player.toggle_pause() {
                                    Ok(()) => flash = Some((Field::Pause, Instant::now())),
                                    Err(e) => notifications
                                        .push(Severity::Error, format!("error pausing: {e}")),
                                },
                                Field::Loop => {
                                    let mode = app.cycle_loop();
                                    let mpv_loop = if mode == LoopMode::One { "inf" } else { "no" };
                                    match player.set_loop_file(mpv_loop) {
                                        Ok(()) => flash = Some((Field::Loop, Instant::now())),
                                        Err(e) => notifications.push(
                                            Severity::Error,
                                            format!("error setting loop: {e}"),
                                        ),
                                    }
                                }
                                Field::Shuffle => {
                                    // same semantics as the 's' key
                                    // (`Action::ShuffleToggled`):
                                    // turning it on reshuffles what's
                                    // upcoming, turning it off leaves
                                    // the order alone.
                                    let mut msg = String::new();
                                    if app.toggle_shuffle() {
                                        let shuffled = play_queue.shuffled_upcoming();
                                        msg = resync_upcoming(
                                            &player,
                                            &cache_index,
                                            &mut play_queue,
                                            shuffled,
                                        );
                                    }
                                    if msg.starts_with("error") {
                                        notifications.push(Severity::Error, msg);
                                    } else {
                                        flash = Some((Field::Shuffle, Instant::now()));
                                    }
                                }
                                Field::Speed => {
                                    let speed = app.bump_speed(1.0);
                                    match player.set_speed(speed) {
                                        Ok(()) => flash = Some((Field::Speed, Instant::now())),
                                        Err(e) => notifications.push(
                                            Severity::Error,
                                            format!("error setting speed: {e}"),
                                        ),
                                    }
                                }
                                Field::Eq => {
                                    eq_menu_open = true;
                                    eq_menu_selected = if app.eq_custom {
                                        ui::eq_menu::PRESET_COUNT
                                    } else {
                                        app.eq_preset
                                    };
                                }
                                Field::Crossfade => {
                                    crossfade_enabled = !crossfade_enabled;
                                    if !crossfade_enabled {
                                        fade_in_started_at = None;
                                        let _ = player.set_volume(base_volume);
                                    }
                                    flash = Some((Field::Crossfade, Instant::now()));
                                }
                                Field::Progress => {
                                    // duration is captured here, once,
                                    // so each arrow key can clamp
                                    // without asking mpv again; nothing
                                    // loaded means nothing to scrub, so
                                    // the drag just doesn't open.
                                    let (elapsed, duration) =
                                        (playback.elapsed_secs, playback.duration_secs);
                                    if let Some(at) = elapsed {
                                        scrub = Some(Scrub {
                                            target_secs: at,
                                            duration_secs: duration,
                                        });
                                        scrub_pending = false;
                                        scrub_sent_at = None;
                                    }
                                }
                            }
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
                    && lyrics_lines.is_none();
                if banner_showing {
                    match key.code {
                        KeyCode::Left => {
                            logo_index = (logo_index + ui::banner::logo_count() - 1)
                                % ui::banner::logo_count();
                            cfg.logo_style = ui::banner::logo_name(logo_index).map(String::from);
                            save_config(&cfg, &mut notifications);
                            continue;
                        }
                        KeyCode::Right => {
                            logo_index = (logo_index + 1) % ui::banner::logo_count();
                            cfg.logo_style = ui::banner::logo_name(logo_index).map(String::from);
                            save_config(&cfg, &mut notifications);
                            continue;
                        }
                        KeyCode::Char('0') => {
                            logo_index = 0;
                            cfg.logo_style = None;
                            save_config(&cfg, &mut notifications);
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
                    continue;
                }

                if key.code == keymap.get(KeyAction::CycleArtMode) {
                    art_mode = art_mode.toggle();
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
                                if let Err(msg) =
                                    toggle_star(&item, &mut favorites, &mut remote_starred)
                                {
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
                                let _ = trigger_downloads(&item, &mut dctx, &mut notifications);
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
                    match open_browse_view(&album) {
                        Ok(new_bv) => {
                            real_art_protocol = picker.as_mut().and_then(|p| {
                                new_bv.art.clone().map(|img| p.new_resize_protocol(img))
                            });
                            browse_view = Some(new_bv);
                        }
                        Err(msg) => notifications.push(Severity::Error, msg),
                    }
                    continue;
                }

                // lyrics view intercepts input directly rather than going
                // through App, since scrolling it shouldn't move the
                // underlying results selection.
                if lyrics_lines.is_some() {
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
                                let rows = lyrics_lines
                                    .as_ref()
                                    .map(|l| {
                                        let borrowed: Vec<&str> =
                                            l.iter().map(String::as_str).collect();
                                        ui::lyrics_view::wrapped_len(
                                            &borrowed,
                                            lyrics_width.max(1) as usize,
                                        )
                                    })
                                    .unwrap_or(0);
                                lyrics_scroll =
                                    ui::scroll::clamp_to_content(lyrics_scroll + 1, rows);
                            }
                        }
                        KeyCode::Char('k') | KeyCode::Up => {
                            lyrics_scroll = lyrics_scroll.saturating_sub(1)
                        }
                        KeyCode::Esc | KeyCode::Char('q') => {
                            lyrics_lines = None;
                            lyrics_scroll = 0;
                        }
                        _ if key.code == keymap.get(KeyAction::OpenLyrics) => {
                            lyrics_lines = None;
                            lyrics_scroll = 0;
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
                    let dispatch = dispatch_action(
                        app::Action::Play(item),
                        &mut ActionCtx {
                            cfg: &cfg,
                            player: &player,
                            app: &mut app,
                            notifications: &mut notifications,
                            queue: &mut play_queue,
                            previous_track: &mut previous_track,
                            player_minimized: &mut player_minimized,
                            browse_view: &mut browse_view,
                            real_art_protocol: &mut real_art_protocol,
                            picker: &mut picker,
                            focus: &mut focus,
                            flash: &mut flash,
                            cache: &cache_index,
                            download_tx: &download_tx,
                            downloading: &mut downloading,
                            scrobble: &mut scrobble,
                            subtitle_tx: &subtitle_tx,
                        },
                    );
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
                        let starred = suggestion_starred_keys(&remote_starred, &favorites);
                        let catalog = suggest::Catalog::from_corpus(&app.corpus);
                        let guard = cache_index.lock().expect("cache mutex poisoned");
                        suggestions = Some(suggest::suggest(&suggest::Request::new(
                            &catalog,
                            &guard,
                            &starred,
                            cache::now_secs(),
                        )));
                        drop(guard);
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
                    let bv = open_starred_view(&cfg.navidrome_servers, &favorites);
                    real_art_protocol = picker
                        .as_mut()
                        .and_then(|p| bv.art.clone().map(|img| p.new_resize_protocol(img)));
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
                    video_fullscreen = !video_fullscreen;
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
                        if let Err(msg) = toggle_star(&item, &mut favorites, &mut remote_starred) {
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
                        let _ = trigger_downloads(&item, &mut dctx, &mut notifications);
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
                                        lyrics_track = label(item);
                                        lyrics_scroll = 0;
                                    }
                                    Ok(None) => {
                                        notifications.push(Severity::Info, "no lyrics found")
                                    }
                                    Err(e) => notifications.push(
                                        Severity::Error,
                                        format!("lyrics lookup failed: {e}"),
                                    ),
                                }
                            }
                            None => notifications.push(Severity::Info, "no lyrics for this item"),
                        },
                        None => notifications.push(Severity::Info, "nothing playing"),
                    }
                    continue;
                }

                let action = app.handle_key(key.code, &keymap);
                let dispatch = dispatch_action(
                    action,
                    &mut ActionCtx {
                        cfg: &cfg,
                        player: &player,
                        app: &mut app,
                        notifications: &mut notifications,
                        queue: &mut play_queue,
                        previous_track: &mut previous_track,
                        player_minimized: &mut player_minimized,
                        browse_view: &mut browse_view,
                        real_art_protocol: &mut real_art_protocol,
                        picker: &mut picker,
                        focus: &mut focus,
                        flash: &mut flash,
                        cache: &cache_index,
                        download_tx: &download_tx,
                        downloading: &mut downloading,
                        scrobble: &mut scrobble,
                        subtitle_tx: &subtitle_tx,
                    },
                );
                if dispatch == Dispatch::Quit {
                    break;
                }
            }
        }
    }
    drop(guard);

    Ok(())
}

#[cfg(test)]
mod download_policy_tests {
    use super::*;

    #[test]
    fn two_plays_on_one_day_never_trigger_a_download() {
        // "if i press enter on a song twice it downloads it, thats not
        // what i want": the play count used to be what crossed the
        // threshold, so it did.
        let mut idx = CacheIndex::default();
        let days = idx.record_play("t1", "2026-03-01");
        assert!(!should_auto_download(days, 2, false, false));
        let days = idx.record_play("t1", "2026-03-01");
        assert!(!should_auto_download(days, 2, false, false));
    }

    #[test]
    fn two_distinct_days_do_trigger_a_download() {
        let mut idx = CacheIndex::default();
        idx.record_play("t1", "2026-03-01");
        let days = idx.record_play("t1", "2026-03-02");
        assert!(should_auto_download(days, 2, false, false));
    }

    #[test]
    fn the_day_threshold_is_configurable() {
        let mut idx = CacheIndex::default();
        idx.record_play("t1", "2026-03-01");
        let days = idx.record_play("t1", "2026-03-02");
        assert!(!should_auto_download(days, 3, false, false));
        let days = idx.record_play("t1", "2026-03-03");
        assert!(should_auto_download(days, 3, false, false));
        // a threshold of 1 (or a nonsense 0) downloads on first play.
        let mut idx = CacheIndex::default();
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
    use crate::browse::tests::{test_client, test_songs, test_view};
    use crate::library::{LocalTrack, MediaKind};
    use std::path::PathBuf;

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
            &None,
            Focus::BottomBar,
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
        let browsing = Some(test_view(test_songs(2)));
        let env = shellsong_env(
            &app,
            &PlayQueue::default(),
            &browsing,
            Focus::Workspace,
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
            subsonic::SearchResult {
                artists: Vec::new(),
                albums: vec![subsonic::Album {
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
mod focus_tests {
    use super::*;

    #[test]
    fn sidebar_right_reaches_workspace_and_back_left() {
        assert_eq!(
            focus_in_direction(Focus::Sidebar, KeyCode::Right),
            Focus::Workspace
        );
        assert_eq!(
            focus_in_direction(Focus::Workspace, KeyCode::Left),
            Focus::Sidebar
        );
    }

    #[test]
    fn down_from_sidebar_or_workspace_reaches_the_bottom_bar() {
        assert_eq!(
            focus_in_direction(Focus::Sidebar, KeyCode::Down),
            Focus::BottomBar
        );
        assert_eq!(
            focus_in_direction(Focus::Workspace, KeyCode::Down),
            Focus::BottomBar
        );
    }

    #[test]
    fn up_from_the_bottom_bar_always_reaches_the_sidebar() {
        assert_eq!(
            focus_in_direction(Focus::BottomBar, KeyCode::Up),
            Focus::Sidebar
        );
    }

    #[test]
    fn wasd_and_arrows_are_the_same_four_directions() {
        for (letter, arrow) in [
            ('a', KeyCode::Left),
            ('d', KeyCode::Right),
            ('w', KeyCode::Up),
            ('s', KeyCode::Down),
        ] {
            assert_eq!(focus_direction_key(KeyCode::Char(letter)), Some(arrow));
            // shift+w arrives uppercase on most terminals.
            assert_eq!(
                focus_direction_key(KeyCode::Char(letter.to_ascii_uppercase())),
                Some(arrow)
            );
            assert_eq!(focus_direction_key(arrow), Some(arrow));
        }
    }

    #[test]
    fn keys_that_are_not_directions_are_rejected() {
        // tab/enter/esc in particular: move mode reads those as "leave
        // move mode", so they must never also register as a move.
        for code in [
            KeyCode::Char('j'),
            KeyCode::Char('q'),
            KeyCode::Enter,
            KeyCode::Esc,
            KeyCode::Tab,
        ] {
            assert_eq!(focus_direction_key(code), None);
        }
    }

    #[test]
    fn directions_with_nowhere_to_go_are_a_no_op() {
        // nothing is further left than the sidebar, further right
        // than the workspace, or further down than the bottom bar.
        assert_eq!(
            focus_in_direction(Focus::Sidebar, KeyCode::Left),
            Focus::Sidebar
        );
        assert_eq!(
            focus_in_direction(Focus::Sidebar, KeyCode::Up),
            Focus::Sidebar
        );
        assert_eq!(
            focus_in_direction(Focus::Workspace, KeyCode::Right),
            Focus::Workspace
        );
        assert_eq!(
            focus_in_direction(Focus::Workspace, KeyCode::Up),
            Focus::Workspace
        );
        assert_eq!(
            focus_in_direction(Focus::BottomBar, KeyCode::Down),
            Focus::BottomBar
        );
        assert_eq!(
            focus_in_direction(Focus::BottomBar, KeyCode::Left),
            Focus::BottomBar
        );
        assert_eq!(
            focus_in_direction(Focus::BottomBar, KeyCode::Right),
            Focus::BottomBar
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
    fn a_named_protocol_only_claims_real_art_once_the_probe_answers() {
        assert_eq!(
            art_mode_for_picker(config::AlbumArtMode::Kitty, true),
            browse::ArtMode::Real
        );
        assert_eq!(
            art_mode_for_picker(config::AlbumArtMode::Kitty, false),
            browse::ArtMode::Ascii,
            "real mode without a picker renders nothing at all"
        );
        assert_eq!(
            art_mode_for_picker(config::AlbumArtMode::Sixel, false),
            browse::ArtMode::Ascii
        );
    }

    #[test]
    fn auto_and_blocks_stay_ascii_whatever_the_probe_says() {
        for detected in [true, false] {
            assert_eq!(
                art_mode_for_picker(config::AlbumArtMode::Auto, detected),
                browse::ArtMode::Ascii
            );
            assert_eq!(
                art_mode_for_picker(config::AlbumArtMode::Blocks, detected),
                browse::ArtMode::Ascii
            );
            assert_eq!(
                art_mode_for_picker(config::AlbumArtMode::Off, detected),
                browse::ArtMode::Off
            );
        }
    }

    #[test]
    fn only_a_named_protocol_counts_as_asking_for_real_art() {
        assert!(wants_real_art(config::AlbumArtMode::Kitty));
        assert!(wants_real_art(config::AlbumArtMode::Sixel));
        assert!(wants_real_art(config::AlbumArtMode::Iterm2));
        assert!(!wants_real_art(config::AlbumArtMode::Auto));
        assert!(!wants_real_art(config::AlbumArtMode::Blocks));
        assert!(!wants_real_art(config::AlbumArtMode::Off));
    }

    #[test]
    fn loading_messages_fit_a_narrow_sidebar() {
        for (scanning, syncing) in [(true, true), (true, false), (false, true)] {
            let msg = loading_message(scanning, syncing).unwrap();
            assert!(msg.len() <= 30, "{msg:?} would be truncated in the sidebar");
        }
    }
}
