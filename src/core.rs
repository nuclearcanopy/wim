//! The player core: everything a shellsong `Op` can touch that is not
//! a pane. `wim` and `shellswim` both drive this; what they do not
//! share goes through `Surface`, whose default impls decline. Read
//! docs/library-seam.md before moving anything across that line.

use std::collections::HashSet;
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use crate::app;
use crate::cache::{self, CacheIndex};
use crate::download::{self, DownloadEvent};
use crate::notify::{Notifications, Severity};
use crate::playqueue::PlayQueue;
use crate::search::SearchItem;
use crate::shellsong::{self, Console, ViewTarget};
use crate::shellsong_host::{Ramp, SearchSettle};
use crate::ui::controlsbar::Field;
use crate::ui::results_list::label;
use crate::{
    browse, config, eq, favorites, library, listenbrainz, lyrics, mpv, opensubtitles, persistence,
    search, subsonic, ui, Focus,
};

/// Local items resolve straight to a file path mpv can load. A remote
/// `Song` resolves straight to a stream url (mpv loads http urls the
/// same as local paths, nothing special needed). Remote `Album`/
/// `Playlist`/`Artist` do NOT resolve here: they represent many
/// tracks, not one, and get expanded into individual `Song` items by
/// `resolve_tracks`/`artist_tracks` before anything ever calls this
/// function on them.
pub fn playable_path(item: &SearchItem) -> Option<String> {
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
pub fn resolve_playable(item: &SearchItem, cache: &Arc<Mutex<CacheIndex>>) -> Option<String> {
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
pub fn resolve_tracks(item: &SearchItem) -> Result<Vec<SearchItem>, String> {
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
pub fn artist_tracks(
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
    // latency); they're independent requests, so they go through
    // `album_details`'s bounded pool instead (a thread per album was
    // a socket per album against a real navidrome instance).
    let ids: Vec<&str> = artist_albums.iter().map(|a| a.id.as_str()).collect();
    let details = client.album_details(&ids);

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
/// Local files flip the persisted favorites set; remote songs call
/// navidrome's own star/unstar so it stays in sync with any other
/// subsonic client hitting the same server. Returns the new state.
pub fn toggle_star(
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
/// A live client for `nav` if the corpus already holds one, else a
/// fresh one. `Client::new` re-reads `password_file` off disk and
/// starts with a cold connection pool, so building one per call is
/// paid for in both disk io and the first round trip.
///
/// Reuse rather than a cached singleton on purpose: these clients are
/// the corpus's own, so the password lives exactly as long as the
/// corpus does and is still zeroized when it drops. Parking one in a
/// `static` would mean it never drops and never zeroizes.
/// Drops any kept client no current profile still describes.
///
/// The sync thread holds clients across passes, so this is what stops
/// a removed or edited profile from being synced through a client
/// carrying the old credentials; dropping it is also what zeroizes
/// that password. `Client::matches` counts the stream options, so a
/// bitrate change is an edit too.
pub fn retain_live_clients(
    clients: &mut Vec<Arc<subsonic::Client>>,
    servers: &[config::NavidromeConfig],
) {
    clients.retain(|c| servers.iter().any(|nav| c.matches(nav)));
}
/// `Result` rather than `Option` so the reason survives: `Client::new`
/// fails on an unreadable `password_file` or a malformed url, and
/// "connection failed" with no cause is a worse error than the one
/// this replaced.
pub fn client_for(
    nav: &config::NavidromeConfig,
    live: &[Arc<subsonic::Client>],
) -> anyhow::Result<Arc<subsonic::Client>> {
    if let Some(live) = live.iter().find(|c| c.matches(nav)) {
        return Ok(live.clone());
    }
    Ok(Arc::new(subsonic::Client::new(nav)?))
}
pub fn open_starred_view(
    servers: &[config::NavidromeConfig],
    favorites: &favorites::Favorites,
    live: &[Arc<subsonic::Client>],
) -> browse::BrowseView {
    let per_server: Vec<Vec<SearchItem>> = thread::scope(|scope| {
        servers
            .iter()
            .map(|nav| {
                scope.spawn(|| {
                    let client = client_for(nav, live).ok()?;
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
        loading: false,
    };
    // through `set_tracks`, not a bare assignment: it re-clamps
    // `selected` with the list it's an index into.
    view.set_tracks(tracks);
    view
}
/// A subtitle file the background fetch wrote to disk. `sub_add` isn't
/// called there: the `Mpv` handle isn't `Send`, so the main thread
/// loads it when this lands, same division of labor as every other
/// background worker in this file.
pub struct SubtitleReady {
    pub path: String,
    pub release: String,
    pub language: String,
}
/// Starts an opensubtitles search + download on its own thread, same
/// fire-and-forget contract as `fire_playing_now`: a no-op whenever
/// `item` isn't a local video, no api key is configured, or the
/// search/download fails for any reason (no matches, network error,
/// quota exhausted). Two network round-trips, so it never runs inline;
/// it used to, which on startup meant holding the first frame behind
/// opensubtitles' response time.
pub fn fire_subtitle_fetch(
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
pub fn lyrics_query(item: &SearchItem) -> Option<(String, String, String)> {
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
pub fn scrobble_metadata(item: &SearchItem) -> Option<listenbrainz::TrackMetadata> {
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
pub fn fire_playing_now(cfg: &config::Config, item: &SearchItem) {
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
pub fn fire_scrobble(cfg: &config::Config, item: &SearchItem, listened_at: i64) {
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
pub struct ScrobbleState {
    /// Set once the "single" listen has fired for the current track,
    /// so the per-tick threshold check (main loop) only ever fires it
    /// once, not every tick after the threshold is crossed.
    pub scrobbled: bool,
    /// The current track's own start time, not whenever a scrobble
    /// happens to fire; used as the `single` submission's
    /// `listened_at`.
    pub started_unix: Option<i64>,
}
/// Called from every site that sets `now_playing` to a new `Some(..)`:
/// resets the per-track scrobble bookkeeping and fires "playing_now".
/// Centralized here so each call site is one line instead of
/// repeating all three steps (easy to silently miss one otherwise).
pub fn start_tracking_playback(
    cfg: &config::Config,
    item: &SearchItem,
    scrobble: &mut ScrobbleState,
) {
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
pub fn try_play(
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
pub fn set_vid_for_item(player: &mpv::Mpv, item: &SearchItem) {
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
pub enum SessionStart {
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
pub fn load_playlist(
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
pub fn enqueue_track(
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
pub fn resync_upcoming(
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
pub fn save_playback_state(player: &mpv::Mpv, queue: &PlayQueue) {
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
pub struct DownloadCtx<'a> {
    pub cache: &'a Arc<Mutex<CacheIndex>>,
    pub download_tx: &'a mpsc::Sender<DownloadEvent>,
    pub downloading: &'a mut HashSet<String>,
    /// Distinct play days a track needs before it auto-downloads; see
    /// `config::Config::cache_days_threshold`.
    pub day_threshold: u32,
}
/// Bundles the two independent "a track just started" trackers
/// (download-on-repeat-listen, scrobble-on-threshold) into one
/// argument, purely to keep `go_to_previous` under clippy's
/// too-many-arguments limit; each one is otherwise unrelated to the
/// other and gets built/used on its own everywhere else.
pub struct TrackChangeCtx<'a, 'b> {
    pub dctx: &'a mut DownloadCtx<'b>,
    pub scrobble: &'a mut ScrobbleState,
    /// where `fire_subtitle_fetch` reports back; bundled here for the
    /// same argument-count reason as everything else in this struct.
    pub subtitle_tx: &'a mpsc::Sender<SubtitleReady>,
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
pub fn go_to_previous(
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
pub enum Dispatch {
    Continue,
    Quit,
}
/// The tracks enter on a browse row plays: the selected one through
/// the end of what the filter actually shows, not of the whole list (a
/// filtered artist page used to play the unfiltered tail). Shared by
/// the browse view's own enter and shellsong's `go()`, so the two
/// cannot pick different tracks.
pub fn browse_session(bv: &browse::BrowseView) -> Vec<SearchItem> {
    let visible = bv.visible_track_indices();
    let from = visible.iter().position(|&i| i == bv.selected).unwrap_or(0);
    visible[from..]
        .iter()
        .filter_map(|&i| bv.tracks.get(i).cloned())
        .collect()
}
/// `main::Focus` is the host's, `shellsong::Focus` the language's
/// mirror of it (kept separate so the language depends on nothing in
/// the ui). One to one.
pub fn shellsong_focus(focus: Focus) -> shellsong::Focus {
    match focus {
        Focus::Sidebar => shellsong::Focus::Sidebar,
        Focus::Workspace => shellsong::Focus::Workspace,
        Focus::BottomBar => shellsong::Focus::BottomBar,
    }
}
/// mpv's current volume, which `volume(+10%)` nudges from and a fade
/// ramps away from. `None` only if the ipc read fails.
pub fn current_volume(player: &mpv::Mpv) -> Option<f64> {
    player.get_property("volume").ok().and_then(|v| v.as_f64())
}
/// Whether `item` is starred right now, so `star()`/`unstar()` can set
/// a state rather than flip one (`toggle_star` only flips).
pub fn is_starred(
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
pub fn album_page(
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
/// What one op reported into the scrollback. `Described` takes
/// `Op::describe()`, which is usually the right line; the host
/// overrides with `Note` when it knows a better one (a track title, a
/// count) and with `Failed` when the op could not be applied. Every op
/// produces exactly one of these: nothing in shellsong is silent
/// (docs/shellsong.md R9).
pub enum Report {
    Described,
    Note(String),
    Failed(String),
}
pub fn op_done() -> (Dispatch, Report) {
    (Dispatch::Continue, Report::Described)
}
pub fn op_note(text: impl Into<String>) -> (Dispatch, Report) {
    (Dispatch::Continue, Report::Note(text.into()))
}
pub fn op_fail(text: impl Into<String>) -> (Dispatch, Report) {
    (Dispatch::Continue, Report::Failed(text.into()))
}
/// One ipc write, reported either way; mpv failing is the one thing
/// most of the transport ops can do wrong.
pub fn op_ipc(result: anyhow::Result<()>) -> (Dispatch, Report) {
    match result {
        Ok(()) => op_done(),
        Err(e) => op_fail(e.to_string()),
    }
}
/// Which way a selection op moves. `up()`/`down()` carry a delta,
/// `top()`/`bottom()` are ends.
pub enum SelectStep {
    By(i32),
    Top,
    Bottom,
}
/// Whether a play that just happened should kick off a background
/// download. Pure, and kept apart from the clock and the cache file so
/// the policy itself is testable: `days_played` is the number of
/// *distinct calendar days* the track has been played on, never the
/// raw play count, which is what made pressing enter twice in one
/// sitting download a track nobody asked to cache.
pub fn should_auto_download(
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
pub fn note_playback_started(
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
pub fn trigger_downloads(
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
/// Deletes every cached file past `cfg.cache_eviction_days` unplayed;
/// best-effort, same as every other cache operation (a failed save
/// just means the eviction is re-attempted next sweep).
pub fn sweep_cache(cache: &Arc<Mutex<CacheIndex>>, eviction_days: u64) {
    if let Ok(mut idx) = cache.lock() {
        if idx.sweep_stale(eviction_days.saturating_mul(24 * 3600)) > 0 {
            let _ = idx.save();
        }
    }
}
/// The one starred set `suggest::Request` wants: remote song ids and
/// local favorite paths together, matching `suggest::Track::key`.
pub fn suggestion_starred_keys(
    remote_starred: &HashSet<String>,
    favorites: &favorites::Favorites,
) -> HashSet<String> {
    remote_starred
        .iter()
        .cloned()
        .chain(favorites.paths().cloned())
        .collect()
}
/// Enter on a preset row of the eq menu applies it, built-in or
/// saved; a no-op when `row` is a band row instead, since adjusting
/// those is `adjust_eq_band`'s job. Deliberately not wired to j/k:
/// this overwrites all ten gains, which is not something plain
/// navigation should ever do.
/// Saved eq curve names, for shellsong's `eq()` completion. Built
/// fresh per keystroke like the corpus candidates are; the list is a
/// handful of short strings, so cloning beats threading a borrow
/// through `CorpusCandidates`'s lifetime.
pub fn eq_preset_names(saved: &eq::CustomPresets) -> Vec<String> {
    saved.iter().map(|p| p.name.clone()).collect()
}
pub fn apply_eq_row(
    app: &mut app::App,
    player: &mpv::Mpv,
    notifications: &mut Notifications,
    saved: &eq::CustomPresets,
    row: usize,
) {
    let (source, gains) = match ui::eq_menu::cursor(row, saved.len()) {
        ui::eq_menu::Cursor::Builtin(i) => {
            // keep the `CycleEq` key's cycle where the menu left it,
            // so 'e' carries on from the preset you just picked.
            app.eq_preset = i;
            (eq::Source::Builtin(i), eq::PRESETS[i].gains)
        }
        ui::eq_menu::Cursor::Saved(i) => match saved.get(i) {
            Some(preset) => (eq::Source::Saved(i), eq::clamp_gains(preset.gains)),
            None => return,
        },
        ui::eq_menu::Cursor::Band(_) => return,
    };
    app.eq_source = Some(source);
    app.eq_custom = false;
    app.eq_gains = gains;
    apply_eq_gains(app, player, notifications);
}
/// Pushes `app.eq_gains` to mpv, reporting a failure: the menu paths
/// used to discard the error while the global cycle-eq key reported
/// it, so the same failure was visible from one route and silent from
/// the other.
pub fn apply_eq_gains(app: &app::App, player: &mpv::Mpv, notifications: &mut Notifications) {
    if let Err(e) = player.set_af(&eq::build_af_filter(&app.eq_gains)) {
        notifications.push(Severity::Error, format!("error setting eq: {e}"));
    }
}
/// How long to let rapid keystrokes settle before actually firing a
/// `search3` request, so a burst of typing costs one network round-
/// trip instead of one per character; see `spawn_background_search`.
pub const REMOTE_SEARCH_DEBOUNCE: Duration = Duration::from_millis(300);
/// One query-changed event, carrying the servers to search against at
/// the time it was sent; `Corpus::remote_clients` (already-indexed
/// servers only) is cheap to call per-keystroke on the main thread, so
/// the snapshot travels with the query rather than the background
/// thread needing its own separate copy of the server list to keep in
/// sync.
pub struct RemoteSearchQuery {
    pub query: String,
    pub clients: Vec<Arc<subsonic::Client>>,
}
/// One server's `search3` result for whatever query it was fired for;
/// not tagged with the query itself; see `spawn_background_search`'s
/// doc for why that's fine.
pub struct RemoteSearchOutcome {
    pub client: Arc<subsonic::Client>,
    pub result: subsonic::SearchResult,
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
pub fn spawn_background_search(
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
/// Everything an op can touch that is not a pane: no `Focus`, no
/// browse view, no menu flags, no selection cursors. Both front-ends
/// build one of these per use, the way `DownloadCtx` already is; what
/// wim has beyond it stays on wim's side of `Surface`.
///
/// This field list is the contract, not `app::App`'s; see
/// docs/library-seam.md for what is still joined inside `App` and why
/// that was left alone.
pub struct CoreCtx<'a> {
    pub cfg: &'a config::Config,
    pub player: &'a mpv::Mpv,
    /// The session: corpus, query and result list, plus the playback
    /// modes. The interpreter resolves names against the results in
    /// both binaries, so they are core; `selected` and the key
    /// handling on the same struct belong to wim and are never read
    /// here.
    pub app: &'a mut app::App,
    pub notifications: &'a mut Notifications,
    /// mpv's playlist as an index -> item map; the only queue there
    /// is, and the only thing "what's playing" is read out of.
    pub queue: &'a mut PlayQueue,
    pub previous_track: &'a mut Option<SearchItem>,
    pub cache: &'a Arc<Mutex<CacheIndex>>,
    pub download_tx: &'a mpsc::Sender<DownloadEvent>,
    pub downloading: &'a mut HashSet<String>,
    pub scrobble: &'a mut ScrobbleState,
    pub subtitle_tx: &'a mpsc::Sender<SubtitleReady>,
    /// Where every op's one line lands (docs/shellsong.md R9).
    pub console: &'a mut Console,
    /// The same monotonic reading `Env::now` carries this frame, so a
    /// ramp started by an op and ticked next frame share one clock.
    pub now: Duration,
    pub now_playing: &'a Option<SearchItem>,
    pub favorites: &'a mut favorites::Favorites,
    pub remote_starred: &'a mut HashSet<String>,
    pub lyrics_client: &'a lyrics::Client,
    // volume, and everything that ramps it.
    pub base_volume: &'a mut f64,
    pub crossfade_enabled: &'a mut bool,
    pub fade_in_started_at: &'a mut Option<Instant>,
    /// `fade(in|out)`'s ramp; `sleep_due` is its deadline-driven
    /// cousin. Both reuse the crossfade ramp's shape rather than
    /// cutting the audio dead.
    pub fade: &'a mut Option<Ramp>,
    pub sleep_due: &'a mut Option<Duration>,
    /// Replaced wholesale by `rescan()`, which needs a fresh channel:
    /// the scan thread signs off by dropping its sender.
    pub scan_rx: &'a mut mpsc::Receiver<library::ScanBatch>,
    pub remote_search_tx: &'a mpsc::Sender<RemoteSearchQuery>,
    pub settle: &'a mut SearchSettle,
    /// Snapshot behind `load_queue`'s completion; refreshed on open
    /// and after a save rather than read off disk per keystroke.
    pub saved_queues: &'a mut Vec<String>,
}

// MOVED_ITEMS_MARKER

/// What the core could not do by itself. Every method has a default
/// that declines with a one-line reason, so a front-end opts in to
/// exactly what it can honour and a new method cannot silently break
/// one. wim honours nearly all of it; shellswim honours what a shell
/// can show and declines the rest by name.
///
/// Passed alongside `CoreCtx` as a second, disjoint borrow: the
/// surface holds the front-end's pane locals, `CoreCtx` holds the
/// core's. See docs/library-seam.md for why this is a trait and not a
/// queue of requests.
pub trait Surface {
    /// Named in every decline, so the message says where the thing it
    /// is refusing does exist.
    fn front_end(&self) -> &'static str;

    /// `Target::Selection`: what is highlighted. wim resolves it the
    /// way its star/download keys do; a front-end with no highlight
    /// should answer with the head of the current result set, which is
    /// where wim's highlight starts.
    fn selection(&self, core: &CoreCtx) -> Option<SearchItem> {
        core.app.results.first().cloned()
    }

    /// `Focus` as the interpreter sees it; `go()` resolves against it.
    fn focus(&self) -> shellsong::Focus {
        shellsong::Focus::Sidebar
    }

    /// Whether a browse view is open, which widens what counts as a
    /// selection and which `go()` target is meant.
    fn browsing(&self) -> bool {
        false
    }

    /// One of the `ViewTarget::is_screen()` targets. The five that are
    /// not screens print into the console and never reach here.
    fn open_screen(&mut self, target: ViewTarget, core: &mut CoreCtx) -> (Dispatch, Report) {
        let _ = core;
        self.decline(format!("view({})", format!("{target:?}").to_lowercase()))
    }

    fn move_selection(&mut self, step: SelectStep, core: &mut CoreCtx) -> (Dispatch, Report) {
        let _ = (step, core);
        self.decline("moving the selection")
    }

    /// `go()` with a browse view open: play from the highlighted row.
    fn go_browse_row(&mut self, core: &mut CoreCtx) -> (Dispatch, Report) {
        let _ = core;
        self.decline("playing from a browse row")
    }

    /// An album/playlist/artist opens its track listing rather than
    /// playing; `Action::Play` on one lands here.
    fn open_browse(&mut self, item: &SearchItem, core: &mut CoreCtx) -> (Dispatch, Report) {
        let _ = core;
        self.decline(format!("opening {}", label(item)))
    }

    /// Which bottom-bar control `go()` would press. `None` means
    /// nothing on the bar is focusable here.
    fn bar_field(&self) -> Option<Field> {
        None
    }

    /// Light a bar control for a moment. Pure feedback; a front-end
    /// with no such affordance ignores it rather than declining,
    /// because the op itself did happen.
    fn flash(&mut self, field: Field) {
        let _ = field;
    }

    /// Whether a standalone play takes over the full player pane.
    fn minimize_player(&mut self, minimize: bool) {
        let _ = minimize;
    }

    /// The queue view's cursor, for the two ops that have to reclamp
    /// it after shortening the queue under it.
    fn queue_cursor(&mut self) -> Option<&mut usize> {
        None
    }

    /// Dropped when an op takes over the screen; see
    /// `Op::closes_console`.
    fn close_console(&mut self) {}

    /// The one-line refusal. Shared so every decline reads the same.
    fn decline(&self, what: impl std::fmt::Display) -> (Dispatch, Report) {
        op_fail(format!("{} cannot do {what}; wim can", self.front_end()))
    }
}

/// Declines everything, which is what makes it the proof that the op
/// table needs no pane state: `core::tests` applies ops against this.
pub struct Headless;

impl Surface for Headless {
    fn front_end(&self) -> &'static str {
        "this front-end"
    }
}
