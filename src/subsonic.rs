//! Hand-rolled Subsonic API client (targets Navidrome). The handful of
//! existing subsonic crates are unmaintained/incomplete, and the surface
//! we need (ping, catalog browsing, playlists, stream urls) is small enough that
//! owning it directly is simpler than fighting someone else's types.

use std::io::Read;
use std::thread;

use md5::{Digest, Md5};
use rand::Rng;
use serde::Deserialize;

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

/// Cheap to clone (ureq's `Agent` is `Arc`-backed internally); every
/// remote `SearchItem` carries an `Arc<Client>` so playback can later
/// resolve to the right server's stream url without the app needing a
/// separate "active server" concept to track.
#[derive(Clone)]
pub struct Client {
    base_url: String,
    username: String,
    password: String,
    agent: ureq::Agent,
    /// Client-side `stream` overrides; `None` means "let the server's
    /// own configured transcoding profile decide," the default.
    max_bitrate_kbps: Option<u32>,
    transcode_format: Option<String>,
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
    // library.rs) — SearchItem::Song doesn't carry duration either.
    #[allow(dead_code)]
    pub duration: Option<u32>,
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
    /// view's ALBUMS column can render off one round trip instead of
    /// waiting on a `getAlbum` per album to learn the names. Read by
    /// the ui wiring that lands separately, hence the allow.
    #[allow(dead_code)]
    pub name: String,
    /// `None` sorts last rather than first; an unknown release date
    /// shouldn't jump to the front of a chronological listing.
    pub year: Option<u32>,
}

/// One album's tracks from an artist fetch. `index` is the album's
/// slot in the chronological album list it came from, because parallel
/// fetches complete out of order.
#[derive(Debug, Clone)]
#[allow(dead_code)] // consumed by the artist view; ui wiring lands separately
pub struct ArtistAlbumTracks {
    pub index: usize,
    pub album: ArtistAlbum,
    pub songs: Vec<Song>,
}

/// Progress events from `stream_artist_tracks`, in arrival order.
#[derive(Debug, Clone)]
#[allow(dead_code)] // same as ArtistAlbumTracks
pub enum ArtistLoad {
    /// Best-effort, `None` when the server has no biography.
    Bio(Option<String>),
    Album(ArtistAlbumTracks),
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

    pub fn new(cfg: &NavidromeConfig) -> anyhow::Result<Self> {
        Ok(Client {
            base_url: normalize_url(&cfg.url),
            username: cfg.username.clone(),
            password: cfg.resolve_password()?,
            agent: ureq::AgentBuilder::new()
                .max_idle_connections_per_host(MAX_IDLE_CONNECTIONS_PER_HOST)
                .build(),
            max_bitrate_kbps: cfg.max_bitrate_kbps,
            transcode_format: cfg.transcode_format.clone(),
        })
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
        let v = self.get("getAlbum", &[("id", album_id)])?;
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
        let v = self.get("getArtist", &[("id", artist_id)])?;
        Ok(parse_artist_albums(&v))
    }

    /// Navidrome's own `getArtistInfo2` already proxies a biography
    /// from last.fm, no separate api key needed; `None` if the server
    /// has nothing (unknown artist, no internet metadata lookup
    /// configured, etc.), not an error.
    pub fn get_artist_bio(&self, artist_id: &str) -> anyhow::Result<Option<String>> {
        let v = self.get("getArtistInfo2", &[("id", artist_id)])?;
        Ok(v.get("artistInfo2")
            .and_then(|a| a.get("biography"))
            .and_then(|b| b.as_str())
            .map(str::to_string)
            .filter(|s| !s.is_empty()))
    }

    /// Same idea for playlists: `getPlaylist`'s tracks live under
    /// `entry`, not `song`, but the per-track shape is identical.
    /// Playlists have no release year, so no metadata wrapper needed.
    pub fn get_playlist_songs(&self, playlist_id: &str) -> anyhow::Result<Vec<Song>> {
        let v = self.get("getPlaylist", &[("id", playlist_id)])?;
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
        let list = v
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
        Ok(list)
    }

    /// The entire artist catalog in one call; `getArtists` (ID3
    /// browsing) returns everything grouped alphabetically by index
    /// letter, unlike album/song listing which is paginated. Matches
    /// how real clients (e.g. Feishin) index artists: flatten every
    /// index's `artist` array into one flat list.
    pub fn all_artists(&self) -> anyhow::Result<Vec<Artist>> {
        let v = self.get("getArtists", &[])?;
        Ok(parse_artists_index(&v))
    }

