//! Hand-rolled Subsonic API client (targets Navidrome). The handful of
//! existing subsonic crates are unmaintained/incomplete, and the surface
//! we need (ping, search3, playlists, stream urls) is small enough that
//! owning it directly is simpler than fighting someone else's types.

use md5::{Digest, Md5};
use rand::Rng;
use serde::Deserialize;

use crate::config::NavidromeConfig;

const API_VERSION: &str = "1.16.1";
const CLIENT_NAME: &str = "wim";

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
    pub duration: Option<u32>,
}

#[derive(Debug, Clone)]
pub struct Playlist {
    pub id: String,
    pub name: String,
}

#[derive(Debug, Default, Clone)]
pub struct SearchResult {
    pub artists: Vec<Artist>,
    pub albums: Vec<Album>,
    pub songs: Vec<Song>,
}

impl Client {
    pub fn new(cfg: &NavidromeConfig) -> anyhow::Result<Self> {
        Ok(Client {
            base_url: normalize_url(&cfg.url),
            username: cfg.username.clone(),
            password: cfg.resolve_password()?,
            agent: ureq::AgentBuilder::new().build(),
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
        let r = v.get("searchResult3").cloned().unwrap_or_default();

        let artists = r
            .get("artist")
            .and_then(|a| a.as_array())
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
            .unwrap_or_default();

        let albums = r
            .get("album")
            .and_then(|a| a.as_array())
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
            .unwrap_or_default();

        let songs = parse_song_array(r.get("song"));

        Ok(SearchResult {
            artists,
            albums,
            songs,
        })
    }

    /// Every track on an album, in album order; used to expand a
    /// playable album into its actual tracks (playing/queuing an album
    /// means playing every song on it, not the album entity itself).
    pub fn get_album_songs(&self, album_id: &str) -> anyhow::Result<Vec<Song>> {
        let v = self.get("getAlbum", &[("id", album_id)])?;
        Ok(parse_song_array(v.get("album").and_then(|a| a.get("song"))))
    }

    /// Same idea for playlists: `getPlaylist`'s tracks live under
    /// `entry`, not `song`, but the per-track shape is identical.
    pub fn get_playlist_songs(&self, playlist_id: &str) -> anyhow::Result<Vec<Song>> {
        let v = self.get("getPlaylist", &[("id", playlist_id)])?;
        Ok(parse_song_array(v.get("playlist").and_then(|p| p.get("entry"))))
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

        format!(
            "{}/rest/stream?id={}&u={}&t={}&s={}&v={}&c={}",
            self.base_url, song_id, self.username, token, salt, API_VERSION, CLIENT_NAME
        )
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
                        artist: s.get("artist").and_then(|v| v.as_str()).unwrap_or_default().to_string(),
                        album: s.get("album").and_then(|v| v.as_str()).unwrap_or_default().to_string(),
                        duration: s.get("duration").and_then(|v| v.as_u64()).map(|d| d as u32),
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Pure parsing for a single `getAlbumList2` page, pulled out of
/// `all_albums()`'s pagination loop so it's testable in isolation.
fn parse_album_list2_page(v: &serde_json::Value) -> Vec<Album> {
    v.get("albumList2")
        .and_then(|a| a.get("album"))
        .and_then(|a| a.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|a| {
                    Some(Album {
                        id: a.get("id")?.as_str()?.to_string(),
                        name: a.get("name")?.as_str()?.to_string(),
                        artist: a.get("artist").and_then(|v| v.as_str()).unwrap_or_default().to_string(),
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

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
        assert_eq!(normalize_url("navidrome.example.com"), "https://navidrome.example.com");
    }

    #[test]
    fn normalize_url_leaves_an_explicit_scheme_alone() {
        assert_eq!(normalize_url("http://192.168.1.5:4533"), "http://192.168.1.5:4533");
        assert_eq!(normalize_url("https://navidrome.example.com/"), "https://navidrome.example.com");
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
}
