//! The player core: everything a shellsong `Op` can touch that is not
//! a pane. `wim` and `shellswim` both drive this; what they do not
//! share goes through `Surface`, whose default impls decline. Read
//! docs/library-seam.md before moving anything across that line.

use std::collections::HashSet;
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use crate::app::{self, Action, LoopMode};
use crate::cache::{self, CacheIndex};
use crate::download::{self, DownloadEvent};
use crate::notify::{Notifications, Severity};
use crate::playqueue::PlayQueue;
use crate::search::SearchItem;
use crate::shellsong::{self, Console, GoTarget, Op, Program, Target, ViewTarget};
use crate::shellsong_host::{Ramp, SearchSettle};
use crate::ui::controlsbar::Field;
use crate::ui::results_list::label;
use crate::{
    browse, config, eq, favorites, library, listenbrainz, lyrics, mpv, namehint, opensubtitles,
    persistence, queues, search, shellsong_host, subsonic, ui, Focus,
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
///
/// The remote half is **optimistic**: the local set flips and the next
/// frame shows it, while the server hears about it on a thread. A star
/// should feel instant, and this used to be a round trip on the
/// keypress. A refusal arrives on `star_tx` and the caller puts the
/// flip back; see `StarFailed`.
pub fn toggle_star(
    item: &SearchItem,
    favorites: &mut favorites::Favorites,
    remote_starred: &mut HashSet<String>,
    star_tx: &mpsc::Sender<StarFailed>,
) -> Result<bool, String> {
    match item {
        SearchItem::LocalMusic { path, .. } | SearchItem::LocalVideo { path, .. } => {
            let now_starred = favorites.toggle(&path.to_string_lossy());
            let _ = favorites::save(favorites);
            Ok(now_starred)
        }
        SearchItem::Song { id, client, .. } => {
            let wanted = !remote_starred.contains(id);
            if wanted {
                remote_starred.insert(id.clone());
            } else {
                remote_starred.remove(id);
            }
            let (client, id, tx) = (client.clone(), id.clone(), star_tx.clone());
            thread::spawn(move || {
                let sent = if wanted {
                    client.star(&id)
                } else {
                    client.unstar(&id)
                };
                if let Err(e) = sent {
                    // a dead receiver only means the app is shutting
                    // down, in which case the flip does not matter.
                    let _ = tx.send(StarFailed {
                        id,
                        wanted,
                        error: e.to_string(),
                    });
                }
            });
            Ok(wanted)
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
/// Opens the starred listing through the browse loader, so the
/// keypress returns at once.
///
/// Local favorites are a file read and go into the view immediately;
/// the per-server fetches run on the loader's thread and fold in when
/// they answer. This used to fan out and join before returning, so
/// pressing the key paid for the slowest configured server.
pub fn open_starred(
    loader: &mut browse::Loader,
    servers: &[config::NavidromeConfig],
    favorites: &favorites::Favorites,
    live: &[Arc<subsonic::Client>],
) -> browse::BrowseView {
    let local: Vec<SearchItem> = favorites
        .paths()
        .filter_map(|path| persistence::item_from_path(path))
        .collect();
    // resolved here rather than in the loader: `client_for` lives in
    // this module and reuses a live client instead of re-reading
    // `password_file` and starting with a cold pool.
    let clients: Vec<Arc<subsonic::Client>> = servers
        .iter()
        .filter_map(|nav| client_for(nav, live).ok())
        .collect();
    loader.open_starred(local, clients)
}
/// A subtitle file the background fetch wrote to disk. `sub_add` isn't
/// called there: the `Mpv` handle isn't `Send`, so the main thread
/// loads it when this lands, same division of labor as every other
/// background worker in this file.
/// A star the server refused, so the optimistic local flip has to go
/// back. Carries what the local set was set *to*, which is what
/// reverting needs; the id alone would not say which way.
#[derive(Debug, Clone)]
pub struct StarFailed {
    pub id: String,
    pub wanted: bool,
    pub error: String,
}

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
/// What a reply's text is, by the api's own `instrumental` flag rather
/// than "both fields happened to be empty": a real track with missing
/// lyrics data must not be mislabelled instrumental. Shared so wim's
/// pane and shellswim's console cannot disagree about one reply.
pub fn lyrics_text(l: &lyrics::Lyrics) -> String {
    if l.instrumental {
        return "(instrumental, no lyrics)".to_string();
    }
    l.plain
        .clone()
        .or_else(|| l.synced.clone())
        .unwrap_or_else(|| "(no lyrics text available)".to_string())
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
    /// Fetches off-thread. `lookup` is a round trip to lrclib, and
    /// calling it from a key handler froze the whole ui until lrclib
    /// answered; "nothing blocks" is the rule in both binaries.
    pub lyrics_loader: &'a mut lyrics::Loader,
    /// Where a refused star reports back, so the optimistic flip can
    /// be undone; drained by each binary's own per-frame drain.
    pub star_tx: &'a mpsc::Sender<StarFailed>,
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
    /// The live `let` bindings. Held here rather than re-read per
    /// keystroke, and written back to disk by `define`/`unset`.
    pub vars: &'a mut shellsong::Scope,
    /// The per-identity settings, written back by `remember`/`forget`.
    pub bindings: &'a mut shellsong_host::Bindings,
}

/// The one place an `Action` from `app.handle_key` is acted on. Both
/// key paths route through here (ordinary keys, and a query being
/// typed); the typing path used to discard its action, which is why
/// the first enter after a search only closed the text box.
pub fn dispatch_action(action: Action, core: &mut CoreCtx, surf: &mut dyn Surface) -> Dispatch {
    match action {
        Action::Quit => {
            save_playback_state(core.player, core.queue);
            return Dispatch::Quit;
        }
        // albums/playlists/artists open a track-listing view instead
        // of playing immediately ("let me scroll through the songs"
        // first); songs/local files still play straight away.
        Action::Play(item @ SearchItem::Album { .. })
        | Action::Play(item @ SearchItem::Playlist { .. })
        | Action::Play(item @ SearchItem::Artist { .. }) => {
            // the view itself (title, and "loading..." until the
            // tracks land) is the confirmation; no status line echo.
            // nothing here waits on the network: the loader answers at
            // once and fills the view in from `Loader::poll`.
            // declined rather than opened by a front-end with no
            // pane to open it in; the report is dropped here because
            // this is the keypress path. `Op::Play` routes a page
            // through `Surface::open_browse` itself so the shell can
            // say why it will not.
            surf.open_browse(&item, core);
        }
        Action::Play(item) => {
            match resolve_tracks(&item) {
                Ok(tracks) if tracks.is_empty() => core
                    .notifications
                    .push(Severity::Info, "nothing to play (empty album or playlist)"),
                Ok(tracks) => {
                    let first = tracks[0].clone();
                    let interrupted = core.queue.current().cloned();
                    let (s, played) = load_playlist(
                        core.player,
                        core.cache,
                        core.queue,
                        tracks,
                        SessionStart::New,
                    );
                    if played {
                        fire_subtitle_fetch(core.cfg, core.subtitle_tx, &first);
                        let mut dctx = DownloadCtx {
                            cache: core.cache,
                            download_tx: core.download_tx,
                            downloading: core.downloading,
                            day_threshold: core.cfg.cache_days_threshold,
                        };
                        note_playback_started(&first, &mut dctx, core.notifications);
                        start_tracking_playback(core.cfg, &first, core.scrobble);
                        *core.previous_track = interrupted;
                        // a standalone play (not from an open
                        // album/playlist) takes over the full player
                        // by default.
                        surf.minimize_player(false);
                    } else {
                        core.notifications.push(Severity::Error, s);
                    }
                }
                Err(msg) => core.notifications.push(Severity::Error, msg),
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
                        let s =
                            enqueue_track(core.player, t, core.cache, core.queue, core.app.shuffle);
                        if s.starts_with("error") {
                            core.notifications.push(Severity::Error, s);
                        }
                    }
                    let n = tracks.len();
                    core.notifications.push(
                        Severity::Info,
                        if n == 1 {
                            "queued 1 track".to_string()
                        } else {
                            format!("queued {n} tracks")
                        },
                    );
                }
                Err(msg) => core.notifications.push(Severity::Error, msg),
            }
        }
        Action::ShuffleToggled(on) => {
            // turning it off leaves the order alone (nothing to put
            // back); turning it on reshuffles what's still upcoming so
            // it takes effect now, not only on later additions.
            if on {
                let shuffled = core.queue.shuffled_upcoming();
                let msg = resync_upcoming(core.player, core.cache, core.queue, shuffled);
                if msg.starts_with("error") {
                    core.notifications.push(Severity::Error, msg);
                    return Dispatch::Continue;
                }
            }
            surf.flash(Field::Shuffle);
        }
        Action::SetLoop(mode) => {
            // "loop all" isn't an mpv concept; it's tracked ourselves
            // (see `PlayQueue::session`) and acted on once mpv's
            // playlist drains, not here. mpv only ever needs to know
            // about single-file loop.
            let mpv_loop = if mode == LoopMode::One { "inf" } else { "no" };
            match core.player.set_loop_file(mpv_loop) {
                Ok(()) => surf.flash(Field::Loop),
                Err(e) => core
                    .notifications
                    .push(Severity::Error, format!("error setting loop: {e}")),
            }
        }
        Action::SetSpeed(speed) => match core.player.set_speed(speed) {
            Ok(()) => surf.flash(Field::Speed),
            Err(e) => core
                .notifications
                .push(Severity::Error, format!("error setting speed: {e}")),
        },
        Action::SetEq(_name, gains) => {
            let filter = eq::build_af_filter(&gains);
            match core.player.set_af(&filter) {
                Ok(()) => surf.flash(Field::Eq),
                Err(e) => core
                    .notifications
                    .push(Severity::Error, format!("error setting eq: {e}")),
            }
        }
        Action::None => {}
    }
    Dispatch::Continue
}
/// Everything a running program knows about the world, rebuilt from
/// live state before every `step`. `queue_len` counts the whole
/// playlist, played prefix included, because that is what the queue
/// view numbers and therefore what `goto(n)`/`remove(n)` mean.
/// Takes its pieces rather than a whole `CoreCtx` for one reason: a
/// `CoreCtx` holds a live mpv handle, and the env has nothing to do
/// with mpv. One builder either way, so the two front-ends cannot
/// disagree about what a program can see.
pub fn shellsong_env(
    app: &app::App,
    queue: &PlayQueue,
    surf: &dyn Surface,
    settled: bool,
    now: Duration,
) -> shellsong::Env {
    shellsong::Env {
        now,
        query: app.query.clone(),
        results: app.results.iter().map(shellsong::Kind::of).collect(),
        search_settled: settled,
        queue_len: queue.entries().len(),
        browsing: surf.browsing(),
        focus: surf.focus(),
    }
}
/// What a term-taking op acts on. `Index` is a row of the results
/// list; `Selection` is whatever is highlighted, resolved exactly the
/// way the keyboard's star/download keys resolve it (a focused browse
/// view owns the highlight, otherwise the sidebar does).
fn shellsong_item(target: Target, core: &CoreCtx, surf: &dyn Surface) -> Option<SearchItem> {
    match target {
        Target::Index(i) => core.app.results.get(i).cloned(),
        Target::Selection => surf.selection(core),
    }
}
/// Applies one op and reports exactly one line into the scrollback.
/// The report is `Op::describe()` unless the host knew something
/// better; `Op::closes_console` is consulted here, in one place,
/// rather than by each view-opener remembering to.
pub fn apply_op(op: Op, core: &mut CoreCtx, surf: &mut dyn Surface) -> Dispatch {
    let described = op.describe();
    let closes = op.closes_console();
    let (dispatch, report) = run_op(op, core, surf);
    // only on success: closing the console to make room for a screen
    // that then failed to open would take the error message with it.
    if closes && !matches!(report, Report::Failed(_)) {
        surf.close_console();
    }
    match report {
        Report::Described => core.console.note(described),
        Report::Note(text) => core.console.note(text),
        Report::Failed(text) => core.console.fail(text),
    }
    dispatch
}
/// The `Op` -> host table. Every arm goes through the call the matching
/// keypress already makes (`dispatch_action` wherever an `Action`
/// exists), so a shellsong command and a keybind can never diverge in
/// behaviour; see docs/shellsong.md.
fn run_op(op: Op, core: &mut CoreCtx, surf: &mut dyn Surface) -> (Dispatch, Report) {
    match op {
        Op::TogglePause => {
            if core.now_playing.is_none() {
                return op_fail("nothing is playing");
            }
            op_ipc(core.player.toggle_pause())
        }
        Op::SkipNext => {
            if core.now_playing.is_none() {
                return op_fail("nothing is playing");
            }
            op_ipc(core.player.playlist_next())
        }
        Op::SkipBack => {
            if core.previous_track.is_none() {
                return op_fail("no previous track");
            }
            let mut dctx = DownloadCtx {
                cache: core.cache,
                download_tx: core.download_tx,
                downloading: core.downloading,
                day_threshold: core.cfg.cache_days_threshold,
            };
            let mut hooks = TrackChangeCtx {
                dctx: &mut dctx,
                scrobble: core.scrobble,
                subtitle_tx: core.subtitle_tx,
            };
            let status = go_to_previous(
                core.cfg,
                core.player,
                core.notifications,
                core.previous_track,
                core.queue,
                &mut hooks,
            );
            op_note(status)
        }
        Op::SeekAbsolute(pos) => {
            // asked for directly rather than read from the observed
            // cache: this runs once per command, not per frame, and a
            // percent target must resolve against the duration as of
            // now rather than whatever the last pushed change said.
            let duration = core
                .player
                .get_property(mpv::PROP_DURATION)
                .ok()
                .and_then(|v| v.as_f64());
            match shellsong_host::seek_target(pos, duration) {
                Ok(secs) => match core.player.seek(secs, false) {
                    Ok(()) => op_note(format!("moved to {secs:.0}s")),
                    Err(e) => op_fail(e.to_string()),
                },
                Err(why) => op_fail(why),
            }
        }
        Op::SeekRelative(delta) => op_ipc(core.player.seek(delta, true)),
        Op::SetVolume(level) => {
            let current = current_volume(core.player).unwrap_or(*core.base_volume);
            let target = shellsong_host::volume_target(level, current);
            match core.player.set_volume(target) {
                // the new level becomes the ceiling the crossfade and
                // fade ramps scale against, or the next fade-in would
                // snap the volume back to whatever it was at startup.
                Ok(()) => {
                    *core.base_volume = target;
                    op_note(format!("volume: {target:.0}%"))
                }
                Err(e) => op_fail(format!("error setting volume: {e}")),
            }
        }
        // mpv's own `mute`, not volume 0: it is reversible without
        // having to remember the level, and `volume(...)` stays the one
        // thing that moves the level.
        Op::Mute(on) => op_ipc(core.player.set_property("mute", serde_json::json!(on))),
        Op::CycleLoop => {
            let mode = core.app.cycle_loop();
            let dispatch = dispatch_action(Action::SetLoop(mode), core, surf);
            (dispatch, Report::Note(format!("loop: {}", mode.label())))
        }
        Op::SetLoop(mode) => {
            core.app.loop_mode = mode;
            let dispatch = dispatch_action(Action::SetLoop(mode), core, surf);
            (dispatch, Report::Described)
        }
        Op::ToggleShuffle => {
            let on = core.app.toggle_shuffle();
            let dispatch = dispatch_action(Action::ShuffleToggled(on), core, surf);
            (
                dispatch,
                Report::Note(format!("shuffle: {}", if on { "on" } else { "off" })),
            )
        }
        Op::SetShuffle(want) => {
            if core.app.shuffle == want {
                return op_note(format!(
                    "shuffle is already {}",
                    if want { "on" } else { "off" }
                ));
            }
            let on = core.app.toggle_shuffle();
            let dispatch = dispatch_action(Action::ShuffleToggled(on), core, surf);
            (dispatch, Report::Described)
        }
        Op::SetSpeed(speed) => {
            core.app.speed = speed;
            let dispatch = dispatch_action(Action::SetSpeed(speed), core, surf);
            (dispatch, Report::Described)
        }
        Op::SpeedUp | Op::SpeedDown => {
            let dir = if matches!(op, Op::SpeedUp) { 1.0 } else { -1.0 };
            let speed = core.app.bump_speed(dir);
            let dispatch = dispatch_action(Action::SetSpeed(speed), core, surf);
            (dispatch, Report::Note(format!("speed: {speed}x")))
        }
        Op::SetEqPreset(name) => {
            // resolved here rather than at lowering time, because the
            // saved curves only exist at runtime; `eq::resolve` reads
            // built-ins first, then the saved list, same precedence the
            // eq menu shows them in.
            let saved = &core.cfg.eq_custom_presets;
            let Some((source, _)) = eq::resolve(&name, saved) else {
                return op_fail(eq::resolve_failure(&name, saved));
            };
            // `apply_eq_row` speaks eq-menu rows, which is exactly
            // built-ins then saved; `eq_menu::cursor` is the inverse of
            // this and a test pins the two agreeing.
            let row = match source {
                eq::Source::Builtin(i) => i,
                eq::Source::Saved(i) => ui::eq_menu::PRESET_COUNT + i,
            };
            apply_eq_row(core.app, core.player, core.notifications, saved, row);
            op_done()
        }
        Op::ToggleCrossfade => {
            *core.crossfade_enabled = !*core.crossfade_enabled;
            if !*core.crossfade_enabled {
                *core.fade_in_started_at = None;
                let _ = core.player.set_volume(*core.base_volume);
            }
            surf.flash(Field::Crossfade);
            op_note(format!(
                "crossfade: {}",
                if *core.crossfade_enabled { "on" } else { "off" }
            ))
        }
        Op::Search(query) => {
            core.app.set_query(&query);
            let fired = notify_query_changed(core.app, core.remote_search_tx);
            core.settle.notified(fired, core.now);
            op_note(format!(
                "search: {query} ({} results so far)",
                core.app.results.len()
            ))
        }
        Op::MoveSelection(delta) => surf.move_selection(SelectStep::By(delta), core),
        Op::SelectTop => surf.move_selection(SelectStep::Top, core),
        Op::SelectBottom => surf.move_selection(SelectStep::Bottom, core),
        Op::Go(GoTarget::Result) => {
            let Some(item) = surf.selection(core) else {
                return op_fail("nothing is selected");
            };
            let name = label(&item);
            let dispatch = dispatch_action(Action::Play(item), core, surf);
            (dispatch, Report::Note(format!("enter on {name}")))
        }
        Op::Go(GoTarget::BrowseRow) => surf.go_browse_row(core),
        Op::Go(GoTarget::BottomBarField) => {
            // enter on the bottom bar tweaks whichever field is
            // selected, and every one of those fields already has its
            // own command; reuse them rather than keeping a second
            // copy of the bar's own match.
            let Some(field) = surf.bar_field() else {
                return surf.decline("bar control to press");
            };
            let field_op = match field {
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
            let result = run_op(field_op, core, surf);
            if closes && !matches!(result.1, Report::Failed(_)) {
                surf.close_console();
            }
            result
        }
        Op::Play(target) => {
            // `go()`'s browse-row reading, for the same reason: with a
            // browse view focused, the selection *is* a row of it, and
            // playing one plays through to the end of the listing.
            if target == Target::Selection
                && surf.focus() == shellsong::Focus::Workspace
                && surf.browsing()
            {
                return run_op(Op::Go(GoTarget::BrowseRow), core, surf);
            }
            let Some(item) = shellsong_item(target, core, surf) else {
                return op_fail("nothing is selected");
            };
            // a page is opened, not played, and opening one needs a
            // pane the core does not have; asked through the surface
            // so a front-end without panes declines by name.
            if matches!(
                item,
                SearchItem::Album { .. } | SearchItem::Playlist { .. } | SearchItem::Artist { .. }
            ) {
                return surf.open_browse(&item, core);
            }
            let name = label(&item);
            let dispatch = dispatch_action(Action::Play(item), core, surf);
            (dispatch, Report::Note(format!("playing: {name}")))
        }
        Op::Enqueue { target, front } => {
            let Some(item) = shellsong_item(target, core, surf) else {
                return op_fail("nothing is selected");
            };
            if !front {
                // `Action::Queued` expands an album/playlist itself and
                // reports its own count; resolving here as well would
                // cost a second fetch of the very same track list.
                let dispatch = dispatch_action(Action::Queued(item), core, surf);
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
                if let Err(why) = enqueue_next(track, core) {
                    return op_fail(why);
                }
            }
            op_note(format!("queued {} tracks next", tracks.len()))
        }
        Op::Star { target, on } => {
            let Some(item) = shellsong_item(target, core, surf) else {
                return op_fail("nothing is selected");
            };
            let verb = if on { "starred" } else { "unstarred" };
            if is_starred(&item, core.favorites, core.remote_starred) == on {
                return op_note(format!("'{}' is already {verb}", item.primary_name()));
            }
            match toggle_star(&item, core.favorites, core.remote_starred, core.star_tx) {
                Ok(_) => op_note(format!("{verb} '{}'", item.primary_name())),
                Err(why) => op_fail(why),
            }
        }
        Op::Download(target) => {
            let Some(item) = shellsong_item(target, core, surf) else {
                return op_fail("nothing is selected");
            };
            let mut dctx = DownloadCtx {
                cache: core.cache,
                download_tx: core.download_tx,
                downloading: core.downloading,
                day_threshold: core.cfg.cache_days_threshold,
            };
            let status = trigger_downloads(&item, &mut dctx, core.notifications);
            op_note(status)
        }
        Op::OpenPage(target) => {
            let Some(item) = shellsong_item(target, core, surf) else {
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
                    album_page(&core.app.corpus, album, client)
                }
                SearchItem::LocalMusic { .. } | SearchItem::LocalVideo { .. } => None,
            };
            match page {
                Some(page) => {
                    let name = label(&page);
                    let dispatch = dispatch_action(Action::Play(page), core, surf);
                    (dispatch, Report::Note(format!("opened {name}")))
                }
                None => op_fail(format!("no page for '{}'", item.primary_name())),
            }
        }
        Op::ClearConsole => {
            // the report line is pushed after this returns, so the
            // console ends up holding exactly one line saying what
            // happened rather than looking like it crashed.
            core.console.lines.clear();
            op_done()
        }
        Op::ClearQueue => {
            // mpv's own `playlist-clear` keeps the file that is
            // playing and drops the rest, which is exactly what
            // clearing the queue means; `entries` only mirrors it.
            match core.player.playlist_clear() {
                Ok(()) => {
                    core.queue.replace_upcoming(Vec::new());
                    if let Some(cursor) = surf.queue_cursor() {
                        *cursor = 0;
                    }
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
                if core.queue.remove(index) {
                    if let Err(e) = core.player.playlist_remove(index) {
                        return op_fail(format!("error removing: {e}"));
                    }
                    removed += 1;
                }
            }
            let left = core.queue.entries().len();
            if let Some(cursor) = surf.queue_cursor() {
                *cursor = (*cursor).min(left.saturating_sub(1));
            }
            op_note(format!("removed {removed} from the queue"))
        }
        Op::GotoQueue(position) => {
            // a `playlist-pos` write, not a reload: mpv keeps the same
            // playlist so `entries` stays index-aligned with it (same
            // as enter in the queue view).
            op_ipc(core.player.set_property(
                "playlist-pos",
                serde_json::json!(position.saturating_sub(1)),
            ))
        }
        Op::ShuffleQueue => {
            let shuffled = core.queue.shuffled_upcoming();
            let count = shuffled.len();
            let status = resync_upcoming(core.player, core.cache, core.queue, shuffled);
            if status.starts_with("error") {
                return op_fail(status);
            }
            op_note(format!("shuffled {count} upcoming"))
        }
        Op::SaveQueue(name) => match queues::save(&name, core.queue.entries()) {
            Ok(0) => op_fail("nothing in the queue to save"),
            Ok(count) => {
                *core.saved_queues = queues::list();
                op_note(format!("saved {count} tracks as '{}'", queues::slug(&name)))
            }
            Err(e) => op_fail(format!("couldn't save '{name}': {e}")),
        },
        Op::LoadQueue(name) => {
            let clients = shellsong_host::clients_of(&core.app.corpus);
            // `load_report` rather than `load`: the plain one discards
            // what it could not rebuild, so a fifty-track queue whose
            // server is gone came back as twelve tracks and said
            // nothing about the other thirty-eight.
            match queues::load_report(&name, &clients) {
                None => op_fail(namehint::unknown(
                    "saved queue",
                    &name,
                    queues::list().iter().map(String::as_str),
                )),
                Some(loaded) if loaded.items.is_empty() => {
                    let why = queues::summarise(&loaded.dropped);
                    if why.is_empty() {
                        op_fail(format!("'{name}' is empty"))
                    } else {
                        op_fail(format!("nothing in '{name}' loaded: {why}"))
                    }
                }
                Some(loaded) => {
                    let count = loaded.items.len();
                    let lost = loaded.dropped.len();
                    match start_session(loaded.items, false, core, surf) {
                        Ok(_) if lost == 0 => op_note(format!("loaded '{name}': {count} tracks")),
                        Ok(_) => op_note(format!(
                            "loaded '{name}': {count} of {} tracks; {} missing: {}",
                            count + lost,
                            lost,
                            queues::summarise(&loaded.dropped)
                        )),
                        Err(why) => op_fail(why),
                    }
                }
            }
        }
        // the terms arrive already expanded: a variable used inside
        // another is resolved while parsing, so this only ever stores
        // what it was handed.
        Op::Define { name, terms } => {
            let count = terms.len();
            let replaced = core.vars.define(&name, terms);
            shellsong_host::save_vars(core.vars);
            let verb = if replaced { "redefined" } else { "defined" };
            op_note(format!("{verb} {name}: {count} terms"))
        }
        Op::Unset(name) => {
            if !core.vars.remove(&name) {
                return op_fail(namehint::unknown("variable", &name, core.vars.names()));
            }
            shellsong_host::save_vars(core.vars);
            op_note(format!("unset {name}"))
        }
        Op::Remember { tier, setting } => {
            let Some(item) = core.now_playing.as_ref() else {
                return op_fail("nothing is playing to remember it for");
            };
            let id = shellsong_host::identity_of(item);
            let Some(name) = shellsong_host::name_at(&id, tier) else {
                return op_fail(format!(
                    "this track has no {} to hang a setting off",
                    tier.label()
                ));
            };
            let report = format!(
                "{} {} remembered for {} '{name}'",
                setting.kind().label(),
                setting.value(),
                tier.label()
            );
            core.bindings.remember(tier, &name, &setting);
            shellsong_host::save_bindings(core.bindings);
            // applied now as well as remembered, through the same arm
            // the typed command goes through, so remembering a setting
            // and setting it cannot drift.
            let (dispatch, _) = run_op(setting.op(), core, surf);
            (dispatch, Report::Note(report))
        }
        Op::Forget { tier, setting } => {
            let Some(item) = core.now_playing.as_ref() else {
                return op_fail("nothing is playing to forget anything for");
            };
            let id = shellsong_host::identity_of(item);
            let Some(name) = shellsong_host::name_at(&id, tier) else {
                return op_fail(format!("this track has no {}", tier.label()));
            };
            if !core.bindings.forget(tier, &name, setting) {
                return op_fail(match setting {
                    Some(kind) => format!(
                        "no {} was remembered for {} '{name}'",
                        kind.label(),
                        tier.label()
                    ),
                    None => format!("nothing was remembered for {} '{name}'", tier.label()),
                });
            }
            shellsong_host::save_bindings(core.bindings);
            op_note(match setting {
                Some(kind) => format!("forgot {} for {} '{name}'", kind.label(), tier.label()),
                None => format!("forgot everything for {} '{name}'", tier.label()),
            })
        }
        // the partition is the language's own: `is_screen()` targets
        // need a pane, the other five print into the console and so
        // behave identically in both binaries.
        Op::View(target) if target.is_screen() => surf.open_screen(target, core),
        Op::View(target) => print_listing(target, core),
        Op::Rescan => {
            // a fresh channel and thread, same as startup: the scan
            // signs off by dropping its sender, so the old receiver
            // can't be reused. Only the local half of the corpus is
            // dropped, or a rescan would also wipe the navidrome
            // catalog until the next sync landed.
            let (tx, rx) = mpsc::channel::<library::ScanBatch>();
            library::spawn_scan(core.cfg.library_paths.clone(), tx);
            *core.scan_rx = rx;
            core.app.corpus.drop_local();
            core.app.scanning_library = true;
            core.app.resync_results();
            op_done()
        }
        Op::Sleep(Some(secs)) => {
            *core.sleep_due = Some(core.now + Duration::from_secs_f64(secs.max(0.0)));
            op_done()
        }
        Op::Sleep(None) => {
            let was_set = core.sleep_due.take().is_some();
            if !was_set {
                return op_note("no sleep timer was set");
            }
            // the ramp may already have taken the volume down.
            let _ = core.player.set_volume(*core.base_volume);
            op_done()
        }
        Op::Fade { into, secs } => {
            let from = current_volume(core.player).unwrap_or(*core.base_volume);
            let to = if into { *core.base_volume } else { 0.0 };
            *core.fade = Some(Ramp::new(
                from,
                to,
                core.now,
                Duration::from_secs_f64(secs.max(0.0)),
            ));
            op_done()
        }
        Op::Quit => {
            let dispatch = dispatch_action(Action::Quit, core, surf);
            (dispatch, Report::Described)
        }
    }
}
/// The five `view(...)` targets that are not screens: they print into
/// the console and need no pane, so both front-ends get them for free
/// and identically. `ViewTarget::is_screen()` is the partition.
pub fn print_listing(target: ViewTarget, core: &mut CoreCtx) -> (Dispatch, Report) {
    match target {
        ViewTarget::Queue => {
            let pos = core.queue.pos();
            let lines: Vec<String> = core
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
                core.console.note(line);
            }
            op_note(format!("{count} in the queue"))
        }
        ViewTarget::Index => {
            let rows = core.app.corpus.breakdown();
            for (source, count) in &rows {
                core.console.note(format!("{source}: {count}"));
            }
            op_note(format!("{} sources indexed", rows.len()))
        }
        ViewTarget::History => {
            // cloned first: the scrollback is being pushed into, and
            // the history lives on the same `Console`.
            let lines: Vec<String> = core.console.history().to_vec();
            if lines.is_empty() {
                return op_note("no history yet");
            }
            let count = lines.len();
            for line in lines {
                core.console.note(format!(": {line}"));
            }
            op_note(format!("{count} past commands"))
        }
        ViewTarget::Playlists => {
            *core.saved_queues = queues::list();
            if core.saved_queues.is_empty() {
                return op_note("no saved queues; save_queue(name) makes one");
            }
            let names = core.saved_queues.clone();
            let count = names.len();
            for name in names {
                core.console.note(name);
            }
            op_note(format!("{count} saved queues"))
        }
        ViewTarget::Bindings => {
            let lines = core.bindings.lines();
            let count = if core.bindings.is_empty() {
                0
            } else {
                lines.len()
            };
            for line in lines {
                core.console.note(line);
            }
            op_note(format!("{count} remembered"))
        }
        // the language generates these lines too, as `let` statements,
        // so the listing is also valid input to paste into a `.shg`.
        ViewTarget::Vars => {
            let lines = core.vars.lines();
            let count = if core.vars.is_empty() { 0 } else { lines.len() };
            for line in lines {
                core.console.note(line);
            }
            op_note(format!("{count} variables"))
        }
        // the one listing whose content the language generates rather
        // than the app, which is what keeps `help()` and `view(help)`
        // from drifting; printing it is still the host's job.
        ViewTarget::Help => {
            let lines = shellsong::help_lines();
            let count = lines.len();
            for line in lines {
                core.console.note(line);
            }
            op_note(format!("{count} commands"))
        }
        // `is_screen()` sends these to the surface; reached only if
        // that partition and this match ever disagree.
        other => (
            Dispatch::Continue,
            Report::Failed(format!("{other:?} is a screen, not a listing")),
        ),
    }
}

/// What one frame of driving a program came to. The loop never
/// blocks: `Await` means the results a `Resolve` wants have not
/// settled, so the caller keeps the program and tries again next
/// frame (see `Program::step`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Drive {
    /// The program is finished; drop it.
    Done,
    /// Suspended on a search; keep it and step again next frame.
    Await,
    /// `quit()` ran; the front-end should exit.
    Quit,
}

/// Steps a program as far as it can go this frame, applying each
/// `Emit` before the next `step`. The one driver, so wim and
/// shellswim cannot diverge on await handling, on when a program is
/// dropped, or on where a failure is reported.
pub fn drive(
    program: &mut Program,
    core: &mut CoreCtx,
    surf: &mut dyn Surface,
    settle_now: impl Fn(&CoreCtx) -> bool,
) -> Drive {
    loop {
        // rebuilt every step, not once per frame: `search(...)` makes
        // the query unsettled again, and a `Resolve` reading a stale
        // `true` would act on the previous query's results, which is
        // the exact bug `search_settled` exists to stop.
        let settled = settle_now(core);
        let env = shellsong_env(core.app, core.queue, surf, settled, core.now);
        match program.step(&env) {
            shellsong::Step::Emit(op) => {
                if apply_op(op, core, surf) == Dispatch::Quit {
                    return Drive::Quit;
                }
                if program.is_done() {
                    return Drive::Done;
                }
            }
            shellsong::Step::Note(text) => core.console.note(text),
            // a non-fatal complaint: reported and the program keeps
            // going, where `Failed` stops it.
            shellsong::Step::Warn(text) => core.console.fail(text),
            shellsong::Step::AwaitResults { .. } => return Drive::Await,
            shellsong::Step::Failed(e) => {
                core.console.fail(e.to_string());
                return Drive::Done;
            }
            shellsong::Step::Done => return Drive::Done,
        }
    }
}

/// Starts a freshly built session: mpv's playlist is replaced and
/// every "a track just started" hook fires for the first track, the
/// same set `Action::Play` fires. One place, so a shellsong-built
/// session can't quietly skip a hook the keyboard runs.
pub fn start_session(
    items: Vec<SearchItem>,
    minimize: bool,
    core: &mut CoreCtx,
    surf: &mut dyn Surface,
) -> Result<String, String> {
    let Some(first) = items.first().cloned() else {
        return Err("nothing to play".to_string());
    };
    let interrupted = core.queue.current().cloned();
    let (status, played) = load_playlist(
        core.player,
        core.cache,
        core.queue,
        items,
        SessionStart::New,
    );
    if !played {
        return Err(status);
    }
    fire_subtitle_fetch(core.cfg, core.subtitle_tx, &first);
    let mut dctx = DownloadCtx {
        cache: core.cache,
        download_tx: core.download_tx,
        downloading: core.downloading,
        day_threshold: core.cfg.cache_days_threshold,
    };
    note_playback_started(&first, &mut dctx, core.notifications);
    start_tracking_playback(core.cfg, &first, core.scrobble);
    *core.previous_track = interrupted;
    surf.minimize_player(minimize);
    Ok(status)
}
/// `add_to_queue(front, ...)`: append, then move the new entry in
/// right behind what is playing. `enqueue_track` still writes both
/// sides (mpv's playlist and the index map); this only reorders, with
/// the same `move_entry` + `playlist_move` pairing the queue view's
/// reorder uses. Appended unshuffled on purpose: `front` means front.
pub fn enqueue_next(item: &SearchItem, core: &mut CoreCtx) -> Result<(), String> {
    let status = enqueue_track(core.player, item, core.cache, core.queue, false);
    if status.starts_with("error") || status.starts_with("not ") {
        return Err(status);
    }
    let from = core.queue.entries().len().saturating_sub(1);
    let to = core.queue.pos().map_or(0, |p| p + 1);
    if to < from && core.queue.move_entry(from, to) {
        if let Err(e) = core.player.playlist_move(from, to) {
            return Err(format!("error moving: {e}"));
        }
    }
    Ok(())
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
pub fn notify_query_changed(
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
/// The `?` list: every command name, packed into rows. Derived from
/// `shellsong::help_lines()` rather than a second table, so a command
/// added to the language appears here without anyone remembering to.
/// `help()` is the full version with signatures; this is the one a new
/// user needs before they know there is a `help()`.
pub fn short_help() -> Vec<String> {
    let mut names: Vec<String> = shellsong::help_lines()
        .iter()
        .filter_map(|line| {
            let sig = line.trim_start();
            let name = sig.split(['(', ' ']).next()?;
            (!name.is_empty()).then(|| name.to_string())
        })
        .collect();
    names.dedup();
    let mut out = vec![format!("{} commands:", names.len())];
    out.extend(pack(&names, HELP_COLUMNS));
    out.push("help() for the arguments, dry(cmd) to see without doing".to_string());
    out
}

/// Rows of at most `budget` cells, words separated by two spaces.
const HELP_COLUMNS: usize = 66;

fn pack(words: &[String], budget: usize) -> Vec<String> {
    let mut rows: Vec<String> = Vec::new();
    let mut row = String::new();
    for w in words {
        let want = if row.is_empty() {
            w.chars().count()
        } else {
            row.chars().count() + 2 + w.chars().count()
        };
        if want > budget && !row.is_empty() {
            rows.push(std::mem::take(&mut row));
        }
        if !row.is_empty() {
            row.push_str("  ");
        }
        row.push_str(w);
    }
    if !row.is_empty() {
        rows.push(row);
    }
    rows
}

/// The name of the live eq curve, which the per-identity settings
/// restore to. A hand-tuned curve has no name, so the configured preset
/// stands in; that is better than leaving one track's curve applied for
/// the rest of the session.
pub fn current_eq_name(app: &app::App, cfg: &config::Config) -> String {
    app.eq_source
        .and_then(|s| eq::source_name(s, &cfg.eq_custom_presets))
        .map(str::to_string)
        .or_else(|| cfg.eq_preset.clone())
        .unwrap_or_else(|| eq::PRESETS[0].name.to_string())
}

/// What the per-identity settings want for a track that just started:
/// the ops to apply, and the one line to toast when the sound actually
/// changed because of them.
///
/// The restore is the load-bearing half, and it lives in
/// `shellsong_host::settle`: a setting the new track does not bind goes
/// back to the value the user had chosen before any binding took over,
/// rather than keeping the last bound one (which is what leaves
/// everything quiet after one quiet song).
pub fn identity_settings(
    item: &SearchItem,
    bindings: &shellsong_host::Bindings,
    overridden: &mut shellsong_host::Overridden,
    eq_now: String,
    volume_now: f64,
    speed_now: f64,
) -> (Vec<Op>, Option<String>) {
    let wanted = bindings.resolve(&shellsong_host::identity_of(item));
    let mut ops = Vec::new();
    if let Some(name) = shellsong_host::settle(&mut overridden.eq, wanted.eq.clone(), eq_now) {
        ops.push(Op::SetEqPreset(name));
    }
    if let Some(v) = shellsong_host::settle(&mut overridden.volume, wanted.volume, volume_now) {
        ops.push(Op::SetVolume(shellsong::Level::Absolute(v)));
    }
    if let Some(s) = shellsong_host::settle(&mut overridden.speed, wanted.speed, speed_now) {
        ops.push(Op::SetSpeed(s));
    }
    // only when something moved, so a bound album says it once on its
    // first track rather than on all twelve.
    let note = (!ops.is_empty()).then(|| match wanted.summary() {
        summary if summary.is_empty() => "settings restored".to_string(),
        summary => format!("remembered: {summary}"),
    });
    (ops, note)
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
        self.decline(&format!("view({})", format!("{target:?}").to_lowercase()))
    }

    fn move_selection(&mut self, step: SelectStep, core: &mut CoreCtx) -> (Dispatch, Report) {
        let _ = (step, core);
        self.decline("selection to move")
    }

    /// `go()` with a browse view open: play from the highlighted row.
    fn go_browse_row(&mut self, core: &mut CoreCtx) -> (Dispatch, Report) {
        let _ = core;
        self.decline("browse row to play from")
    }

    /// An album/playlist/artist opens its track listing rather than
    /// playing; `Action::Play` on one lands here.
    fn open_browse(&mut self, item: &SearchItem, core: &mut CoreCtx) -> (Dispatch, Report) {
        let _ = core;
        self.decline(&format!("page to open {} in", label(item)))
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

    /// The one-line refusal. Shared so every decline reads the same
    /// and always names where the thing does exist.
    fn decline(&self, what: &str) -> (Dispatch, Report) {
        op_fail(format!("{} has no {what}; wim does", self.front_end()))
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

#[cfg(test)]
mod seam_tests {
    use super::*;
    use crate::browse::fixtures::test_songs;

    /// The whole claim of the library split, as a test: an op applies
    /// with `Headless` in the room, which is a unit struct. There is
    /// no `Focus`, no browse view, no menu flag and no selection
    /// cursor anywhere in these calls, and the ops still work and
    /// still report one line each.
    ///
    /// `#[ignore]` because `CoreCtx` holds a live mpv handle and
    /// spawning one is ~400ms; the repo's convention for a test that
    /// needs mpv (see `mpv::tests`). Run it with
    /// `cargo test --lib seam -- --ignored`.
    #[test]
    #[ignore]
    fn ops_apply_with_no_ui_state_at_all() {
        let player = mpv::Mpv::spawn().expect("mpv on PATH");
        // measured on a desktop that may have a real wim playing:
        // exercise the control path, do not touch the speakers.
        let _ = player.set_property("ao", serde_json::json!("null"));
        let mut h = Harness::new();
        let mut surf = Headless;

        // a transport op: reaches mpv, reports one line, no panes.
        let (dispatch, report) = {
            let mut core = h.ctx(&player);
            (
                apply_op(
                    Op::SetVolume(shellsong::Level::Absolute(55.0)),
                    &mut core,
                    &mut surf,
                ),
                core.console.lines.last().map(|l| l.text.clone()),
            )
        };
        assert_eq!(dispatch, Dispatch::Continue);
        assert_eq!(report.as_deref(), Some("volume: 55%"));

        // a console-only listing: handled entirely in the core, so it
        // is identical in both front-ends.
        {
            let mut core = h.ctx(&player);
            apply_op(Op::View(ViewTarget::Index), &mut core, &mut surf);
        }
        assert!(
            h.console.lines.iter().any(|l| l.text.contains("indexed")),
            "view(index) must print in the core: {:?}",
            h.console.lines
        );

        // a pane op: declined, not crashed and not silently dropped.
        {
            let mut core = h.ctx(&player);
            apply_op(Op::View(ViewTarget::Eq), &mut core, &mut surf);
        }
        let last = h.console.lines.last().expect("a line").clone();
        assert_eq!(
            last.kind,
            crate::shellsong::LineKind::Error,
            "a decline is a failure line: {last:?}"
        );
        assert!(
            last.text.contains("wim"),
            "and it says where the eq is: {}",
            last.text
        );

        // every op reports exactly one line (docs/shellsong.md R9),
        // declines included.
        let before = h.console.lines.len();
        {
            let mut core = h.ctx(&player);
            apply_op(Op::SelectTop, &mut core, &mut surf);
        }
        assert_eq!(
            h.console.lines.len(),
            before + 1,
            "one line per op, no more and no fewer"
        );
    }

    /// `Env` is built with no pane state either: `Headless` answers
    /// the only two readings it takes from a front-end. Needs no mpv,
    /// so this one is not ignored.
    #[test]
    fn the_env_needs_no_front_end_state() {
        let mut app = app::App::new(crate::search::Corpus::new());
        app.set_query("hex");
        let mut queue = PlayQueue::default();
        queue.reset(test_songs(2));
        let env = shellsong_env(&app, &queue, &Headless, true, Duration::from_secs(3));
        assert_eq!(env.query, "hex");
        assert_eq!(env.queue_len, 2);
        assert_eq!(env.focus, shellsong::Focus::Sidebar);
        assert!(!env.browsing, "a headless front-end is never browsing");
    }

    /// `dry()` describes every op in both binaries, including the ones
    /// a front-end would decline: it lowers to notes and never reaches
    /// the op table at all, so there is nothing to decline.
    #[test]
    fn dry_describes_even_what_a_front_end_would_refuse() {
        let mut program = Program::parse("dry(view(eq))").expect("parses");
        let env = shellsong_env(
            &app::App::new(crate::search::Corpus::new()),
            &PlayQueue::default(),
            &Headless,
            true,
            Duration::ZERO,
        );
        let mut notes = Vec::new();
        loop {
            match program.step(&env) {
                shellsong::Step::Note(t) => notes.push(t),
                shellsong::Step::Done => break,
                other => panic!("dry must only ever emit notes, got {other:?}"),
            }
        }
        let text = notes.join("\n");
        assert!(text.contains("view(eq)"), "{text}");
        assert!(text.contains("viewing eq"), "{text}");
    }

    /// `short_help` is derived from the language's own table, so a
    /// command added to shellsong shows up under `?` with nobody
    /// remembering to add it.
    #[test]
    fn the_short_help_lists_every_command_the_language_has() {
        let lines = short_help();
        let joined = lines.join(" ");
        for expected in ["play", "queue", "volume", "view", "dry", "quit"] {
            assert!(joined.contains(expected), "? must list {expected}");
        }
        let claimed: usize = lines[0]
            .split_whitespace()
            .next()
            .and_then(|n| n.parse().ok())
            .expect("a count on the first line");
        assert_eq!(
            claimed,
            shellsong::help_lines().len(),
            "the count must match the language's own table"
        );
        assert!(
            lines.iter().all(|l| l.chars().count() <= 80),
            "every row has to fit a narrow terminal: {lines:?}"
        );
    }

    /// Everything a `CoreCtx` needs that is not the mpv handle, owned,
    /// so a test can build one without a front-end in sight.
    struct Harness {
        cfg: config::Config,
        app: app::App,
        notifications: Notifications,
        queue: PlayQueue,
        previous_track: Option<SearchItem>,
        cache: Arc<Mutex<CacheIndex>>,
        download_tx: mpsc::Sender<DownloadEvent>,
        _download_rx: mpsc::Receiver<DownloadEvent>,
        downloading: HashSet<String>,
        scrobble: ScrobbleState,
        subtitle_tx: mpsc::Sender<SubtitleReady>,
        _subtitle_rx: mpsc::Receiver<SubtitleReady>,
        console: Console,
        now_playing: Option<SearchItem>,
        favorites: favorites::Favorites,
        remote_starred: HashSet<String>,
        lyrics_client: lyrics::Client,
        lyrics_loader: lyrics::Loader,
        star_tx: mpsc::Sender<StarFailed>,
        _star_rx: mpsc::Receiver<StarFailed>,
        base_volume: f64,
        crossfade_enabled: bool,
        fade_in_started_at: Option<Instant>,
        fade: Option<Ramp>,
        sleep_due: Option<Duration>,
        scan_rx: mpsc::Receiver<library::ScanBatch>,
        _scan_tx: mpsc::Sender<library::ScanBatch>,
        remote_search_tx: mpsc::Sender<RemoteSearchQuery>,
        _remote_search_rx: mpsc::Receiver<RemoteSearchQuery>,
        settle: SearchSettle,
        saved_queues: Vec<String>,
        vars: shellsong::Scope,
        bindings: shellsong_host::Bindings,
    }

    impl Harness {
        fn new() -> Harness {
            let (download_tx, _download_rx) = mpsc::channel();
            let (subtitle_tx, _subtitle_rx) = mpsc::channel();
            let (_scan_tx, scan_rx) = mpsc::channel();
            let (remote_search_tx, _remote_search_rx) = mpsc::channel();
            let (star_tx, star_rx) = mpsc::channel();
            Harness {
                cfg: config::Config::default(),
                app: app::App::new(crate::search::Corpus::new()),
                notifications: Notifications::default(),
                queue: PlayQueue::default(),
                previous_track: None,
                cache: Arc::new(Mutex::new(CacheIndex::default())),
                download_tx,
                _download_rx,
                downloading: HashSet::new(),
                scrobble: ScrobbleState::default(),
                subtitle_tx,
                _subtitle_rx,
                console: Console::empty(),
                now_playing: Some(test_songs(1).remove(0)),
                favorites: favorites::Favorites::default(),
                remote_starred: HashSet::new(),
                lyrics_client: lyrics::Client::new(),
                lyrics_loader: lyrics::Loader::new(),
                star_tx,
                _star_rx: star_rx,
                base_volume: 100.0,
                crossfade_enabled: false,
                fade_in_started_at: None,
                fade: None,
                sleep_due: None,
                scan_rx,
                _scan_tx,
                remote_search_tx,
                _remote_search_rx,
                settle: SearchSettle::default(),
                saved_queues: Vec::new(),
                vars: shellsong::Scope::default(),
                bindings: shellsong_host::Bindings::default(),
            }
        }

        fn ctx<'a>(&'a mut self, player: &'a mpv::Mpv) -> CoreCtx<'a> {
            CoreCtx {
                cfg: &self.cfg,
                player,
                app: &mut self.app,
                notifications: &mut self.notifications,
                queue: &mut self.queue,
                previous_track: &mut self.previous_track,
                cache: &self.cache,
                download_tx: &self.download_tx,
                downloading: &mut self.downloading,
                scrobble: &mut self.scrobble,
                subtitle_tx: &self.subtitle_tx,
                console: &mut self.console,
                now: Duration::from_secs(1),
                now_playing: &self.now_playing,
                favorites: &mut self.favorites,
                remote_starred: &mut self.remote_starred,
                lyrics_client: &self.lyrics_client,
                lyrics_loader: &mut self.lyrics_loader,
                star_tx: &self.star_tx,
                base_volume: &mut self.base_volume,
                crossfade_enabled: &mut self.crossfade_enabled,
                fade_in_started_at: &mut self.fade_in_started_at,
                fade: &mut self.fade,
                sleep_due: &mut self.sleep_due,
                scan_rx: &mut self.scan_rx,
                remote_search_tx: &self.remote_search_tx,
                settle: &mut self.settle,
                saved_queues: &mut self.saved_queues,
                vars: &mut self.vars,
                bindings: &mut self.bindings,
            }
        }
    }

    // ----------------------------------------------- optimistic stars

    /// The point of the change: the local set flips before any network
    /// call can have answered, so the next frame already shows it.
    /// This used to be a round trip on the keypress.
    #[test]
    fn starring_a_remote_song_flips_the_set_without_waiting() {
        let (star_tx, _star_rx) = mpsc::channel();
        let mut favorites = favorites::Favorites::default();
        let mut starred = HashSet::new();
        let item = test_songs(1).remove(0);
        let crate::search::SearchItem::Song { id, .. } = &item else {
            panic!("expected a song");
        };
        let id = id.clone();

        let now = toggle_star(&item, &mut favorites, &mut starred, &star_tx)
            .expect("a song can be starred");
        assert!(now, "it reports the new state at once");
        assert!(starred.contains(&id), "and the set already holds it");

        let now =
            toggle_star(&item, &mut favorites, &mut starred, &star_tx).expect("and unstarred");
        assert!(!now);
        assert!(!starred.contains(&id));
    }

    /// A refusal has to be reportable: the sender carries which way the
    /// flip went, because the id alone cannot say how to undo it.
    #[test]
    fn a_refusal_carries_enough_to_undo_the_flip() {
        let failed = StarFailed {
            id: "s1".into(),
            wanted: true,
            error: "502".into(),
        };
        let mut starred = HashSet::from(["s1".to_string()]);
        // what both binaries' drains do.
        if failed.wanted {
            starred.remove(&failed.id);
        } else {
            starred.insert(failed.id.clone());
        }
        assert!(!starred.contains("s1"), "a refused star is taken back");

        let failed = StarFailed {
            id: "s2".into(),
            wanted: false,
            error: "502".into(),
        };
        let mut starred: HashSet<String> = HashSet::new();
        if failed.wanted {
            starred.remove(&failed.id);
        } else {
            starred.insert(failed.id.clone());
        }
        assert!(starred.contains("s2"), "a refused unstar is put back");
    }

    /// Local files are a file write, not a round trip, so they stay
    /// synchronous and must keep working unchanged.
    #[test]
    fn a_local_file_still_toggles_synchronously() {
        let (star_tx, _star_rx) = mpsc::channel();
        let mut favorites = favorites::Favorites::default();
        let mut starred = HashSet::new();
        let item = crate::search::SearchItem::LocalMusic {
            path: std::path::PathBuf::from("/music/a.flac"),
            title: "a".into(),
            artist: None,
            album: None,
        };
        assert!(toggle_star(&item, &mut favorites, &mut starred, &star_tx).unwrap());
        assert!(starred.is_empty(), "a local star is not a remote one");
    }

    #[test]
    fn only_tracks_can_be_starred() {
        let (star_tx, _star_rx) = mpsc::channel();
        let mut favorites = favorites::Favorites::default();
        let mut starred = HashSet::new();
        let item = crate::search::SearchItem::Playlist {
            id: "p1".into(),
            name: "mix".into(),
            client: crate::browse::tests::test_client(),
        };
        assert!(toggle_star(&item, &mut favorites, &mut starred, &star_tx).is_err());
    }
}
