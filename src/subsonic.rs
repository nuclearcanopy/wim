//! Hand-rolled Subsonic API client (targets Navidrome). The handful of
//! existing subsonic crates are unmaintained/incomplete, and the surface
//! we need (ping, catalog browsing, playlists, stream urls) is small enough that
//! owning it directly is simpler than fighting someone else's types.

use std::collections::HashMap;
use std::io::Read;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread;
use std::time::{Duration, Instant};

use md5::{Digest, Md5};
use rand::Rng;
use serde::Deserialize;
use zeroize::Zeroizing;

use crate::config::NavidromeConfig;

const API_VERSION: &str = "1.16.1";
const CLIENT_NAME: &str = "wim";

/// ureq's own default is 1 idle connection per host, so a fan-out of
/// concurrent requests pools exactly one of them and every later call
/// pays a fresh tcp+tls handshake; this keeps the whole artist-page
/// fan-out (plus cover art and search) on warm, reused connections.
const MAX_IDLE_CONNECTIONS_PER_HOST: usize = 16;

/// How many `getAlbum` requests an artist fetch runs at once. Subsonic
/// has no single call for every song by an artist (see
/// `artist_albums_with_tracks`), so it is one request per album; 8
/// hides nearly all the latency without opening a socket per album.
const ALBUM_FETCH_CONCURRENCY: usize = 8;

/// `getAlbumList2`'s per-page cap per the subsonic spec.
const ALBUM_PAGE_SIZE: usize = 500;

/// How many catalog pages `all_albums` has in flight at once. Lower
/// than `ALBUM_FETCH_CONCURRENCY` because each page is 500 albums of
/// json, and because every launch pays this against every configured
/// server at the same time.
const ALBUM_PAGE_CONCURRENCY: usize = 4;

/// How long a cached navigation answer (a tracklist, an album's year,
/// an artist's album list, a bio) may be served from memory. Pinned to
/// the background catalog sync's own interval, so nothing a server
/// changes is ever more than one sync cycle stale; see
/// docs/startup-cache.md.
const NAV_CACHE_TTL: Duration = Duration::from_secs(45);

/// Cached navigation answers per server. 256 covers any plausible
/// browsing session (an album tracklist each) and bounds what a day-
/// long session can hold.
const NAV_CACHE_CAPACITY: usize = 256;

/// One cached response, with when it was fetched.
struct CachedResponse {
    at: Instant,
    value: serde_json::Value,
}

/// Request-keyed navigation responses plus the catalog fingerprints
/// that invalidate them. Insertion-ordered keys so the cap evicts the
/// oldest entry rather than an arbitrary one.
#[derive(Default)]
struct NavCache {
    entries: HashMap<String, CachedResponse>,
    order: Vec<String>,
    /// Per catalog endpoint, a digest of the ids it last answered with
    /// (see `catalog_fingerprint`). A change means the server's
    /// catalog moved under us, so every cached tracklist for it is
    /// suspect and goes immediately rather than waiting out its ttl.
    fingerprints: HashMap<&'static str, String>,
}

impl NavCache {
    fn get(&self, key: &str) -> Option<serde_json::Value> {
        let hit = self.entries.get(key)?;
        (hit.at.elapsed() < NAV_CACHE_TTL).then(|| hit.value.clone())
    }

    fn put(&mut self, key: String, value: serde_json::Value) {
        self.order.retain(|k| k != &key);
        self.order.push(key.clone());
        self.entries.insert(
            key,
            CachedResponse {
                at: Instant::now(),
                value,
            },
        );
        while self.order.len() > NAV_CACHE_CAPACITY {
            let oldest = self.order.remove(0);
            self.entries.remove(&oldest);
        }
    }

    fn clear(&mut self) {
        self.entries.clear();
        self.order.clear();
    }
}

/// Navigation caches keyed by `username@base_url`, so every `Client`
/// built for one server shares one: the catalog sync builds a fresh
/// client per pass, and it is the thing that knows the catalog
/// changed, while the clients the corpus hands to the ui live for the
/// whole session. Holds parsed responses and id digests only, never a
/// credential (see docs/security.md).
static NAV_CACHES: OnceLock<Mutex<HashMap<String, Arc<Mutex<NavCache>>>>> = OnceLock::new();

fn nav_cache_for(username: &str, base_url: &str) -> Arc<Mutex<NavCache>> {
    let registry = NAV_CACHES.get_or_init(|| Mutex::new(HashMap::new()));
    let key = format!("{username}@{base_url}");
    let Ok(mut registry) = registry.lock() else {
        // a poisoned registry means some thread panicked holding it;
        // an unshared cache is still correct, just not shared.
        return Arc::new(Mutex::new(NavCache::default()));
    };
    registry.entry(key).or_default().clone()
}

/// Order-independent digest of whatever ids a catalog endpoint
/// answered with: the cheapest "did this server's catalog change"
/// signal available without the server telling us.
fn catalog_fingerprint(ids: impl Iterator<Item = impl AsRef<str>>) -> String {
    let mut sorted: Vec<String> = ids.map(|id| id.as_ref().to_string()).collect();
    sorted.sort_unstable();
    let mut hasher = Md5::new();
    for id in sorted {
        hasher.update(id.as_bytes());
        hasher.update(b"\n");
    }
    format!("{:x}", hasher.finalize())
}

/// Cheap to clone (ureq's `Agent` is `Arc`-backed internally); every
/// remote `SearchItem` carries an `Arc<Client>` so playback can later
/// resolve to the right server's stream url without the app needing a
/// separate "active server" concept to track.
#[derive(Clone)]
pub struct Client {
    base_url: String,
    username: String,
    /// Wiped on drop. Subsonic auth is `md5(password + salt)` with a
    /// fresh salt per request, so the cleartext has to stay resident
    /// for the whole session; zeroizing at least keeps it out of
    /// freed heap pages, core dumps, and swap afterwards. It is still
    /// cleartext in config.toml (0600) and passes through the raw
    /// TOML string on load, so this narrows the window, not the
    /// storage; see docs/security.md.
    password: Zeroizing<String>,
    agent: ureq::Agent,
    /// Client-side `stream` overrides; `None` means "let the server's
    /// own configured transcoding profile decide," the default.
    max_bitrate_kbps: Option<u32>,
    transcode_format: Option<String>,
    /// Shared with every other `Client` for the same server; see
    /// `NAV_CACHES`.
    nav_cache: Arc<Mutex<NavCache>>,
}

impl std::fmt::Debug for Client {
    /// Hand-written so the password never ends up in a `{:?}` log line.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Client")
            .field("base_url", &self.base_url)
            .field("username", &self.username)
            .finish()
    }
}

#[derive(Debug, Clone)]
pub struct Artist {
    pub id: String,
    pub name: String,
}

#[derive(Debug, Clone)]
pub struct Album {
    pub id: String,
    pub name: String,
    pub artist: String,
}

#[derive(Debug, Clone)]
pub struct Song {
    pub id: String,
    pub title: String,
    pub artist: String,
    pub album: String,
    // parsed for fidelity to the api response but not read anywhere
    // yet; same documented gap as LocalTrack::duration_secs (see
    // library.rs): SearchItem::Song doesn't carry duration either.
    #[allow(dead_code)]
    pub duration: Option<u32>,
    /// Every genre tag the server has for this track, in server order
    /// (empty when untagged). Navidrome reports both a legacy single
    /// `genre` string and a multi-valued `genres` array; see
    /// `parse_genres`. The only genre source the suggestion engine
    /// has, so it is as good as the server's tags and no better.
    pub genres: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct Playlist {
    pub id: String,
    pub name: String,
}

#[derive(Debug, Clone)]
pub struct AlbumDetails {
    pub year: Option<u32>,
    pub songs: Vec<Song>,
}

#[derive(Debug, Clone)]
pub struct ArtistAlbum {
    pub id: String,
    /// `getArtist` already carries every album name, so the artist
    /// view's ALBUMS column renders off one round trip instead of
    /// waiting on a `getAlbum` per album to learn the names.
    pub name: String,
    /// `None` sorts last rather than first; an unknown release date
    /// shouldn't jump to the front of a chronological listing.
    pub year: Option<u32>,
}

/// One album's tracks from an artist fetch. `index` is the album's
/// slot in the chronological album list it came from, because parallel
/// fetches complete out of order.
#[derive(Debug, Clone)]
pub struct ArtistAlbumTracks {
    pub index: usize,
    /// Which album these songs are from. `browse::Loader` keys off
    /// `index` into the album list it already has, so nothing reads
    /// this yet; it stays because an event that says only "some
    /// album's songs" is a worse thing to hand a consumer.
    #[allow(dead_code)]
    pub album: ArtistAlbum,
    pub songs: Vec<Song>,
}

/// Progress events from `stream_artist_tracks`, in arrival order.
#[derive(Debug, Clone)]
pub enum ArtistLoad {
    /// Best-effort, `None` when the server has no biography.
    Bio(Option<String>),
    Album(ArtistAlbumTracks),
}

/// `getArtistInfo2`, both halves. The `similarArtist` list is
/// last.fm's own similarity graph proxied by the server, which is the
/// only cross-artist signal available without any collaborative data
/// of our own; see docs/suggestions.md.
#[derive(Debug, Default, Clone)]
pub struct ArtistInfo {
    pub biography: Option<String>,
    /// Ranked most-similar-first by last.fm. Entries the server
    /// doesn't have in the library come back with no usable id (or
    /// `"-1"`), so consumers match on `name` and intersect against
    /// what is actually indexed rather than trusting the id.
    pub similar_artists: Vec<Artist>,
}

#[derive(Debug, Default, Clone)]
pub struct SearchResult {
    pub artists: Vec<Artist>,
    pub albums: Vec<Album>,
    pub songs: Vec<Song>,
}

impl Client {
    /// For grouping corpus items by which server they came from (the
    /// on-demand index breakdown); not used for any request itself.
    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    /// Half of the identity of a persisted catalog (`cache.rs`): two
    /// accounts on one server don't see the same playlists, so the
    /// username is part of which catalog is whose.
    pub fn username(&self) -> &str {
        &self.username
    }

