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
mod search;
mod subsonic;
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
use search::SearchItem;
use theme::{shade, NEAR_BLACK};
use ui::albumart::AsciiArt;
use ui::banner::Banner;
use ui::browse::BrowseListView;
use ui::controlsbar::{ControlsBar, Field};
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

/// Appends `item` to mpv's own playlist (plays immediately if nothing
/// was already loaded, otherwise queues gaplessly; mpv auto-advances
/// its own playlist on end-of-file with none of the round-trip gap that
/// reactively reloading on our end would introduce).
fn try_enqueue(player: &mpv::Mpv, item: &SearchItem, cache: &Arc<Mutex<CacheIndex>>) -> String {
    match resolve_playable(item, cache) {
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
fn resync_queue(player: &mpv::Mpv, items: &[SearchItem], cache: &Arc<Mutex<CacheIndex>>) -> String {
    if let Err(e) = player.playlist_clear() {
        return format!("error resyncing queue: {e}");
    }
    for item in items {
        if let Some(path) = resolve_playable(item, cache) {
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
    listens_threshold: u32,
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

fn go_to_previous(
    cfg: &config::Config,
    player: &mpv::Mpv,
    notifications: &mut Notifications,
    now_playing: &mut Option<SearchItem>,
    previous_track: &mut Option<SearchItem>,
    queue: &mut Vec<SearchItem>,
    hooks: &mut TrackChangeCtx,
) -> String {
    let Some(prev) = previous_track.take() else {
        return "no previous track".to_string();
    };
    let (status, played) = try_play(player, &prev, hooks.dctx.cache);
    if played {
        if let Some(interrupted) = now_playing.take() {
            queue.insert(0, interrupted);
        }
        fire_subtitle_fetch(cfg, hooks.subtitle_tx, &prev);
        note_playback_started(&prev, hooks.dctx, notifications);
        start_tracking_playback(cfg, &prev, hooks.scrobble);
        *now_playing = Some(prev);
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
    now_playing: &'a mut Option<SearchItem>,
    previous_track: &'a mut Option<SearchItem>,
    original_queue: &'a mut Vec<SearchItem>,
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
            save_playback_state(ctx.player, ctx.now_playing, &ctx.app.queue);
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
                Ok(mut tracks) => {
                    let first = tracks.remove(0);
                    let (s, played) = try_play(ctx.player, &first, ctx.cache);
                    if played {
                        for t in &tracks {
                            let _ = try_enqueue(ctx.player, t, ctx.cache);
                        }
                        *ctx.original_queue = std::iter::once(first.clone())
                            .chain(tracks.iter().cloned())
                            .collect();
                        fire_subtitle_fetch(ctx.cfg, ctx.subtitle_tx, &first);
                        let mut dctx = DownloadCtx {
                            cache: ctx.cache,
                            download_tx: ctx.download_tx,
                            downloading: ctx.downloading,
                            listens_threshold: ctx.cfg.cache_listens_threshold,
                        };
                        note_playback_started(&first, &mut dctx, ctx.notifications);
                        start_tracking_playback(ctx.cfg, &first, ctx.scrobble);
                        *ctx.previous_track = ctx.now_playing.replace(first);
                        ctx.app.queue = tracks;
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
                        let s = try_enqueue(ctx.player, t, ctx.cache);
                        if s.starts_with("error") {
                            ctx.notifications.push(Severity::Error, s);
                        }
                    }
                    let n = tracks.len();
                    ctx.app.queue.extend(tracks);
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
        Action::QueueReordered(items) => {
            let msg = resync_queue(ctx.player, &items, ctx.cache);
            if msg.starts_with("error") {
                ctx.notifications.push(Severity::Error, msg);
            } else {
                *ctx.flash = Some((Field::Shuffle, Instant::now()));
            }
        }
        Action::SetLoop(mode) => {
            // "loop all" isn't an mpv concept; it's tracked ourselves
            // (see `original_queue`) and acted on when the queue
            // empties, not here. mpv only ever needs to know about
            // single-file loop.
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

/// Called every time a remote song actually starts playing (every
/// `now_playing`-replacing call site threads this through): bumps its
/// play count and, once that crosses `dctx.listens_threshold`, kicks
/// off a background download so the track is cached for next time
/// (see `cache.rs`/`download.rs`). A no-op for local items (never
/// cached at all) and for anything already cached or already
/// mid-download.
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
    let (count, already_cached) = match dctx.cache.lock() {
        Ok(mut idx) => {
            let count = idx.record_play(id);
            let _ = idx.save();
            (count, idx.cached_path(id).is_some())
        }
        Err(_) => return,
    };
    if count < dctx.listens_threshold || already_cached || dctx.downloading.contains(id) {
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
/// are already local, nothing to cache).
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
fn notify_query_changed(app: &app::App, remote_search_tx: &mpsc::Sender<RemoteSearchQuery>) {
    if app.query.trim().is_empty() {
        return;
    }
    let clients = app.corpus.remote_clients();
    if clients.is_empty() {
        return;
    }
    let _ = remote_search_tx.send(RemoteSearchQuery {
        query: app.query.clone(),
        clients,
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
    let (scan_tx, scan_rx) = mpsc::channel::<library::ScanBatch>();
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
    // sidebar_selected/bottom_selected (below) stay bright for this
    // long right after *leaving* move mode, then fall back to their
    // normal dark level. during move mode itself they're bright the
    // whole time, which is the real "where am I" answer; this is just
    // the tail end of it, so the landing is still visible for a beat
    // after the mode drops.
    let mut focus_flash_until = Instant::now();
    const FOCUS_FLASH_DURATION: Duration = Duration::from_millis(700);

    let mut favorites = favorites::load();
    // song ids this session knows are starred, across every configured
    // server; arrives with each background sync result and is kept
    // current on every manual toggle. unioned rather than replaced, so
    // a toggle can't be clobbered by a fetch that was already in
    // flight when it happened.
    let mut remote_starred: HashSet<String> = HashSet::new();

    let mut now_playing: Option<SearchItem> = None;
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
    // the full track list a play session started with (first track +
    // everything that got queued alongside it), kept around purely so
    // "loop all" has something to restart from once `app.queue` drains;
    // `app.queue` itself is a live, shrinking FIFO, not a fixed list.
    let mut original_queue: Vec<SearchItem> = Vec::new();

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
            if try_play(&player, &item, &cache_index).1 {
                if let Some(pos) = saved.position_secs {
                    let _ = player.seek(pos, false);
                }
                fire_subtitle_fetch(&cfg, &subtitle_tx, &item);
                let mut dctx = DownloadCtx {
                    cache: &cache_index,
                    download_tx: &download_tx,
                    downloading: &mut downloading,
                    listens_threshold: cfg.cache_listens_threshold,
                };
                note_playback_started(&item, &mut dctx, &mut notifications);
                start_tracking_playback(&cfg, &item, &mut scrobble);
                now_playing = Some(item);
            }
        }
        for queued_path in &saved.queue_paths {
            if let Some(item) = persistence::item_from_path(queued_path) {
                pending_enqueue.push(item.clone());
                app.queue.push(item);
            }
        }
        if let Some(vol) = saved.volume {
            let _ = player.set_volume(vol);
        }
    }
    trace::mark("playback state restored (queue deferred)");

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
    // `Focus::BottomBar`'s own selection cursor; independent of
    // `flash`, a steady cursor position rather than a transient
    // acknowledgment, but both feed `ControlsBar`'s rendering.
    let mut bottombar_selected = Field::Loop;
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
    // everything that costs an ipc round-trip per track runs on the
    // first pass through the loop instead of before it; see the drain
    // right after `draw`.
    let mut first_frame_drawn = false;
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

            // resize mode's own panel pick highlights these the same
            // way it always has; a *permanent* highlight tied to
            // plain navigation focus was tried and reverted earlier
            // this session (it made the plain '│' segments a visibly
            // different, permanently-DIM shade from the section-
            // header junctions right next to them, which always
            // stayed NEAR_BLACK). A brief pulse right when focus
            // actually changes, decaying back to that same dark level
            // within FOCUS_FLASH_DURATION, doesn't have that problem:
            // it's indistinguishable from the resting state the rest
            // of the time, but still answers "where am I" exactly
            // when that's actually in question.
            // bright for the whole time move mode is active (that's
            // the point: you can see where the next move starts from
            // before pressing anything), plus a short decaying tail
            // after it drops so the landing stays visible for a beat.
            let focus_lit = input_mode == InputMode::Move || Instant::now() < focus_flash_until;
            let sidebar_selected = (panels.resize_mode
                && panels.resize_selected == ResizePanel::Sidebar)
                || (!panels.resize_mode
                    && focus_lit
                    && matches!(focus, Focus::Sidebar | Focus::Workspace));
            let bottom_selected = (panels.resize_mode
                && panels.resize_selected == ResizePanel::BottomBar)
                || (!panels.resize_mode && focus_lit && focus == Focus::BottomBar);

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
                    ui::browse::rule_rows(main_content, bv, art_mode, cell_aspect())
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
                        cell_aspect: cell_aspect(),
                    },
                    main_content,
                );
            } else if show_full_player {
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
                        eq_preset: eq_label(&app),
                        crossfade_on: crossfade_enabled,
                        previous: previous_label.as_deref(),
                        next: next_label.as_deref(),
                        keymap: &keymap,
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
                let aspect = cell_aspect();
                f.render_widget(
                    BrowseListView {
                        view: bv,
                        favorites: &favorites,
                        remote_starred: &remote_starred,
                        art_mode,
                        cell_aspect: aspect,
                        cursor_visible,
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
                let elapsed_secs = start.elapsed().as_secs_f32();
                let phase = elapsed_secs * 0.25;
                f.render_widget(
                    Banner {
                        phase,
                        elapsed_secs,
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
                        cell_aspect: cell_aspect(),
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
                    format!("{} plays", cfg.cache_listens_threshold),
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
                let (elapsed_secs, duration_secs, paused) = playback_info(&player);
                f.render_widget(
                    MiniPlayerBar {
                        title: &label(item),
                        elapsed_secs,
                        duration_secs,
                        paused,
                        loop_label: app.loop_mode.label(),
                        shuffle_on: app.shuffle,
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
            let flash_level = flash.map(|(field, at)| {
                let t = at.elapsed().as_secs_f32() / FLASH_DURATION.as_secs_f32();
                (field, (1.0 - t).clamp(0.0, 1.0))
            });
            f.render_widget(
                ControlsBar {
                    move_mode: input_mode == InputMode::Move,
                    focus_label: match focus {
                        Focus::Sidebar => "sidebar",
                        Focus::Workspace => "workspace",
                        Focus::BottomBar => "bottom bar",
                    },
                    queue_len: app.queue.len(),
                    loop_label: app.loop_mode.label(),
                    shuffle_on: app.shuffle,
                    speed: app.speed,
                    eq_preset: eq_label(&app),
                    crossfade_on: crossfade_enabled,
                    flash: flash_level,
                    selected: (focus == Focus::BottomBar).then_some(bottombar_selected),
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
                ui::helpbox::HelpBox {
                    label: &format!(
                        "{} help",
                        keymap::key_label(keymap.get(KeyAction::OpenHelp))
                    ),
                },
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
        })?;

        // the ui is on screen; now hand mpv the restored queue, which
        // is one blocking ipc round-trip per track (200 of them on a
        // real saved queue) and has no business delaying the first
        // frame. one-shot, not per-frame work.
        if !first_frame_drawn {
            first_frame_drawn = true;
            trace::mark("first frame drawn");
            for item in pending_enqueue.drain(..) {
                let _ = try_enqueue(&player, &item, &cache_index);
            }
            trace::mark("mpv playlist refilled");
        }

        // non-blocking: mpv auto-advances its own playlist on
        // end-of-file (gaplessly); this just keeps our queue *view* in
        // sync with what mpv already moved past, nothing to load here.
        if let Ok(mpv::Event::EndFile) = player.events().try_recv() {
            match app.pop_queue() {
                Some(next) => {
                    set_vid_for_item(&player, &next);
                    fire_subtitle_fetch(&cfg, &subtitle_tx, &next);
                    let mut dctx = DownloadCtx {
                        cache: &cache_index,
                        download_tx: &download_tx,
                        downloading: &mut downloading,
                        listens_threshold: cfg.cache_listens_threshold,
                    };
                    note_playback_started(&next, &mut dctx, &mut notifications);
                    start_tracking_playback(&cfg, &next, &mut scrobble);
                    previous_track = now_playing.replace(next);
                }
                // mpv has no concept of "the queue we built", only its
                // own (now-empty) playlist; "loop all" replaying from
                // the top is purely our own bookkeeping via
                // `original_queue`, reloaded the same first-play-then-
                // enqueue-the-rest way a fresh play does.
                None if app.loop_mode == LoopMode::All && !original_queue.is_empty() => {
                    let mut tracks = original_queue.clone().into_iter();
                    let first = tracks.next().expect("checked non-empty above");
                    let (s, played) = try_play(&player, &first, &cache_index);
                    if !played {
                        notifications.push(Severity::Error, s);
                    } else {
                        let rest: Vec<SearchItem> = tracks.collect();
                        for t in &rest {
                            let _ = try_enqueue(&player, t, &cache_index);
                        }
                        fire_subtitle_fetch(&cfg, &subtitle_tx, &first);
                        let mut dctx = DownloadCtx {
                            cache: &cache_index,
                            download_tx: &download_tx,
                            downloading: &mut downloading,
                            listens_threshold: cfg.cache_listens_threshold,
                        };
                        note_playback_started(&first, &mut dctx, &mut notifications);
                        start_tracking_playback(&cfg, &first, &mut scrobble);
                        previous_track = now_playing.replace(first);
                        app.queue = rest;
                    }
                }
                None => now_playing = None,
            }
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

        // scrobbling: fires the "single" listen exactly once per
        // track, the moment the configured threshold is crossed;
        // checked every tick regardless of which view is on screen,
        // same unconditional treatment as the crossfade ramp above.
        // a no-op (see `fire_scrobble`) if listenbrainz isn't
        // configured at all.
        if !scrobble.scrobbled {
            if let Some(item) = &now_playing {
                let (elapsed, duration, _) = playback_info(&player);
                if let (Some(elapsed), Some(duration)) = (elapsed, duration) {
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
                } else if !help_open {
                    let before = app.query.clone();
                    app.paste_into_query(text);
                    if app.query != before {
                        notify_query_changed(&app, &remote_search_tx);
                    }
                }
            }

            if let Event::Key(key) = ev {
                // the one unconditional quit, regardless of what
                // modal/menu is open; 'q' still works too but only
                // where nothing else claims it first (see app.rs),
                // same as every other single-letter shortcut.
                if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
                    save_playback_state(&player, &now_playing, &app.queue);
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
                        notify_query_changed(&app, &remote_search_tx);
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
                            now_playing: &mut now_playing,
                            previous_track: &mut previous_track,
                            original_queue: &mut original_queue,
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
                            cfg.cache_listens_threshold =
                                cfg.cache_listens_threshold.saturating_sub(1).max(1);
                        }
                        KeyCode::Char('l') | KeyCode::Right if settings_menu_selected == 3 => {
                            cfg.cache_listens_threshold = (cfg.cache_listens_threshold + 1).min(20);
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
                            // one last pulse on the pane just landed
                            // in, so leaving move mode still answers
                            // "where am I now" for a moment after the
                            // permanent move-mode highlight drops.
                            focus_flash_until = Instant::now() + FOCUS_FLASH_DURATION;
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

                // pane-local navigation for the bottom bar, same
                // "only j/k/arrows/enter care which pane has focus"
                // rule as the browse view further down: global
                // playback controls (space/f/b/c/etc.) fall through
                // untouched below regardless of focus, only these
                // four keys are intercepted here.
                if focus == Focus::BottomBar {
                    match key.code {
                        KeyCode::Char('j') | KeyCode::Down => {
                            bottombar_selected = bottombar_selected.next();
                            continue;
                        }
                        KeyCode::Char('k') | KeyCode::Up => {
                            bottombar_selected = bottombar_selected.prev();
                            continue;
                        }
                        KeyCode::Enter => {
                            match bottombar_selected {
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
                                    let items = app.toggle_shuffle_and_get_queue();
                                    let msg = resync_queue(&player, &items, &cache_index);
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
                            }
                            continue;
                        }
                        _ => {}
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
                            let visible = bv.visible_track_indices();
                            let from = visible.iter().position(|&i| i == bv.selected).unwrap_or(0);
                            let session: Vec<SearchItem> = visible[from..]
                                .iter()
                                .map(|&i| bv.tracks[i].clone())
                                .collect();
                            let mut tracks = session.clone().into_iter();
                            if let Some(first) = tracks.next() {
                                let (s, played) = try_play(&player, &first, &cache_index);
                                if played {
                                    let rest: Vec<SearchItem> = tracks.collect();
                                    for t in &rest {
                                        let _ = try_enqueue(&player, t, &cache_index);
                                    }
                                    fire_subtitle_fetch(&cfg, &subtitle_tx, &first);
                                    let mut dctx = DownloadCtx {
                                        cache: &cache_index,
                                        download_tx: &download_tx,
                                        downloading: &mut downloading,
                                        listens_threshold: cfg.cache_listens_threshold,
                                    };
                                    note_playback_started(&first, &mut dctx, &mut notifications);
                                    start_tracking_playback(&cfg, &first, &mut scrobble);
                                    previous_track = now_playing.replace(first);
                                    app.queue = rest;
                                    original_queue = session;
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
                                    listens_threshold: cfg.cache_listens_threshold,
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
                        listens_threshold: cfg.cache_listens_threshold,
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
                        &mut now_playing,
                        &mut previous_track,
                        &mut app.queue,
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
                            listens_threshold: cfg.cache_listens_threshold,
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
                        now_playing: &mut now_playing,
                        previous_track: &mut previous_track,
                        original_queue: &mut original_queue,
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