    /// The entire album catalog, paginated via `getAlbumList2`
    /// (`offset`/`size`); unlike artists, album listing has no
    /// single-call "everything" mode, so this loops until a page
    /// returns fewer than requested (the standard termination
    /// condition; Navidrome also reports `x-total-count` but that
    /// requires reading response headers, which ureq's `into_json()`
    /// convenience here doesn't expose; the fewer-than-requested check
    /// is sufficient and matches what the spec guarantees).
    pub fn all_albums(&self) -> anyhow::Result<Vec<Album>> {
        const PAGE_SIZE: usize = 500;
        let mut albums = Vec::new();
        let mut offset = 0usize;
        loop {
            let v = self.get(
                "getAlbumList2",
                &[
                    ("type", "alphabeticalByName"),
                    ("size", &PAGE_SIZE.to_string()),
                    ("offset", &offset.to_string()),
                ],
            )?;
            let page = parse_album_list2_page(&v);
            let got = page.len();
            albums.extend(page);
            if got < PAGE_SIZE {
                break;
            }
            offset += PAGE_SIZE;
        }
        Ok(albums)
    }

    /// Stream url for a song id. Transcoding is left to the server's own
    /// configured profile by default (no format/bitrate override); see
    /// ROADMAP.md.
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
        if let Some(format) = &self.transcode_format {
            url.push_str(&format!("&format={format}"));
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

/// The artist-page data path. Separate impl block (and one blanket
/// `dead_code` allow) because the ui wiring that calls these lands
/// separately; drop the allow once it does.
#[allow(dead_code)]
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
    pub fn artist_albums_with_tracks(
        &self,
        artist_id: &str,
    ) -> anyhow::Result<Vec<ArtistAlbumTracks>> {
        let albums = self.artist_albums_chronological(artist_id)?;
        let workers = ALBUM_FETCH_CONCURRENCY.min(albums.len());
        if workers == 0 {
            return Ok(Vec::new());
        }
        let albums = &albums;
        let batches: Vec<Vec<ArtistAlbumTracks>> = thread::scope(|scope| {
            (0..workers)
                .map(|worker| {
                    scope.spawn(move || {
                        albums
                            .iter()
                            .enumerate()
                            .skip(worker)
                            .step_by(workers)
                            .filter_map(|(index, album)| self.album_tracks(index, album))
                            .collect::<Vec<_>>()
                    })
                })
                .collect::<Vec<_>>()
                .into_iter()
                .map(|handle| handle.join().unwrap_or_default())
                .collect()
        });
        let mut tracks: Vec<ArtistAlbumTracks> = batches.into_iter().flatten().collect();
        tracks.sort_by_key(|t| t.index);
        Ok(tracks)
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
fn normalize_url(url: &str) -> String {
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
                    })
                })
                .collect()
        })
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

    /// Minimal http/1.1 server for the fetch-path tests: canned
    /// subsonic json, an optional per-request delay standing in for
    /// network latency, and counters for requests and accepted
    /// connections (the latter is how keep-alive reuse is asserted).
    struct MockServer {
        url: String,
        requests: Arc<AtomicUsize>,
        connections: Arc<AtomicUsize>,
    }

    impl MockServer {
        fn start(albums: usize, songs_per_album: usize, latency: Duration) -> MockServer {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let url = format!("http://{}", listener.local_addr().unwrap());
            let requests = Arc::new(AtomicUsize::new(0));
            let connections = Arc::new(AtomicUsize::new(0));
            let (req, conn) = (requests.clone(), connections.clone());
            // detached: the test binary exits without waiting on it
            thread::spawn(move || {
                for stream in listener.incoming().flatten() {
                    conn.fetch_add(1, Ordering::Relaxed);
                    let req = req.clone();
                    thread::spawn(move || {
                        serve(stream, albums, songs_per_album, latency, &req);
                    });
                }
            });
            MockServer {
                url,
                requests,
                connections,
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
    }

    /// One keep-alive connection: requests until the client hangs up.
    fn serve(
        stream: TcpStream,
        albums: usize,
        songs_per_album: usize,
        latency: Duration,
        requests: &AtomicUsize,
    ) {
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
            requests.fetch_add(1, Ordering::Relaxed);
            thread::sleep(latency);
            let body = mock_body(&request_line, albums, songs_per_album);
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

    /// Years descend with album position, so the returned order is the
    /// reverse of the chronological one every caller sorts into.
    fn mock_body(request_line: &str, albums: usize, songs_per_album: usize) -> String {
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

    #[test]
    fn stream_url_omits_overrides_by_default_and_includes_them_when_set() {
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
        assert!(!url.contains("format="));

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
        eprintln!(
            "parallel: {parallel} songs, {} requests, {} connections, {parallel_elapsed:?}",
            server.requests(),
            server.connections()
        );
        assert_eq!(serial, parallel);
        assert!(parallel_elapsed * 2 < serial_elapsed);
    }
}