    pub fn new(cfg: &NavidromeConfig) -> anyhow::Result<Self> {
        let base_url = normalize_url(&cfg.url);
        Ok(Client {
            nav_cache: nav_cache_for(&cfg.username, &base_url),
            base_url,
            username: cfg.username.clone(),
            password: Zeroizing::new(cfg.resolve_password()?),
            agent: ureq::AgentBuilder::new()
                .max_idle_connections_per_host(MAX_IDLE_CONNECTIONS_PER_HOST)
                .build(),
            max_bitrate_kbps: cfg.max_bitrate_kbps,
            transcode_format: cfg.transcode_format.clone(),
        })
    }

    /// Whether this client is already the one `cfg` describes, so a
    /// caller holding live clients can reuse one instead of building
    /// another. `Client::new` re-reads `password_file` off disk and
    /// starts with a cold connection pool, which is the whole 1.94s to
    /// 375ms difference this codebase already paid for once.
    ///
    /// Stream options are part of identity, not just username and url:
    /// a client carries `max_bitrate_kbps` and `transcode_format` into
    /// every stream url it builds, and `open_starred_view`'s items get
    /// played. Matching on identity alone would let a client built
    /// before a bitrate change go on streaming at the old setting.
    pub fn matches(&self, cfg: &NavidromeConfig) -> bool {
        self.base_url == normalize_url(&cfg.url)
            && self.username == cfg.username
            && self.max_bitrate_kbps == cfg.max_bitrate_kbps
            && self.transcode_format == cfg.transcode_format
    }

    /// Subsonic token auth: `t = md5(password + salt)`, random `s` per
    /// request, so the plaintext password never goes over the wire (only
    /// matters if the server isn't behind TLS; harmless either way).
    fn auth_params(&self) -> Vec<(String, String)> {
        let salt: String = {
            let mut rng = rand::thread_rng();
            (0..12)
                .map(|_| rng.sample(rand::distributions::Alphanumeric) as char)
                .collect()
        };
        let mut hasher = Md5::new();
        hasher.update(self.password.as_bytes());
        hasher.update(salt.as_bytes());
        let token = format!("{:x}", hasher.finalize());

        vec![
            ("u".into(), self.username.clone()),
            ("t".into(), token),
            ("s".into(), salt),
            ("v".into(), API_VERSION.into()),
            ("c".into(), CLIENT_NAME.into()),
            ("f".into(), "json".into()),
        ]
    }

    fn get(&self, endpoint: &str, extra: &[(&str, &str)]) -> anyhow::Result<serde_json::Value> {
        let url = format!("{}/rest/{endpoint}", self.base_url);
        let mut req = self.agent.get(&url);
        for (k, v) in self.auth_params() {
            req = req.query(&k, &v);
        }
        for (k, v) in extra {
            req = req.query(k, v);
        }
        let body: SubsonicEnvelope = req.call()?.into_json()?;
        if body.subsonic_response.status != "ok" {
            let msg = body
                .subsonic_response
                .error
                .map(|e| e.message)
                .unwrap_or_else(|| "unknown subsonic error".into());
            anyhow::bail!("subsonic error: {msg}");
        }
        Ok(serde_json::to_value(body.subsonic_response)?)
    }

    /// `get` for the listings a navigation hit is made of, served from
    /// memory while an answer younger than `NAV_CACHE_TTL` is around.
    /// Only ever used for idempotent reads whose answer is the same
    /// for every caller; the catalog endpoints deliberately stay
    /// uncached, since they are the invalidation signal and the thing
    /// asking for them is the sync that wants fresh data.
    fn get_cached(
        &self,
        endpoint: &str,
        extra: &[(&str, &str)],
    ) -> anyhow::Result<serde_json::Value> {
        let key = extra.iter().fold(endpoint.to_string(), |mut key, (k, v)| {
            key.push('\u{1f}');
            key.push_str(k);
            key.push('=');
            key.push_str(v);
            key
        });
        if let Some(hit) = self.nav_cache.lock().ok().and_then(|c| c.get(&key)) {
            return Ok(hit);
        }
        let value = self.get(endpoint, extra)?;
        if let Ok(mut cache) = self.nav_cache.lock() {
            cache.put(key, value.clone());
        }
        Ok(value)
    }

    /// Drops every cached navigation answer for this server when
    /// `ids` differ from what the same endpoint answered last time.
    /// Called from the catalog endpoints, which is what makes the
    /// background sync the invalidation path rather than the ttl
    /// alone.
    fn invalidate_on_catalog_change(
        &self,
        endpoint: &'static str,
        ids: impl Iterator<Item = impl AsRef<str>>,
    ) {
        let fingerprint = catalog_fingerprint(ids);
        let Ok(mut cache) = self.nav_cache.lock() else {
            return;
        };
        match cache.fingerprints.insert(endpoint, fingerprint.clone()) {
            // first catalog fetch of the process: nothing cached yet
            // from before it, so there is nothing to drop.
            None => {}
            Some(previous) if previous == fingerprint => {}
            Some(_) => cache.clear(),
        }
    }

    pub fn ping(&self) -> anyhow::Result<()> {
        self.get("ping", &[])?;
        Ok(())
    }

    /// Every track on an album, in album order, plus the album's own
    /// release year (shown in the album view; `getAlbumList2` doesn't
    /// carry it, only `getAlbum` does); used both to expand a playable
    /// album into its actual tracks and to populate the track-listing
    /// view when you open one.
    pub fn get_album(&self, album_id: &str) -> anyhow::Result<AlbumDetails> {
        let v = self.get_cached("getAlbum", &[("id", album_id)])?;
        let album = v.get("album");
        Ok(AlbumDetails {
            year: album
                .and_then(|a| a.get("year"))
                .and_then(|y| y.as_u64())
                .map(|y| y as u32),
            songs: parse_song_array(album.and_then(|a| a.get("song"))),
        })
    }

    /// Every album credited to this artist (ID3 browsing), for the
    /// artist profile view's chronological album list. Doesn't include
    /// songs; see `get_album` per-album for those (and the Global/
    /// Various-Artists filtering that requires).
    pub fn get_artist(&self, artist_id: &str) -> anyhow::Result<Vec<ArtistAlbum>> {
        let v = self.get_cached("getArtist", &[("id", artist_id)])?;
        Ok(parse_artist_albums(&v))
    }

    /// Navidrome's own `getArtistInfo2` already proxies last.fm, no
    /// separate api key needed: one call carries both the biography and
    /// a ranked `similarArtist` list. Empty fields rather than an error
    /// when the server has nothing (unknown artist, no internet
    /// metadata lookup configured, etc.).
    pub fn get_artist_info(&self, artist_id: &str) -> anyhow::Result<ArtistInfo> {
        let v = self.get_cached("getArtistInfo2", &[("id", artist_id)])?;
        Ok(parse_artist_info(&v))
    }

    /// Just the biography half of `get_artist_info`, for the artist
    /// page's bio pane.
    pub fn get_artist_bio(&self, artist_id: &str) -> anyhow::Result<Option<String>> {
        Ok(self.get_artist_info(artist_id)?.biography)
    }

    /// Same idea for playlists: `getPlaylist`'s tracks live under
    /// `entry`, not `song`, but the per-track shape is identical.
    /// Playlists have no release year, so no metadata wrapper needed.
    pub fn get_playlist_songs(&self, playlist_id: &str) -> anyhow::Result<Vec<Song>> {
        let v = self.get_cached("getPlaylist", &[("id", playlist_id)])?;
        Ok(parse_song_array(
            v.get("playlist").and_then(|p| p.get("entry")),
        ))
    }

    pub fn star(&self, id: &str) -> anyhow::Result<()> {
        self.get("star", &[("id", id)])?;
        Ok(())
    }

    pub fn unstar(&self, id: &str) -> anyhow::Result<()> {
        self.get("unstar", &[("id", id)])?;
        Ok(())
    }

    /// Every song this server has starred; used both to know which
    /// rows should show the star marker and to populate the "starred
    /// songs" browse view (which behaves like any other track
    /// listing, just sourced from here instead of one album).
    pub fn get_starred_songs(&self) -> anyhow::Result<Vec<Song>> {
        let v = self.get("getStarred2", &[])?;
        Ok(parse_song_array(
            v.get("starred2").and_then(|s| s.get("song")),
        ))
    }

    pub fn playlists(&self) -> anyhow::Result<Vec<Playlist>> {
        let v = self.get("getPlaylists", &[])?;
        let list: Vec<Playlist> = v
            .get("playlists")
            .and_then(|p| p.get("playlist"))
            .and_then(|p| p.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|p| {
                        Some(Playlist {
                            id: p.get("id")?.as_str()?.to_string(),
                            name: p.get("name")?.as_str()?.to_string(),
                        })
                    })
                    .collect()
            })
            .unwrap_or_default();
        self.invalidate_on_catalog_change("getPlaylists", list.iter().map(|p| &p.id));
        Ok(list)
    }

    /// The entire artist catalog in one call; `getArtists` (ID3
    /// browsing) returns everything grouped alphabetically by index
    /// letter, unlike album/song listing which is paginated. Matches
    /// how real clients (e.g. Feishin) index artists: flatten every
    /// index's `artist` array into one flat list.
    pub fn all_artists(&self) -> anyhow::Result<Vec<Artist>> {
        let v = self.get("getArtists", &[])?;
        let artists = parse_artists_index(&v);
        self.invalidate_on_catalog_change("getArtists", artists.iter().map(|a| &a.id));
        Ok(artists)
    }

    /// The entire album catalog, paginated via `getAlbumList2`
    /// (`offset`/`size`); unlike artists, album listing has no
    /// single-call "everything" mode, so this runs until a page
    /// returns fewer than requested (the standard termination
    /// condition; Navidrome also reports `x-total-count` but that
    /// requires reading response headers, which ureq's `into_json()`
    /// convenience here doesn't expose; the fewer-than-requested check
    /// is sufficient and matches what the spec guarantees).
    ///
    /// Pages after the first are independent requests at known
    /// offsets, so they go out `ALBUM_PAGE_CONCURRENCY` at a time
    /// instead of one round trip per page: a 5k-album library was 11
    /// stacked round trips. The cost is a handful of requests past the
    /// end of the catalog (one short of a full wave, worst case),
    /// which answer empty; only libraries over one page pay it at all.
    pub fn all_albums(&self) -> anyhow::Result<Vec<Album>> {
        let albums = self.all_album_pages()?;
        self.invalidate_on_catalog_change("getAlbumList2", albums.iter().map(|a| &a.id));
        Ok(albums)
    }

    fn all_album_pages(&self) -> anyhow::Result<Vec<Album>> {
        let mut albums = self.album_page(0)?;
        if albums.len() < ALBUM_PAGE_SIZE {
            return Ok(albums);
        }
        let mut offset = ALBUM_PAGE_SIZE;
        loop {
            let wave: Vec<anyhow::Result<Vec<Album>>> = thread::scope(|scope| {
                (0..ALBUM_PAGE_CONCURRENCY)
                    .map(|i| scope.spawn(move || self.album_page(offset + i * ALBUM_PAGE_SIZE)))
                    .collect::<Vec<_>>()
                    .into_iter()
                    .map(|handle| {
                        handle
                            .join()
                            .unwrap_or_else(|_| anyhow::bail!("panicked fetching an album page"))
                    })
                    .collect()
            });
            for page in wave {
                let page = page?;
                let got = page.len();
                albums.extend(page);
                // a short page is the end of the catalog, so every
                // later offset in this wave is past it; whatever they
                // answered is dropped rather than appended out of
                // order (the next sync picks up anything added since).
                if got < ALBUM_PAGE_SIZE {
                    return Ok(albums);
                }
            }
            offset += ALBUM_PAGE_SIZE * ALBUM_PAGE_CONCURRENCY;
        }
    }

    fn album_page(&self, offset: usize) -> anyhow::Result<Vec<Album>> {
        let v = self.get(
            "getAlbumList2",
            &[
                ("type", "alphabeticalByName"),
                ("size", &ALBUM_PAGE_SIZE.to_string()),
                ("offset", &offset.to_string()),
            ],
        )?;
        Ok(parse_album_list2_page(&v))
    }

    /// Stream url for a song id. Asks for `format=raw` (the subsonic
    /// idiom for "don't transcode") unless the config overrides format
    /// or bitrate, so playback is the original file rather than
    /// whatever profile the server happens to have on this player; see
    /// docs/audio-quality.md.
    pub fn stream_url(&self, song_id: &str) -> String {
        let salt: String = {
            let mut rng = rand::thread_rng();
            (0..12)
                .map(|_| rng.sample(rand::distributions::Alphanumeric) as char)
                .collect()
        };
        let mut hasher = Md5::new();
        hasher.update(self.password.as_bytes());
        hasher.update(salt.as_bytes());
        let token = format!("{:x}", hasher.finalize());

        let mut url = format!(
            "{}/rest/stream?id={}&u={}&t={}&s={}&v={}&c={}",
            self.base_url, song_id, self.username, token, salt, API_VERSION, CLIENT_NAME
        );
        if let Some(kbps) = self.max_bitrate_kbps {
            url.push_str(&format!("&maxBitRate={kbps}"));
        }
        match &self.transcode_format {
            Some(format) => url.push_str(&format!("&format={format}")),
            // a bitrate cap only means anything if the server may
            // re-encode, so raw is only safe to force without one.
            None if self.max_bitrate_kbps.is_none() => url.push_str("&format=raw"),
            None => {}
        }
        url
    }

    /// Live, on-demand search via Subsonic's `search3` (ID3-based):
    /// the counterpart to `getArtists`/`getAlbumList2`'s eager, up-
    /// front indexing (see `fetch_navidrome_catalog`'s own comment in
    /// main.rs for why songs specifically are *not* indexed that way).
    /// Covers whatever an eager index misses, most importantly
    /// individual song titles, without ever pulling the whole catalog
    /// of songs up front. Same per-entity shapes as `getArtist`/
    /// `getAlbum`/`getPlaylist`'s track lists, just all three under one
    /// `searchResult3` envelope instead of one call per entity type.
    pub fn search3(&self, query: &str) -> anyhow::Result<SearchResult> {
        let v = self.get(
            "search3",
            &[
                ("query", query),
                ("artistCount", "20"),
                ("albumCount", "20"),
                ("songCount", "40"),
            ],
        )?;
        Ok(parse_search3_response(&v))
    }

    /// Raw image bytes for an album's (or song's) cover art; navidrome
    /// accepts the album id directly as the `getCoverArt` id in the
    /// common case. Not JSON, so this bypasses `get()` and reads the
    /// response body straight off the connection.
    pub fn get_cover_art_bytes(&self, cover_art_id: &str) -> anyhow::Result<Vec<u8>> {
        let url = format!("{}/rest/getCoverArt", self.base_url);
        let mut req = self.agent.get(&url);
        for (k, v) in self.auth_params() {
            req = req.query(&k, &v);
        }
        req = req.query("id", cover_art_id);
        let mut bytes = Vec::new();
        req.call()?.into_reader().read_to_end(&mut bytes)?;
        Ok(bytes)
    }
}

/// The artist-page data path: `stream_artist_tracks` is what the
/// artist view opens on (see `browse::Loader`), the rest is what it
/// is built out of.
impl Client {
    /// Every song on every album credited to this artist, grouped by
    /// album, chronological. Subsonic has no single call for this:
    /// `getArtist` returns albums without songs, `getAlbumList2` can't
    /// filter by artist, and `getTopSongs` is a last.fm-ranked subset,
    /// so it stays one `getAlbum` per album; those are independent, so
    /// they run `ALBUM_FETCH_CONCURRENCY` at a time rather than
    /// serially. Callers wanting only the artist's own tracks filter on
    /// each `Song::artist` (a compilation's other credits land here
    /// too, same as before).
    /// The serial reference the streaming path is checked against
    /// (see this module's tests); nothing in the app calls it.
    #[allow(dead_code)]
    pub fn artist_albums_with_tracks(
        &self,
        artist_id: &str,
    ) -> anyhow::Result<Vec<ArtistAlbumTracks>> {
        let albums = self.artist_albums_chronological(artist_id)?;
        let ids: Vec<&str> = albums.iter().map(|a| a.id.as_str()).collect();
        Ok(self
            .album_details(&ids)
            .into_iter()
            .zip(albums)
            .enumerate()
            .filter_map(|(index, (details, album))| {
                Some(ArtistAlbumTracks {
                    index,
                    album,
                    songs: details?.songs,
                })
            })
            .collect())
    }

    /// `getAlbum` for many album ids at once, answers in the same
    /// order as `album_ids` (`None` where that album's fetch failed;
    /// one unreadable album shouldn't take down a whole artist page).
    ///
    /// The one bounded fan-out for album fetches: a worker per slot
    /// would be a socket per album, which against a real Navidrome
    /// instance means an artist with 60 releases opening 60
    /// connections. Workers pull from a shared counter rather than
    /// striping a fixed slice each, since album sizes vary a lot.
    pub fn album_details(&self, album_ids: &[&str]) -> Vec<Option<AlbumDetails>> {
        let workers = ALBUM_FETCH_CONCURRENCY.min(album_ids.len());
        if workers == 0 {
            return Vec::new();
        }
        let next = AtomicUsize::new(0);
        let done: Vec<Vec<(usize, AlbumDetails)>> = thread::scope(|scope| {
            (0..workers)
                .map(|_| {
                    let next = &next;
                    scope.spawn(move || {
                        let mut mine = Vec::new();
                        loop {
                            let index = next.fetch_add(1, Ordering::Relaxed);
                            let Some(id) = album_ids.get(index) else {
                                return mine;
                            };
                            if let Ok(details) = self.get_album(id) {
                                mine.push((index, details));
                            }
                        }
                    })
                })
                .collect::<Vec<_>>()
                .into_iter()
                .map(|handle| handle.join().unwrap_or_default())
                .collect()
        });
        let mut slots: Vec<Option<AlbumDetails>> = (0..album_ids.len()).map(|_| None).collect();
        for (index, details) in done.into_iter().flatten() {
            if let Some(slot) = slots.get_mut(index) {
                *slot = Some(details);
            }
        }
        slots
    }

    /// Non-blocking counterpart for the artist view: one `getArtist`
    /// round trip, then the album list comes straight back so the page
    /// can render immediately while everything else streams over `tx`.
    ///
    /// Drive it with `try_recv` from the event loop. Each
    /// `ArtistLoad::Album` carries one album's tracks and its `index`
    /// into the returned list (completions arrive out of order);
    /// `ArtistLoad::Bio` arrives at most once. `Disconnected` means
    /// every fetch finished. Dropping the receiver (closing the view)
    /// stops the workers at their next send, so a navigate-away costs
    /// at most one more in-flight request per worker.
    pub fn stream_artist_tracks(
        &self,
        artist_id: &str,
        tx: std::sync::mpsc::Sender<ArtistLoad>,
    ) -> anyhow::Result<Vec<ArtistAlbum>> {
        let albums = self.artist_albums_chronological(artist_id)?;
        let workers = ALBUM_FETCH_CONCURRENCY.min(albums.len());
        for worker in 0..workers {
            let client = self.clone();
            let tx = tx.clone();
            let stripe: Vec<(usize, ArtistAlbum)> = albums
                .iter()
                .cloned()
                .enumerate()
                .skip(worker)
                .step_by(workers)
                .collect();
            thread::spawn(move || {
                for (index, album) in stripe {
                    let Some(tracks) = client.album_tracks(index, &album) else {
                        continue;
                    };
                    if tx.send(ArtistLoad::Album(tracks)).is_err() {
                        return;
                    }
                }
            });
        }
        // navidrome proxies the bio from last.fm, which is slow enough
        // on its own to be worth taking off the open path; it rides the
        // same channel so the caller has one thing to poll.
        let client = self.clone();
        let bio_artist = artist_id.to_string();
        thread::spawn(move || {
            let bio = client.get_artist_bio(&bio_artist).ok().flatten();
            let _ = tx.send(ArtistLoad::Bio(bio));
        });
        Ok(albums)
    }

    /// `getTopSongs`: the artist's most-played tracks per last.fm,
    /// resolved by the server to tracks it actually has, ranked
    /// most-popular-first. Keyed by artist *name*, not id, because
    /// that is what the endpoint takes. Powers the "famous track by an
    /// artist you love that you have never played" suggestion, which
    /// plain play-coverage can't rank on its own.
    pub fn get_top_songs(&self, artist_name: &str, count: usize) -> anyhow::Result<Vec<Song>> {
        let v = self.get_cached(
            "getTopSongs",
            &[("artist", artist_name), ("count", &count.to_string())],
        )?;
        Ok(parse_song_array(
            v.get("topSongs").and_then(|t| t.get("song")),
        ))
    }

    /// Chronological, not whatever order `getArtist` returned
    /// (alphabetical); unknown-year albums sort last rather than first.
    fn artist_albums_chronological(&self, artist_id: &str) -> anyhow::Result<Vec<ArtistAlbum>> {
        let mut albums = self.get_artist(artist_id)?;
        albums.sort_by_key(|a| a.year.unwrap_or(u32::MAX));
        Ok(albums)
    }

    /// `None` on any per-album failure: one unreadable album shouldn't
    /// take down a whole artist page.
    fn album_tracks(&self, index: usize, album: &ArtistAlbum) -> Option<ArtistAlbumTracks> {
        let details = self.get_album(&album.id).ok()?;
        Some(ArtistAlbumTracks {
            index,
            album: album.clone(),
            songs: details.songs,
        })
    }
}

#[derive(Debug, Deserialize)]
struct SubsonicEnvelope {
    #[serde(rename = "subsonic-response")]
    subsonic_response: SubsonicResponse,
}

#[derive(Debug, Deserialize, serde::Serialize)]
struct SubsonicResponse {
    status: String,
    #[serde(flatten)]
    rest: serde_json::Value,
    #[serde(skip_serializing)]
    error: Option<SubsonicError>,
}

#[derive(Debug, Deserialize)]
struct SubsonicError {
    message: String,
}

/// So typing a bare `myserver.com` (no scheme) in the login prompt
/// actually works instead of silently failing to connect; defaults to
/// https, since that's the sane assumption for anything reachable from
/// outside localhost. Trailing slashes are trimmed either way.
pub(crate) fn normalize_url(url: &str) -> String {
    let trimmed = url.trim().trim_end_matches('/');
    if trimmed.starts_with("http://") || trimmed.starts_with("https://") {
        trimmed.to_string()
    } else {
        format!("https://{trimmed}")
    }
}

/// Pure parsing, pulled out of `all_artists()` so it's testable against
/// a realistic fixture without a live server. `getArtists` groups
/// results by alphabetical index letter; flatten every index's
/// `artist` array into one flat list, same as Feishin's own subsonic
/// controller does.
fn parse_artists_index(v: &serde_json::Value) -> Vec<Artist> {
    v.get("artists")
        .and_then(|a| a.get("index"))
        .and_then(|i| i.as_array())
        .map(|indices| {
            indices
                .iter()
                .filter_map(|idx| idx.get("artist").and_then(|a| a.as_array()))
                .flatten()
                .filter_map(|a| {
                    Some(Artist {
                        id: a.get("id")?.as_str()?.to_string(),
                        name: a.get("name")?.as_str()?.to_string(),
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Pure parsing shared by `search3`, `getAlbum`'s track list, and
/// `getPlaylist`'s track list; all three return the same per-track
/// shape, just nested under a different parent key.
fn parse_song_array(arr: Option<&serde_json::Value>) -> Vec<Song> {
    arr.and_then(|a| a.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|s| {
                    Some(Song {
                        id: s.get("id")?.as_str()?.to_string(),
                        title: s.get("title")?.as_str()?.to_string(),
                        artist: s
                            .get("artist")
                            .and_then(|v| v.as_str())
                            .unwrap_or_default()
                            .to_string(),
                        album: s
                            .get("album")
                            .and_then(|v| v.as_str())
                            .unwrap_or_default()
                            .to_string(),
                        duration: s.get("duration").and_then(|v| v.as_u64()).map(|d| d as u32),
                        genres: parse_genres(s),
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Pure parsing for `getArtistInfo2`. `similarArtist` entries that
/// aren't in the library carry `"-1"` (the subsonic "no such id"
/// convention) or no id at all; those are kept with an empty id since
/// the name alone is still enough to intersect against the corpus.
fn parse_artist_info(v: &serde_json::Value) -> ArtistInfo {
    let info = v.get("artistInfo2");
    ArtistInfo {
        biography: info
            .and_then(|a| a.get("biography"))
            .and_then(|b| b.as_str())
            .map(str::to_string)
            .filter(|s| !s.is_empty()),
        similar_artists: info
            .and_then(|a| a.get("similarArtist"))
            .and_then(|a| a.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|a| {
                        let name = a.get("name")?.as_str()?.to_string();
                        let id = a
                            .get("id")
                            .and_then(|i| i.as_str())
                            .filter(|i| *i != "-1")
                            .unwrap_or_default()
                            .to_string();
                        (!name.is_empty()).then_some(Artist { id, name })
                    })
                    .collect()
            })
            .unwrap_or_default(),
    }
}

/// Genre tags off one song object. Navidrome serves both shapes: the
/// legacy single `genre` string every subsonic client reads, and a
/// multi-valued `genres: [{name}]` array. Prefer the array (a track
/// tagged "Hip-Hop; Cloud Rap" is two signals, not one) and fall back
/// to the string, so a server that only sends one still works.
fn parse_genres(song: &serde_json::Value) -> Vec<String> {
    let multi: Vec<String> = song
        .get("genres")
        .and_then(|g| g.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|g| g.get("name").and_then(|n| n.as_str()))
                .filter(|n| !n.is_empty())
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();
    if !multi.is_empty() {
        return multi;
    }
    song.get("genre")
        .and_then(|g| g.as_str())
        .filter(|g| !g.is_empty())
        .map(|g| vec![g.to_string()])
        .unwrap_or_default()
}

/// Pure parsing for `getArtist`'s nested album list, pulled out of
/// `get_artist()` so it's testable in isolation.
fn parse_artist_albums(v: &serde_json::Value) -> Vec<ArtistAlbum> {
    v.get("artist")
        .and_then(|a| a.get("album"))
        .and_then(|a| a.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|a| {
                    Some(ArtistAlbum {
                        id: a.get("id")?.as_str()?.to_string(),
                        name: a
                            .get("name")
                            .and_then(|n| n.as_str())
                            .unwrap_or_default()
                            .to_string(),
                        year: a.get("year").and_then(|y| y.as_u64()).map(|y| y as u32),
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Pure parsing for a flat array of ID3 album objects (`id`/`name`/
/// `artist`), shared by `getAlbumList2`'s page parsing and `search3`'s
/// `album` array, which use the identical per-album shape.
fn parse_album_array(arr: Option<&serde_json::Value>) -> Vec<Album> {
    arr.and_then(|a| a.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|a| {
                    Some(Album {
                        id: a.get("id")?.as_str()?.to_string(),
                        name: a.get("name")?.as_str()?.to_string(),
                        artist: a
                            .get("artist")
                            .and_then(|v| v.as_str())
                            .unwrap_or_default()
                            .to_string(),
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Pure parsing for a single `getAlbumList2` page, pulled out of
/// `all_albums()`'s pagination loop so it's testable in isolation.
fn parse_album_list2_page(v: &serde_json::Value) -> Vec<Album> {
    parse_album_array(v.get("albumList2").and_then(|a| a.get("album")))
}

/// Pure parsing for a flat array of ID3 artist objects (`id`/`name`),
/// as returned directly (not grouped by index letter, unlike
/// `getArtists`) by `search3`'s `artist` array.
fn parse_artist_array(arr: Option<&serde_json::Value>) -> Vec<Artist> {
    arr.and_then(|a| a.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|a| {
                    Some(Artist {
                        id: a.get("id")?.as_str()?.to_string(),
                        name: a.get("name")?.as_str()?.to_string(),
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Pure parsing for `search3`'s `searchResult3` envelope, pulled out
/// of `search3()` so it's testable against a fixture without a live
/// server, same as every other parse helper in this file.
fn parse_search3_response(v: &serde_json::Value) -> SearchResult {
    let r = v.get("searchResult3");
    SearchResult {
        artists: parse_artist_array(r.and_then(|r| r.get("artist"))),
        albums: parse_album_array(r.and_then(|r| r.get("album"))),
        songs: parse_song_array(r.and_then(|r| r.get("song"))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader, Write};
    use std::net::{TcpListener, TcpStream};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{mpsc, Arc};
    use std::time::{Duration, Instant};

    /// What the mock's catalog endpoints answer, mutable so a test can
    /// change a server's catalog under a client that's already running
    /// (which is what the background sync sees on a real server).
    struct MockCatalog {
        albums: AtomicUsize,
        rev: AtomicUsize,
    }

    /// Everything one mock server answers from, shared with each
    /// connection handler.
    struct MockFixture {
        albums: usize,
        songs_per_album: usize,
        latency: Duration,
        requests: Arc<AtomicUsize>,
        catalog: Arc<MockCatalog>,
    }

    /// Minimal http/1.1 server for the fetch-path tests: canned
    /// subsonic json, an optional per-request delay standing in for
    /// network latency, and counters for requests and accepted
    /// connections (the latter is how keep-alive reuse is asserted).
    struct MockServer {
        url: String,
        requests: Arc<AtomicUsize>,
        connections: Arc<AtomicUsize>,
        catalog: Arc<MockCatalog>,
    }

    impl MockServer {
        fn start(albums: usize, songs_per_album: usize, latency: Duration) -> MockServer {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let url = format!("http://{}", listener.local_addr().unwrap());
            let requests = Arc::new(AtomicUsize::new(0));
            let connections = Arc::new(AtomicUsize::new(0));
            let catalog = Arc::new(MockCatalog {
                albums: AtomicUsize::new(0),
                rev: AtomicUsize::new(0),
            });
            let fixture = Arc::new(MockFixture {
                albums,
                songs_per_album,
                latency,
                requests: requests.clone(),
                catalog: catalog.clone(),
            });
            let conn = connections.clone();
            // detached: the test binary exits without waiting on it
            thread::spawn(move || {
                for stream in listener.incoming().flatten() {
                    conn.fetch_add(1, Ordering::Relaxed);
                    let fixture = fixture.clone();
                    thread::spawn(move || {
                        serve(stream, &fixture);
                    });
                }
            });
            MockServer {
                url,
                requests,
                connections,
                catalog,
            }
        }

        fn client(&self) -> Client {
            Client::new(&NavidromeConfig {
                url: self.url.clone(),
                username: "test".into(),
                password: Some("pw".into()),
                password_file: None,
                max_bitrate_kbps: None,
                transcode_format: None,
            })
            .unwrap()
        }

        fn requests(&self) -> usize {
            self.requests.load(Ordering::Relaxed)
        }

        fn connections(&self) -> usize {
            self.connections.load(Ordering::Relaxed)
        }

        /// How many albums `getAlbumList2` has to page through.
        fn with_catalog_albums(self, total: usize) -> MockServer {
            self.catalog.albums.store(total, Ordering::Relaxed);
            self
        }

        /// Same album count, different ids: a catalog that changed
        /// under the client, which is what has to drop cached
        /// tracklists.
        fn replace_catalog(&self) {
            self.catalog.rev.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// One keep-alive connection: requests until the client hangs up.
    fn serve(stream: TcpStream, fixture: &MockFixture) {
        let mut reader = BufReader::new(stream.try_clone().unwrap());
        let mut writer = stream;
        loop {
            let mut request_line = String::new();
            if reader.read_line(&mut request_line).unwrap_or(0) == 0 {
                return;
            }
            loop {
                let mut header = String::new();
                match reader.read_line(&mut header) {
                    Ok(0) => return,
                    Ok(_) if header.trim().is_empty() => break,
                    Ok(_) => {}
                    Err(_) => return,
                }
            }
            fixture.requests.fetch_add(1, Ordering::Relaxed);
            thread::sleep(fixture.latency);
            let body = mock_body(&request_line, fixture);
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
                body.len()
            );
            if writer.write_all(response.as_bytes()).is_err() {
                return;
            }
            let _ = writer.flush();
        }
    }

    /// One query parameter off a request line, or `None`.
    fn query_param(request_line: &str, key: &str) -> Option<usize> {
        request_line
            .split(&format!("{key}="))
            .nth(1)?
            .split(['&', ' '])
            .next()?
            .parse()
            .ok()
    }

    /// Years descend with album position, so the returned order is the
    /// reverse of the chronological one every caller sorts into.
    fn mock_body(request_line: &str, fixture: &MockFixture) -> String {
        let (albums, songs_per_album) = (fixture.albums, fixture.songs_per_album);
        if request_line.contains("/rest/getAlbumList2") {
            let total = fixture.catalog.albums.load(Ordering::Relaxed);
            let rev = fixture.catalog.rev.load(Ordering::Relaxed);
            let offset = query_param(request_line, "offset").unwrap_or(0);
            let size = query_param(request_line, "size").unwrap_or(ALBUM_PAGE_SIZE);
            let page: Vec<String> = (offset..total.min(offset + size))
                .map(|i| {
                    format!(r#"{{"id":"cat{rev}-{i}","name":"catalog {i}","artist":"artist"}}"#)
                })
                .collect();
            return format!(
                r#"{{"subsonic-response":{{"status":"ok","albumList2":{{"album":[{}]}}}}}}"#,
                page.join(",")
            );
        }
        if request_line.contains("/rest/getArtistInfo2") {
            return r#"{"subsonic-response":{"status":"ok","artistInfo2":{"biography":"mock bio"}}}"#
                .to_string();
        }
        if request_line.contains("/rest/getArtist?") {
            let album_json: Vec<String> = (0..albums)
                .map(|i| {
                    format!(
                        r#"{{"id":"al-{i}","name":"album {i}","year":{}}}"#,
                        2030 - i
                    )
                })
                .collect();
            return format!(
                r#"{{"subsonic-response":{{"status":"ok","artist":{{"id":"ar-1","name":"Bladee","album":[{}]}}}}}}"#,
                album_json.join(",")
            );
        }
        if request_line.contains("/rest/getAlbum?") {
            let album = request_line
                .split("id=")
                .nth(1)
                .and_then(|rest| rest.split(['&', ' ']).next())
                .unwrap_or("al-0")
                .to_string();
            let song_json: Vec<String> = (0..songs_per_album)
                .map(|i| {
                    format!(
                        r#"{{"id":"{album}-s{i}","title":"track {i}","artist":"Bladee","album":"{album}","duration":100}}"#
                    )
                })
                .collect();
            return format!(
                r#"{{"subsonic-response":{{"status":"ok","album":{{"id":"{album}","name":"{album}","year":2024,"song":[{}]}}}}}}"#,
                song_json.join(",")
            );
        }
        r#"{"subsonic-response":{"status":"ok"}}"#.to_string()
    }

    /// Without an explicit format a server-side player profile decides
    /// what wim gets, which is how the same track can arrive as a lossy
    /// re-encode here and as the original in a browser; raw pins it.
    #[test]
    fn stream_url_asks_for_raw_when_nothing_is_overridden() {
        let plain = Client::new(&NavidromeConfig {
            url: "https://example.invalid".into(),
            username: "test".into(),
            password: Some("pw".into()),
            password_file: None,
            max_bitrate_kbps: None,
            transcode_format: None,
        })
        .unwrap();
        let url = plain.stream_url("song-1");
        assert!(!url.contains("maxBitRate"));
        assert!(url.contains("format=raw"));
    }

    /// A bitrate cap is a request to re-encode, so raw must not be
    /// forced alongside it (the server would have to ignore one).
    #[test]
    fn stream_url_drops_raw_when_a_bitrate_cap_is_set() {
        let capped = Client::new(&NavidromeConfig {
            url: "https://example.invalid".into(),
            username: "test".into(),
            password: Some("pw".into()),
            password_file: None,
            max_bitrate_kbps: Some(192),
            transcode_format: None,
        })
        .unwrap();
        let url = capped.stream_url("song-1");
        assert!(url.contains("maxBitRate=192"));
        assert!(!url.contains("format="));
    }

    #[test]
    fn stream_url_includes_overrides_when_set() {
        let overridden = Client::new(&NavidromeConfig {
            url: "https://example.invalid".into(),
            username: "test".into(),
            password: Some("pw".into()),
            password_file: None,
            max_bitrate_kbps: Some(128),
            transcode_format: Some("opus".into()),
        })
        .unwrap();
        let url = overridden.stream_url("song-1");
        assert!(url.contains("maxBitRate=128"));
        assert!(url.contains("format=opus"));
    }

    #[test]
    fn token_is_deterministic_md5_of_password_and_salt() {
        let mut hasher = Md5::new();
        hasher.update(b"sesame");
        hasher.update(b"c19b2d");
        let token = format!("{:x}", hasher.finalize());
        // known-answer check against the Subsonic API docs example
        assert_eq!(token, "26719a1196d2a940705a59634eb18eab");
    }

    #[test]
    fn normalize_url_defaults_to_https_when_scheme_omitted() {
        assert_eq!(
            normalize_url("navidrome.example.com"),
            "https://navidrome.example.com"
        );
    }

    #[test]
    fn normalize_url_leaves_an_explicit_scheme_alone() {
        assert_eq!(
            normalize_url("http://192.168.1.5:4533"),
            "http://192.168.1.5:4533"
        );
        assert_eq!(
            normalize_url("https://navidrome.example.com/"),
            "https://navidrome.example.com"
        );
    }

    #[test]
    fn parses_artists_index_flattening_every_letter_group() {
        let v: serde_json::Value = serde_json::from_str(
            r#"{
                "artists": {
                    "ignoredArticles": "The El La",
                    "index": [
                        {"name": "A", "artist": [{"id": "ar-1", "name": "Artist Alpha", "albumCount": 3}]},
                        {"name": "B", "artist": [
                            {"id": "ar-2", "name": "Bladee", "albumCount": 12},
                            {"id": "ar-3", "name": "Burial", "albumCount": 2}
                        ]}
                    ]
                }
            }"#,
        )
        .unwrap();
        let artists = parse_artists_index(&v);
        let names: Vec<&str> = artists.iter().map(|a| a.name.as_str()).collect();
        assert_eq!(names, vec!["Artist Alpha", "Bladee", "Burial"]);
    }

    #[test]
    fn parses_album_list2_page_including_various_artists_compilations() {
        let v: serde_json::Value = serde_json::from_str(
            r#"{
                "albumList2": {
                    "album": [
                        {"id": "al-1", "name": "Cold Visions", "artist": "Bladee", "artistId": "ar-2"},
                        {"id": "al-2", "name": "Global", "artist": "Various Artists"}
                    ]
                }
            }"#,
        )
        .unwrap();
        let albums = parse_album_list2_page(&v);
        assert_eq!(albums.len(), 2);
        assert_eq!(albums[0].name, "Cold Visions");
        assert_eq!(albums[0].artist, "Bladee");
        assert_eq!(albums[1].artist, "Various Artists");
    }

    #[test]
    fn parses_artist_albums_including_a_missing_year() {
        let v: serde_json::Value = serde_json::from_str(
            r#"{"artist": {"id": "ar-2", "name": "Bladee", "album": [
                {"id": "al-1", "name": "Cold Visions", "year": 2024},
                {"id": "al-2", "name": "Global"}
            ]}}"#,
        )
        .unwrap();
        let albums = parse_artist_albums(&v);
        assert_eq!(albums.len(), 2);
        assert_eq!(albums[0].name, "Cold Visions");
        assert_eq!(albums[0].year, Some(2024));
        assert_eq!(albums[1].name, "Global");
        assert_eq!(albums[1].year, None);
    }

    #[test]
    fn parses_album_songs_regardless_of_parent_key() {
        // getAlbum nests tracks under "song"...
        let album: serde_json::Value = serde_json::from_str(
            r#"{"album": {"id": "al-1", "song": [
                {"id": "s1", "title": "Icedancer", "artist": "Bladee", "album": "Cold Visions", "duration": 180}
            ]}}"#,
        )
        .unwrap();
        let songs = parse_song_array(album.get("album").and_then(|a| a.get("song")));
        assert_eq!(songs.len(), 1);
        assert_eq!(songs[0].title, "Icedancer");

        // ...getPlaylist nests them under "entry"; same per-track shape.
        let playlist: serde_json::Value = serde_json::from_str(
            r#"{"playlist": {"id": "p1", "entry": [
                {"id": "s2", "title": "Drain Gang", "artist": "Bladee", "album": "Global", "duration": 120}
            ]}}"#,
        )
        .unwrap();
        let entries = parse_song_array(playlist.get("playlist").and_then(|p| p.get("entry")));
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].title, "Drain Gang");
    }

    #[test]
    fn missing_fields_parse_to_empty_not_a_panic() {
        let v: serde_json::Value = serde_json::from_str(r#"{"status":"ok"}"#).unwrap();
        assert!(parse_artists_index(&v).is_empty());
        assert!(parse_album_list2_page(&v).is_empty());
        assert!(parse_song_array(None).is_empty());
        let info = parse_artist_info(&v);
        assert!(info.biography.is_none());
        assert!(info.similar_artists.is_empty());
    }

    /// The whole point of re-parsing `getArtistInfo2`: this list was
    /// being thrown away, and it is the only cross-artist similarity
    /// signal available without collaborative data.
    #[test]
    fn parses_similar_artists_alongside_the_biography() {
        let v: serde_json::Value = serde_json::from_str(
            r#"{"artistInfo2": {
                "biography": "swedish artist",
                "similarArtist": [
                    {"id": "ar-7", "name": "Ecco2k", "albumCount": 2},
                    {"id": "-1", "name": "Thaiboy Digital", "albumCount": 0},
                    {"name": "Yung Lean"},
                    {"id": "ar-9", "name": ""}
                ]
            }}"#,
        )
        .unwrap();
        let info = parse_artist_info(&v);
        assert_eq!(info.biography.as_deref(), Some("swedish artist"));
        let names: Vec<&str> = info
            .similar_artists
            .iter()
            .map(|a| a.name.as_str())
            .collect();
        assert_eq!(
            names,
            vec!["Ecco2k", "Thaiboy Digital", "Yung Lean"],
            "nameless entries drop, id-less ones stay: the name is what gets intersected"
        );
        assert_eq!(info.similar_artists[0].id, "ar-7");
        assert_eq!(
            info.similar_artists[1].id, "",
            "\"-1\" means not in this library, so it is not a usable id"
        );
        assert_eq!(info.similar_artists[2].id, "");
    }

    /// Navidrome serves the multi-valued `genres` array alongside the
    /// legacy single `genre` string; the array wins when both are
    /// there, since a two-genre track is two signals.
    #[test]
    fn parses_the_multi_valued_genres_array_in_preference_to_the_legacy_string() {
        let v: serde_json::Value = serde_json::from_str(
            r#"{"song": [
                {"id": "s1", "title": "Be Nice 2 Me", "artist": "Bladee", "album": "Eversince",
                 "genre": "Hip-Hop", "genres": [{"name": "Hip-Hop"}, {"name": "Cloud Rap"}]}
            ]}"#,
        )
        .unwrap();
        let songs = parse_song_array(v.get("song"));
        assert_eq!(songs[0].genres, vec!["Hip-Hop", "Cloud Rap"]);
    }

    #[test]
    fn falls_back_to_the_legacy_genre_string_and_tolerates_no_genre_at_all() {
        let v: serde_json::Value = serde_json::from_str(
            r#"{"song": [
                {"id": "s1", "title": "One", "artist": "Metallica", "album": "Black", "genre": "Metal"},
                {"id": "s2", "title": "Two", "artist": "Metallica", "album": "Black"},
                {"id": "s3", "title": "Three", "artist": "Metallica", "album": "Black", "genre": "", "genres": []}
            ]}"#,
        )
        .unwrap();
        let songs = parse_song_array(v.get("song"));
        assert_eq!(songs[0].genres, vec!["Metal"]);
        assert!(songs[1].genres.is_empty());
        assert!(songs[2].genres.is_empty(), "empty tags are not a genre");
    }

    #[test]
    fn parses_top_songs_in_the_servers_popularity_order() {
        let v: serde_json::Value = serde_json::from_str(
            r#"{"topSongs": {"song": [
                {"id": "s1", "title": "Be Nice 2 Me", "artist": "Bladee", "album": "Eversince", "genre": "Cloud Rap"},
                {"id": "s2", "title": "Obedient", "artist": "Bladee", "album": "Eversince"}
            ]}}"#,
        )
        .unwrap();
        let songs = parse_song_array(v.get("topSongs").and_then(|t| t.get("song")));
        let titles: Vec<&str> = songs.iter().map(|s| s.title.as_str()).collect();
        assert_eq!(titles, vec!["Be Nice 2 Me", "Obedient"]);
        assert_eq!(songs[0].genres, vec!["Cloud Rap"]);
    }

    #[test]
    fn parses_search3_response_including_a_song_title_match() {
        // shape per the Subsonic API docs for search3 (ID3-based):
        // artist/album/song all flat arrays directly under
        // searchResult3, same per-entity fields as getArtist/
        // getAlbum/getPlaylist elsewhere in this file.
        let v: serde_json::Value = serde_json::from_str(
            r#"{
                "searchResult3": {
                    "artist": [{"id": "ar-2", "name": "Bladee"}],
                    "album": [{"id": "al-1", "name": "Icedancer", "artist": "Bladee"}],
                    "song": [
                        {"id": "s1", "title": "Flatline", "artist": "Bladee", "album": "Icedancer", "duration": 142}
                    ]
                }
            }"#,
        )
        .unwrap();
        let result = parse_search3_response(&v);
        assert_eq!(result.artists.len(), 1);
        assert_eq!(result.artists[0].name, "Bladee");
        assert_eq!(result.albums.len(), 1);
        assert_eq!(result.albums[0].name, "Icedancer");
        assert_eq!(result.songs.len(), 1);
        assert_eq!(result.songs[0].title, "Flatline");
    }

    #[test]
    fn parses_search3_response_with_no_matches_as_empty_not_a_panic() {
        let v: serde_json::Value = serde_json::from_str(r#"{"searchResult3": {}}"#).unwrap();
        let result = parse_search3_response(&v);
        assert!(result.artists.is_empty());
        assert!(result.albums.is_empty());
        assert!(result.songs.is_empty());
    }

    #[test]
    fn artist_fetch_returns_every_album_chronologically_with_its_tracks() {
        let server = MockServer::start(5, 4, Duration::ZERO);
        let tracks = server.client().artist_albums_with_tracks("ar-1").unwrap();
        assert_eq!(tracks.len(), 5);
        // mock years descend with album position, so chronological
        // order is the reverse of what getArtist returned
        let names: Vec<&str> = tracks.iter().map(|t| t.album.name.as_str()).collect();
        assert_eq!(
            names,
            vec!["album 4", "album 3", "album 2", "album 1", "album 0"]
        );
        let indexes: Vec<usize> = tracks.iter().map(|t| t.index).collect();
        assert_eq!(indexes, vec![0, 1, 2, 3, 4]);
        assert_eq!(tracks.iter().map(|t| t.songs.len()).sum::<usize>(), 20);
        assert_eq!(tracks[0].songs[0].album, "al-4");
        assert_eq!(server.requests(), 6); // one getArtist + one getAlbum per album
    }

    #[test]
    fn artist_fetch_reuses_connections_instead_of_one_per_album() {
        let server = MockServer::start(15, 2, Duration::ZERO);
        let tracks = server.client().artist_albums_with_tracks("ar-1").unwrap();
        assert_eq!(tracks.len(), 15);
        assert_eq!(server.requests(), 16);
        // 16 requests over at most one connection per worker; with
        // ureq's default idle-per-host of 1 this was ~16 handshakes.
        assert!(
            server.connections() <= ALBUM_FETCH_CONCURRENCY + 1,
            "opened {} connections for 16 requests",
            server.connections()
        );
    }

    #[test]
    fn artist_fetch_with_no_albums_is_empty_not_an_error() {
        let server = MockServer::start(0, 0, Duration::ZERO);
        assert!(server
            .client()
            .artist_albums_with_tracks("ar-1")
            .unwrap()
            .is_empty());
        assert_eq!(server.requests(), 1);
    }

    #[test]
    fn streaming_artist_fetch_returns_the_album_list_then_streams_tracks_and_bio() {
        let server = MockServer::start(6, 3, Duration::from_millis(20));
        let client = server.client();
        let (tx, rx) = mpsc::channel();
        let albums = client.stream_artist_tracks("ar-1", tx).unwrap();
        // the album list (names included) is back after one round
        // trip, before any getAlbum has been issued
        assert_eq!(albums.len(), 6);
        assert_eq!(albums[0].name, "album 5");
        assert_eq!(albums[0].year, Some(2025));

        let mut streamed = vec![None; albums.len()];
        let mut bio = None;
        for event in rx {
            match event {
                ArtistLoad::Album(tracks) => {
                    let index = tracks.index;
                    streamed[index] = Some(tracks);
                }
                ArtistLoad::Bio(b) => bio = b,
            }
        }
        assert!(streamed.iter().all(|slot| slot.is_some()));
        let songs: usize = streamed
            .iter()
            .map(|slot| slot.as_ref().unwrap().songs.len())
            .sum();
        assert_eq!(songs, 18);
        assert_eq!(bio.as_deref(), Some("mock bio"));
        // getArtist + 6 getAlbum + getArtistInfo2, nothing refetched
        assert_eq!(server.requests(), 8);
    }

    #[test]
    fn streaming_artist_fetch_stops_when_the_receiver_is_dropped() {
        let server = MockServer::start(60, 1, Duration::from_millis(20));
        let client = server.client();
        let (tx, rx) = mpsc::channel();
        client.stream_artist_tracks("ar-1", tx).unwrap();
        drop(rx);
        thread::sleep(Duration::from_millis(600));
        // each worker notices on its first failed send, so a closed
        // view costs at most one in-flight request per worker
        assert!(
            server.requests() <= 2 + ALBUM_FETCH_CONCURRENCY * 2,
            "kept fetching after the receiver was dropped: {} requests",
            server.requests()
        );
    }

    #[test]
    fn the_whole_album_catalog_comes_back_across_pages() {
        let server = MockServer::start(0, 0, Duration::ZERO).with_catalog_albums(1_200);
        let albums = server.client().all_albums().unwrap();
        assert_eq!(albums.len(), 1_200);
        assert_eq!(albums[0].id, "cat0-0");
        assert_eq!(albums[1_199].id, "cat0-1199");
        let distinct: std::collections::HashSet<&str> =
            albums.iter().map(|a| a.id.as_str()).collect();
        assert_eq!(distinct.len(), 1_200, "no page was fetched twice");
        // page 0, then one wave of four: 500 + 500 + 200 is the whole
        // catalog, and the two offsets past the end answer empty.
        assert_eq!(server.requests(), 5);
    }

    #[test]
    fn a_catalog_inside_one_page_is_a_single_request() {
        let server = MockServer::start(0, 0, Duration::ZERO).with_catalog_albums(120);
        assert_eq!(server.client().all_albums().unwrap().len(), 120);
        assert_eq!(
            server.requests(),
            1,
            "a library under one page must not pay for a wave"
        );
    }

    /// Catalog pages are independent round trips at known offsets, so
    /// they overlap; serial pagination cost one latency per page.
    #[test]
    #[ignore = "timing, run explicitly"]
    fn album_pagination_timing_serial_vs_waved() {
        const LATENCY: Duration = Duration::from_millis(120);
        let server = MockServer::start(0, 0, LATENCY).with_catalog_albums(5_000);
        let client = server.client();

        let started = Instant::now();
        let mut serial = 0usize;
        let mut offset = 0usize;
        loop {
            let page = client.album_page(offset).unwrap();
            let got = page.len();
            serial += got;
            if got < ALBUM_PAGE_SIZE {
                break;
            }
            offset += ALBUM_PAGE_SIZE;
        }
        let serial_elapsed = started.elapsed();
        let serial_requests = server.requests();

        let server = MockServer::start(0, 0, LATENCY).with_catalog_albums(5_000);
        let client = server.client();
        let started = Instant::now();
        let waved = client.all_albums().unwrap().len();
        let waved_elapsed = started.elapsed();

        eprintln!(
            "5000 albums @ {LATENCY:?} latency:\n  \
             serial: {serial} albums, {serial_requests} requests, {serial_elapsed:?}\n  \
             waved:  {waved} albums, {} requests, {waved_elapsed:?}",
            server.requests(),
        );
        assert_eq!(serial, waved);
        assert!(waved_elapsed * 2 < serial_elapsed);
    }

    #[test]
    fn opening_the_same_album_twice_costs_one_request() {
        let server = MockServer::start(2, 3, Duration::ZERO);
        let client = server.client();
        let first = client.get_album("al-0").unwrap();
        let second = client.get_album("al-0").unwrap();
        assert_eq!(first.songs.len(), second.songs.len());
        assert_eq!(first.year, second.year);
        assert_eq!(server.requests(), 1, "the second open must be served warm");

        client.get_album("al-1").unwrap();
        assert_eq!(server.requests(), 2, "a different album is its own key");
    }

    /// Opening an artist, then one of their albums, used to re-ask for
    /// a tracklist the artist fetch had already read.
    #[test]
    fn an_artist_page_reopened_costs_nothing() {
        let server = MockServer::start(6, 4, Duration::ZERO);
        let client = server.client();
        assert_eq!(client.artist_albums_with_tracks("ar-1").unwrap().len(), 6);
        assert_eq!(server.requests(), 7); // getArtist + one per album
        assert_eq!(client.artist_albums_with_tracks("ar-1").unwrap().len(), 6);
        client.get_album("al-3").unwrap();
        assert_eq!(
            server.requests(),
            7,
            "reopening an artist within the ttl must not re-ask for anything"
        );
    }

    /// The invalidation path: the background catalog sync is what
    /// notices a server's catalog moved, and every cached tracklist
    /// for it has to go at that point rather than waiting out its ttl.
    #[test]
    fn a_changed_catalog_drops_cached_tracklists() {
        let server = MockServer::start(2, 3, Duration::ZERO).with_catalog_albums(10);
        // the sync client and the client the corpus hands the ui are
        // different instances for the same server, exactly as in the
        // app; they share one cache.
        let syncing = server.client();
        let browsing = server.client();

        syncing.all_albums().unwrap(); // first fingerprint
        browsing.get_album("al-0").unwrap();
        browsing.get_album("al-0").unwrap();
        let before = server.requests();

        syncing.all_albums().unwrap(); // unchanged: nothing dropped
        browsing.get_album("al-0").unwrap();
        assert_eq!(
            server.requests(),
            before + 1,
            "an unchanged catalog must not throw the cache away"
        );

        server.replace_catalog();
        syncing.all_albums().unwrap();
        let after_sync = server.requests();
        browsing.get_album("al-0").unwrap();
        assert_eq!(
            server.requests(),
            after_sync + 1,
            "a changed catalog must force the next open to refetch"
        );
    }

    #[test]
    fn a_cached_answer_past_its_ttl_is_not_served() {
        let mut cache = NavCache::default();
        let key = "getAlbum\u{1f}id=al-1".to_string();
        cache.put(key.clone(), serde_json::json!({"album": {}}));
        assert!(cache.get(&key).is_some());
        // backdate past the ttl rather than sleeping through it
        if let Some(entry) = cache.entries.get_mut(&key) {
            entry.at = Instant::now() - NAV_CACHE_TTL - Duration::from_secs(1);
        }
        assert!(
            cache.get(&key).is_none(),
            "nothing may be served for longer than one sync interval"
        );
    }

    #[test]
    fn the_response_cache_stays_bounded() {
        let mut cache = NavCache::default();
        for i in 0..NAV_CACHE_CAPACITY + 50 {
            cache.put(format!("getAlbum\u{1f}id=al-{i}"), serde_json::json!(i));
        }
        assert_eq!(cache.entries.len(), NAV_CACHE_CAPACITY);
        assert_eq!(cache.order.len(), NAV_CACHE_CAPACITY);
        assert!(
            cache.get("getAlbum\u{1f}id=al-0").is_none(),
            "the oldest entry is the one that goes"
        );
        let newest = format!("getAlbum\u{1f}id=al-{}", NAV_CACHE_CAPACITY + 49);
        assert!(cache.get(&newest).is_some());
    }

    /// Serial-vs-parallel numbers for the artist page against the mock
    /// server's simulated latency; a timing assertion, so it's opt-in
    /// (`cargo test -- --ignored --nocapture`).
    #[test]
    #[ignore = "timing, run explicitly"]
    fn artist_fetch_timing_serial_vs_parallel() {
        const LATENCY: Duration = Duration::from_millis(120);
        let server = MockServer::start(15, 10, LATENCY);
        let client = server.client();

        let started = Instant::now();
        let albums = client.get_artist("ar-1").unwrap();
        let serial: usize = albums
            .iter()
            .map(|a| client.get_album(&a.id).unwrap().songs.len())
            .sum();
        let serial_elapsed = started.elapsed();
        let serial_requests = server.requests();
        let serial_connections = server.connections();

        let server = MockServer::start(15, 10, LATENCY);
        let client = server.client();
        let started = Instant::now();
        let parallel: usize = client
            .artist_albums_with_tracks("ar-1")
            .unwrap()
            .iter()
            .map(|t| t.songs.len())
            .sum();
        let parallel_elapsed = started.elapsed();

        eprintln!(
            "serial:   {serial} songs, {serial_requests} requests, {serial_connections} connections, {serial_elapsed:?}"
        );
        // a second open of the same artist, which is what going back
        // to the sidebar and in again costs.
        let started = Instant::now();
        let reopened: usize = client
            .artist_albums_with_tracks("ar-1")
            .unwrap()
            .iter()
            .map(|t| t.songs.len())
            .sum();
        let reopened_elapsed = started.elapsed();

        eprintln!(
            "parallel: {parallel} songs, {} requests, {} connections, {parallel_elapsed:?}",
            server.requests(),
            server.connections()
        );
        eprintln!(
            "reopened: {reopened} songs, {} requests total, {reopened_elapsed:?}",
            server.requests()
        );
        assert_eq!(serial, parallel);
        assert_eq!(serial, reopened);
        assert!(parallel_elapsed * 2 < serial_elapsed);
        assert!(reopened_elapsed * 10 < parallel_elapsed);
    }

    fn cfg(url: &str, user: &str) -> NavidromeConfig {
        NavidromeConfig {
            url: url.into(),
            username: user.into(),
            password: Some("pw".into()),
            password_file: None,
            max_bitrate_kbps: None,
            transcode_format: None,
        }
    }

    #[test]
    fn a_client_matches_the_config_it_was_built_from() {
        let c = cfg("https://example.invalid", "ana");
        assert!(Client::new(&c).unwrap().matches(&c));
    }

    /// Normalization is why this lives on `Client`: a trailing slash
    /// must not look like a different server.
    #[test]
    fn a_trailing_slash_is_still_the_same_server() {
        let built = Client::new(&cfg("https://example.invalid", "ana")).unwrap();
        assert!(built.matches(&cfg("https://example.invalid/", "ana")));
    }

    #[test]
    fn a_different_user_or_host_does_not_match() {
        let built = Client::new(&cfg("https://example.invalid", "ana")).unwrap();
        assert!(!built.matches(&cfg("https://example.invalid", "bob")));
        assert!(!built.matches(&cfg("https://other.invalid", "ana")));
    }

    /// The reason stream options are in the match: reusing a client
    /// built before a bitrate change would stream at the old setting.
    #[test]
    fn changed_stream_options_make_it_a_different_client() {
        let base = cfg("https://example.invalid", "ana");
        let built = Client::new(&base).unwrap();
        let mut rebitrated = base.clone();
        rebitrated.max_bitrate_kbps = Some(128);
        assert!(!built.matches(&rebitrated));
        let mut retranscoded = base.clone();
        retranscoded.transcode_format = Some("opus".into());
        assert!(!built.matches(&retranscoded));
    }
}
