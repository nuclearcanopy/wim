//! Lyrics providers, LRC parsing and asynchronous, cached lookups.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::sync::Arc;
use std::thread;

use lofty::file::TaggedFileExt;
use lofty::prelude::*;
use lofty::probe::Probe;
use md5::{Digest, Md5};
use serde::{Deserialize, Serialize};

const BASE_URL: &str = "https://lrclib.net/api";

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Lyrics {
    #[serde(default, rename = "plainLyrics")]
    pub plain: Option<String>,
    #[serde(default, rename = "syncedLyrics")]
    pub synced: Option<String>,
    #[serde(default)]
    pub instrumental: bool,
    #[serde(default)]
    pub source: String,
    #[serde(default)]
    pub attribution: Option<String>,
    #[serde(default)]
    pub tracking_url: Option<String>,
    #[serde(default, rename = "trackName")]
    pub track_name: Option<String>,
    #[serde(default, rename = "artistName")]
    pub artist_name: Option<String>,
    #[serde(default, rename = "albumName")]
    pub album_name: Option<String>,
    #[serde(default)]
    pub duration: Option<f64>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct TimedLine {
    pub at: f64,
    pub text: String,
}

fn candidate_score(lyrics: &Lyrics, artist: &str, track: &str) -> f64 {
    let normalize = |value: &str| {
        value
            .to_lowercase()
            .chars()
            .filter(|ch| ch.is_alphanumeric())
            .collect::<String>()
    };
    let title_score = lyrics
        .track_name
        .as_deref()
        .map(|value| strsim::normalized_levenshtein(&normalize(value), &normalize(track)))
        .unwrap_or(0.0);
    if artist.trim().is_empty() {
        title_score
    } else {
        let artist_score = lyrics
            .artist_name
            .as_deref()
            .map(|value| strsim::normalized_levenshtein(&normalize(value), &normalize(artist)))
            .unwrap_or(0.0);
        title_score * 0.75 + artist_score * 0.25
    }
}

/// Parses line-synchronized LRC, including repeated timestamps, signed
/// offsets and one-, two-, or three-digit fractional seconds.
pub fn parse_synced(raw: &str) -> Vec<TimedLine> {
    let mut offset = 0.0;
    let mut lines = Vec::new();
    for source_line in raw.lines() {
        let mut rest = source_line.trim();
        if let Some(value) = rest
            .strip_prefix("[offset:")
            .and_then(|s| s.strip_suffix(']'))
        {
            if let Ok(ms) = value.trim().parse::<i64>() {
                offset = ms as f64 / 1000.0;
            }
            continue;
        }
        let mut timestamps = Vec::new();
        while let Some(after_open) = rest.strip_prefix('[') {
            let Some((stamp, after_close)) = after_open.split_once(']') else {
                break;
            };
            let Some((minutes, seconds)) = stamp.split_once(':') else {
                break;
            };
            let (Ok(minutes), Ok(seconds)) = (minutes.parse::<u64>(), seconds.parse::<f64>())
            else {
                break;
            };
            if !seconds.is_finite() || !(0.0..60.0).contains(&seconds) {
                break;
            }
            timestamps.push(minutes as f64 * 60.0 + seconds + offset);
            rest = after_close;
        }
        if timestamps.is_empty() {
            continue;
        }
        let text = strip_enhanced_timestamps(rest.trim());
        for at in timestamps {
            lines.push(TimedLine {
                at: at.max(0.0),
                text: text.clone(),
            });
        }
    }
    lines.sort_by(|a, b| a.at.total_cmp(&b.at));
    lines
}

fn strip_enhanced_timestamps(text: &str) -> String {
    let mut output = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(open) = rest.find('<') {
        output.push_str(&rest[..open]);
        let after_open = &rest[open + 1..];
        let Some(close) = after_open.find('>') else {
            output.push_str(&rest[open..]);
            return output;
        };
        let stamp = &after_open[..close];
        let valid = stamp.split_once(':').is_some_and(|(minutes, seconds)| {
            minutes.parse::<u64>().is_ok()
                && seconds
                    .parse::<f64>()
                    .is_ok_and(|value| value.is_finite() && (0.0..60.0).contains(&value))
        });
        if !valid {
            output.push_str(&rest[open..open + close + 2]);
        }
        rest = &after_open[close + 1..];
    }
    output.push_str(rest);
    output
}

#[derive(Clone)]
pub struct Client {
    agent: ureq::Agent,
    cache_dir: Option<PathBuf>,
    providers: Vec<String>,
    api_keys: std::collections::HashMap<String, zeroize::Zeroizing<String>>,
}

impl Default for Client {
    fn default() -> Self {
        Self::new()
    }
}

impl Client {
    pub fn new() -> Self {
        Self::with_providers(
            crate::config::LyricsConfig::default().providers,
            std::collections::HashMap::new(),
        )
    }

    pub fn with_providers(
        providers: Vec<String>,
        api_keys: std::collections::HashMap<String, String>,
    ) -> Self {
        let api_keys = api_keys
            .into_iter()
            .map(|(provider, key)| (provider, zeroize::Zeroizing::new(key)))
            .collect();
        Client {
            agent: ureq::AgentBuilder::new()
                .user_agent("wim/0.1 (+https://github.com/nuclearcanopy/wim)")
                .timeout_connect(std::time::Duration::from_secs(10))
                .timeout_read(std::time::Duration::from_secs(20))
                .build(),
            cache_dir: default_cache_dir(),
            providers,
            api_keys,
        }
    }

    /// Exact lookup; lrclib's `/get` endpoint wants all four fields to
    /// find the precise match. `Ok(None)` on a clean "not found" (the
    /// server's 404), `Err` for anything else.
    pub fn get(
        &self,
        artist: &str,
        track: &str,
        album: &str,
        duration_secs: u32,
    ) -> anyhow::Result<Option<Lyrics>> {
        let resp = self
            .agent
            .get(&format!("{BASE_URL}/get"))
            .query("artist_name", artist)
            .query("track_name", track)
            .query("album_name", album)
            .query("duration", &duration_secs.to_string())
            .call();

        match resp {
            Ok(r) => {
                let mut lyrics: Lyrics = r.into_json()?;
                lyrics.source = "lrclib".into();
                Ok(Some(lyrics))
            }
            Err(ureq::Error::Status(404, _)) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    /// Fuzzy fallback for when we don't have an exact duration (e.g. a
    /// local file lofty couldn't read duration from) or `get` found
    /// nothing; never dead-end on a missing duration tag the way an
    /// exact-match-only lookup would. Returns the first match, if any.
    pub fn search(&self, artist: &str, track: &str) -> anyhow::Result<Option<Lyrics>> {
        let results: Vec<Lyrics> = self
            .agent
            .get(&format!("{BASE_URL}/search"))
            .query("artist_name", artist)
            .query("track_name", track)
            .call()?
            .into_json()?;
        Ok(results
            .into_iter()
            .filter_map(|mut lyrics| {
                let score = candidate_score(&lyrics, artist, track);
                (score >= 0.72).then(|| {
                    lyrics.source = "lrclib".into();
                    (score, lyrics)
                })
            })
            .max_by(|a, b| a.0.total_cmp(&b.0))
            .map(|(_, lyrics)| lyrics))
    }

    /// Tries the exact lookup when a duration is known, falling back to
    /// fuzzy search either way nothing came back.
    pub fn lookup(
        &self,
        artist: &str,
        track: &str,
        album: &str,
        duration_secs: Option<u32>,
    ) -> anyhow::Result<Option<Lyrics>> {
        self.lookup_with_path(artist, track, album, duration_secs, None)
    }

    fn lookup_with_path(
        &self,
        artist: &str,
        track: &str,
        album: &str,
        duration_secs: Option<u32>,
        local_path: Option<&Path>,
    ) -> anyhow::Result<Option<Lyrics>> {
        let key = lyrics_cache_key(
            artist,
            track,
            album,
            duration_secs,
            local_path,
            &self.cache_signature(),
        );
        if let Some(cached) = self.read_cache(&key) {
            return Ok(Some(cached));
        }

        let mut best = None;
        let mut last_error = None;
        for provider in &self.providers {
            let result = match provider.as_str() {
                "lrclib" => self.lookup_lrclib(artist, track, album, duration_secs),
                "karalyr" => self.karalyr_lookup(artist, track, album, duration_secs),
                "lyrics-ovh" => self.lyrics_ovh(artist, track),
                "musixmatch" => self
                    .api_keys
                    .get("musixmatch")
                    .map(|key| self.musixmatch(artist, track, album, duration_secs, key))
                    .unwrap_or(Ok(None)),
                "local" => Ok(local_path.and_then(local_lyrics)),
                _ => Ok(None),
            };
            match result {
                Ok(Some(found)) if found.synced.is_some() => {
                    best = Some(found);
                    break;
                }
                Ok(Some(found)) if best.is_none() => best = Some(found),
                Ok(_) => {}
                Err(error) => last_error = Some(error),
            }
        }
        if best.is_none() {
            if let Some(error) = last_error {
                return Err(error);
            }
        }

        if let Some(found) = best.as_ref() {
            self.write_cache(&key, found);
        }
        Ok(best)
    }

    pub fn lookup_subsonic(
        &self,
        server: &crate::subsonic::Client,
        song_id: &str,
        artist: &str,
        track: &str,
        album: &str,
        duration_secs: Option<u32>,
    ) -> anyhow::Result<Option<Lyrics>> {
        let remote_album = format!("{album}\0{}\0{song_id}", server.base_url());
        let key = lyrics_cache_key(
            artist,
            track,
            &remote_album,
            duration_secs,
            None,
            &self.cache_signature(),
        );
        if let Some(cached) = self.read_cache(&key) {
            return Ok(Some(cached));
        }
        let remote = self
            .providers
            .iter()
            .any(|provider| provider == "opensubsonic")
            .then(|| server.get_song_lyrics(song_id).ok())
            .flatten()
            .and_then(|value| subsonic_lyrics(&value));
        if remote
            .as_ref()
            .and_then(|lyrics| lyrics.synced.as_ref())
            .is_some()
        {
            if let Some(found) = remote.as_ref() {
                self.write_cache(&key, found);
            }
            return Ok(remote);
        }
        let online = self.lookup(artist, track, album, duration_secs)?;
        let best = match (online, remote) {
            (Some(online), Some(remote)) if online.synced.is_none() => Some(remote),
            (Some(online), _) => Some(online),
            (None, remote) => remote,
        };
        if let Some(found) = best.as_ref() {
            self.write_cache(&key, found);
        }
        Ok(best)
    }

    fn cache_path(&self, key: &str) -> Option<PathBuf> {
        self.cache_dir
            .as_ref()
            .map(|dir| dir.join(format!("{key}.json")))
    }

    fn cache_signature(&self) -> Vec<String> {
        let mut signature = self.providers.clone();
        let mut keys: Vec<_> = self.api_keys.iter().collect();
        keys.sort_by(|a, b| a.0.cmp(b.0));
        for (provider, key) in keys {
            let digest = Md5::digest(key.as_bytes());
            signature.push(format!("key:{provider}:{digest:x}"));
        }
        signature
    }

    fn read_cache(&self, key: &str) -> Option<Lyrics> {
        let path = self.cache_path(key)?;
        let bytes = std::fs::read(path).ok()?;
        serde_json::from_slice(&bytes).ok()
    }

    fn write_cache(&self, key: &str, lyrics: &Lyrics) {
        if lyrics.source == "musixmatch" {
            return;
        }
        let Some(path) = self.cache_path(key) else {
            return;
        };
        let (Some(parent), Ok(bytes)) = (path.parent(), serde_json::to_vec(lyrics)) else {
            return;
        };
        if std::fs::create_dir_all(parent).is_ok() {
            let temp = path.with_extension("tmp");
            if std::fs::write(&temp, bytes).is_ok() {
                let _ = std::fs::rename(temp, path);
            }
        }
    }

    fn lookup_lrclib(
        &self,
        artist: &str,
        track: &str,
        album: &str,
        duration_secs: Option<u32>,
    ) -> anyhow::Result<Option<Lyrics>> {
        if let Some(duration) = duration_secs {
            if let Some(found) = self.get(artist, track, album, duration)? {
                if found.synced.is_some() {
                    return Ok(Some(found));
                }
                let plain = Some(found);
                return Ok(self.search(artist, track)?.or(plain));
            }
        }
        self.search(artist, track)
    }

    fn karalyr_lookup(
        &self,
        artist: &str,
        track: &str,
        album: &str,
        duration_secs: Option<u32>,
    ) -> anyhow::Result<Option<Lyrics>> {
        if artist.trim().is_empty() || track.trim().is_empty() {
            return Ok(None);
        }
        let mut request = self
            .agent
            .get("https://karalyr.com/api/get")
            .set("X-Karalyr-Client", "wim/0.1")
            .query("artist_name", artist)
            .query("track_name", track);
        if !album.trim().is_empty() {
            request = request.query("album_name", album);
        }
        if let Some(duration) = duration_secs {
            request = request.query("duration", &duration.to_string());
        }
        match request.call() {
            Ok(response) => {
                let mut lyrics: Lyrics = response.into_json()?;
                if lyrics
                    .synced
                    .as_deref()
                    .is_some_and(|text| !parse_synced(text).is_empty())
                {
                    lyrics.source = "Karalyr".into();
                    Ok(Some(lyrics))
                } else {
                    Ok(None)
                }
            }
            Err(ureq::Error::Status(404, _)) => Ok(None),
            Err(error) => Err(error.into()),
        }
    }

    fn lyrics_ovh(&self, artist: &str, track: &str) -> anyhow::Result<Option<Lyrics>> {
        if artist.trim().is_empty() || track.trim().is_empty() {
            return Ok(None);
        }
        let url = format!(
            "https://api.lyrics.ovh/v1/{}/{}",
            url_segment(artist),
            url_segment(track)
        );
        match self.agent.get(&url).call() {
            Ok(response) => {
                #[derive(Deserialize)]
                struct Response {
                    lyrics: String,
                }
                let body: Response = response.into_json()?;
                Ok((!body.lyrics.trim().is_empty()).then_some(Lyrics {
                    plain: Some(body.lyrics),
                    source: "lyrics.ovh".into(),
                    ..Lyrics::default()
                }))
            }
            Err(ureq::Error::Status(404, _)) => Ok(None),
            Err(error) => Err(error.into()),
        }
    }

    fn musixmatch(
        &self,
        artist: &str,
        track: &str,
        album: &str,
        duration: Option<u32>,
        api_key: &str,
    ) -> anyhow::Result<Option<Lyrics>> {
        let mut request = self
            .agent
            .get("https://api.musixmatch.com/ws/1.1/matcher.track.get")
            .query("format", "json")
            .query("q_track", track)
            .query("q_artist", artist)
            .query("q_album", album)
            .query("apikey", api_key);
        if let Some(duration) = duration {
            request = request.query("q_duration", &duration.to_string());
        }
        let matched: serde_json::Value = match request.call() {
            Ok(response) => response.into_json()?,
            Err(ureq::Error::Status(404, _)) => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        let Some(track_data) = matched.pointer("/message/body/track") else {
            return Ok(None);
        };
        if track_data
            .get("restricted")
            .and_then(serde_json::Value::as_i64)
            .is_some_and(|restricted| restricted != 0)
        {
            return Ok(None);
        }
        let Some(commontrack_id) = track_data.get("commontrack_id") else {
            return Ok(None);
        };
        if track_data
            .get("has_subtitles")
            .and_then(serde_json::Value::as_i64)
            == Some(0)
        {
            return Ok(None);
        }
        let response = self
            .agent
            .get("https://api.musixmatch.com/ws/1.1/track.subtitle.get")
            .query("format", "json")
            .query("subtitle_format", "lrc")
            .query("commontrack_id", &commontrack_id.to_string())
            .query("apikey", api_key)
            .call()?;
        let body: serde_json::Value = response.into_json()?;
        let Some(subtitle) = body.pointer("/message/body/subtitle") else {
            return Ok(None);
        };
        let Some(synced) = subtitle
            .get("subtitle_body")
            .and_then(serde_json::Value::as_str)
            .filter(|value| !value.trim().is_empty())
        else {
            return Ok(None);
        };
        if parse_synced(synced).is_empty() {
            return Ok(None);
        }
        let plain = parse_synced(synced)
            .into_iter()
            .map(|line| line.text)
            .collect::<Vec<_>>()
            .join("\n");
        Ok(Some(Lyrics {
            plain: Some(plain),
            synced: Some(synced.to_string()),
            instrumental: false,
            source: "musixmatch".into(),
            attribution: subtitle
                .get("lyrics_copyright")
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned),
            tracking_url: subtitle
                .get("pixel_tracking_url")
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned),
            ..Lyrics::default()
        }))
    }

    /// Musixmatch requires a pixel view event whenever its lyrics are
    /// displayed. Called off the UI thread; this also avoids repeating
    /// the event when a user scrolls or resizes the lyrics pane.
    pub fn track_display(&self, lyrics: &Lyrics) {
        let Some(url) = lyrics.tracking_url.clone() else {
            return;
        };
        let agent = self.agent.clone();
        thread::spawn(move || {
            let _ = agent.get(&url).call();
        });
    }
}

fn url_segment(value: &str) -> String {
    let mut encoded = String::new();
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~') {
            encoded.push(byte as char);
        } else {
            encoded.push_str(&format!("%{byte:02X}"));
        }
    }
    encoded
}

fn default_cache_dir() -> Option<PathBuf> {
    #[cfg(test)]
    {
        None
    }
    #[cfg(not(test))]
    {
        dirs::cache_dir().map(|p| p.join("wim").join("lyrics"))
    }
}

fn local_lyrics(path: &Path) -> Option<Lyrics> {
    let parent = path.parent()?;
    let stem = path.file_stem()?.to_string_lossy();
    for extension in ["lrc", "txt"] {
        let mut candidates = vec![parent.join(format!("{stem}.{extension}"))];
        candidates.push(path.with_extension(extension));
        if let Some(name) = path.file_name() {
            candidates.push(parent.join(format!("{}.{extension}", name.to_string_lossy())));
        }
        for candidate in candidates {
            let Ok(text) = std::fs::read_to_string(candidate) else {
                continue;
            };
            if text.trim().is_empty() {
                continue;
            }
            let synced =
                (extension == "lrc" && !parse_synced(&text).is_empty()).then_some(text.clone());
            let plain = if let Some(synced) = synced.as_deref() {
                Some(
                    parse_synced(synced)
                        .into_iter()
                        .map(|line| line.text)
                        .collect::<Vec<_>>()
                        .join("\n"),
                )
            } else {
                Some(text)
            };
            return Some(Lyrics {
                plain,
                synced,
                source: format!("local .{extension}"),
                ..Lyrics::default()
            });
        }
    }

    let tagged = Probe::open(path).ok()?.read().ok()?;
    let text = tagged
        .primary_tag()
        .or_else(|| tagged.first_tag())?
        .get_string(&ItemKey::Lyrics)?
        .to_owned();
    let synced = (!parse_synced(&text).is_empty()).then_some(text.clone());
    let plain = if synced.is_some() {
        Some(
            parse_synced(&text)
                .into_iter()
                .map(|line| line.text)
                .collect::<Vec<_>>()
                .join("\n"),
        )
    } else {
        Some(text)
    };
    Some(Lyrics {
        plain,
        synced,
        source: "embedded tag".into(),
        ..Lyrics::default()
    })
}

fn subsonic_lyrics(value: &serde_json::Value) -> Option<Lyrics> {
    let entries = value.pointer("/lyricsList/structuredLyrics")?.as_array()?;
    let mut plain = None;
    for entry in entries {
        let lines = entry.get("line")?.as_array()?;
        if lines.is_empty() {
            continue;
        }
        let text = lines
            .iter()
            .filter_map(|line| line.get("value").and_then(serde_json::Value::as_str))
            .collect::<Vec<_>>();
        if text.is_empty() {
            continue;
        }
        let text = text.join("\n");
        if entry.get("synced").and_then(serde_json::Value::as_bool) == Some(true) {
            let offset = entry
                .get("offset")
                .and_then(serde_json::Value::as_i64)
                .unwrap_or(0);
            let synced = lines
                .iter()
                .filter_map(|line| {
                    let ms = line.get("start")?.as_u64()? as i64 + offset;
                    let value = line.get("value")?.as_str()?;
                    let ms = ms.max(0);
                    Some(format!(
                        "[{:02}:{:02}.{:02}]{value}",
                        ms / 60_000,
                        (ms / 1_000) % 60,
                        (ms % 1_000) / 10
                    ))
                })
                .collect::<Vec<_>>()
                .join("\n");
            if !parse_synced(&synced).is_empty() {
                return Some(Lyrics {
                    plain: Some(text),
                    synced: Some(synced),
                    source: "OpenSubsonic".into(),
                    ..Lyrics::default()
                });
            }
        } else if plain.is_none() {
            plain = Some(Lyrics {
                plain: Some(text),
                source: "OpenSubsonic".into(),
                ..Lyrics::default()
            });
        }
    }
    plain
}

fn lyrics_cache_key(
    artist: &str,
    track: &str,
    album: &str,
    duration_secs: Option<u32>,
    local_path: Option<&Path>,
    providers: &[String],
) -> String {
    let local = local_path
        .map(|path| {
            let stamp = std::fs::metadata(path)
                .and_then(|metadata| metadata.modified())
                .ok()
                .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|duration| duration.as_secs())
                .unwrap_or_default();
            format!("{}\0{stamp}", path.display())
        })
        .unwrap_or_default();
    let mut digest = Md5::new();
    digest.update(format!(
        "{}\0{}\0{}\0{}\0{}\0{}",
        artist.trim().to_lowercase(),
        track.trim().to_lowercase(),
        album.trim().to_lowercase(),
        duration_secs.unwrap_or_default(),
        local,
        providers.join("\0")
    ));
    format!("{:x}", digest.finalize())
}

/// A lyrics fetch that does not block the input path.
///
/// `Client::lookup` is a network round trip and it used to be called
/// straight from the key handler, so pressing the lyrics key froze the
/// whole ui until lrclib answered. Same shape as `browse::Loader`,
/// deliberately: one generation counter, bumped on every request and
/// on every `invalidate`, checked by the worker before it sends and
/// again in `poll`, so a reply for a track the user has already moved
/// on from is dropped instead of appearing over the new one.
///
/// **The blanket `#[cfg_attr(not(test), allow(dead_code))]` this used
/// to carry is gone**, and that was not cosmetic: it covered the whole
/// impl, so `invalidate` having no production caller for its entire
/// life was never flagged, and the pane reopened itself over the next
/// track. What is still unreached in the binary is annotated one item
/// at a time now, so the next one is a warning.
pub struct Loader {
    /// Shared with the workers so a superseded fetch can give up
    /// before it sends, rather than only being filtered on arrival.
    generation: Arc<AtomicU64>,
    tx: Sender<Fetched>,
    rx: Receiver<Fetched>,
    /// The track a fetch is in flight for; `None` once it has been
    /// delivered or disowned, which is the one reading `invalidate`
    /// is observable through.
    pending: Option<String>,
}

struct Fetched {
    generation: u64,
    track: String,
    result: Result<Option<Lyrics>, String>,
}

impl Default for Loader {
    fn default() -> Self {
        Self::new()
    }
}

impl Loader {
    pub fn new() -> Self {
        let (tx, rx) = mpsc::channel();
        Loader {
            generation: Arc::new(AtomicU64::new(0)),
            tx,
            rx,
            pending: None,
        }
    }

    /// Starts a fetch, superseding anything already in flight. The
    /// label is what `pending` reports and what comes back with the
    /// reply, so the caller can tell which track answered without
    /// holding its own copy.
    pub fn request(
        &mut self,
        client: &Client,
        label: impl Into<String>,
        artist: &str,
        title: &str,
        album: &str,
        duration_secs: Option<u32>,
    ) {
        self.request_with_path(client, label, artist, title, album, duration_secs, None);
    }

    pub fn request_local(
        &mut self,
        client: &Client,
        label: impl Into<String>,
        artist: &str,
        title: &str,
        album: &str,
        duration_secs: Option<u32>,
        path: &Path,
    ) {
        self.request_with_path(
            client,
            label,
            artist,
            title,
            album,
            duration_secs,
            Some(path.to_path_buf()),
        );
    }

    fn request_with_path(
        &mut self,
        client: &Client,
        label: impl Into<String>,
        artist: &str,
        title: &str,
        album: &str,
        duration_secs: Option<u32>,
        local_path: Option<PathBuf>,
    ) {
        let label = label.into();
        let client = client.clone();
        let (artist, title, album) = (artist.to_owned(), title.to_owned(), album.to_owned());
        self.start_request(label, move || {
            client.lookup_with_path(
                &artist,
                &title,
                &album,
                duration_secs,
                local_path.as_deref(),
            )
        });
    }

    pub fn request_subsonic(
        &mut self,
        client: &Client,
        server: Arc<crate::subsonic::Client>,
        song_id: String,
        label: impl Into<String>,
        artist: &str,
        title: &str,
        album: &str,
        duration_secs: Option<u32>,
    ) {
        let label = label.into();
        let lyrics_client = client.clone();
        let (artist, title, album) = (artist.to_owned(), title.to_owned(), album.to_owned());
        self.start_request(label, move || {
            lyrics_client.lookup_subsonic(&server, &song_id, &artist, &title, &album, duration_secs)
        });
    }

    fn start_request(
        &mut self,
        label: String,
        lookup: impl FnOnce() -> anyhow::Result<Option<Lyrics>> + Send + 'static,
    ) {
        // bumping first is what makes the previous fetch stale; its
        // worker sees the change and drops its own result.
        let generation = self.generation.fetch_add(1, Ordering::SeqCst) + 1;
        self.pending = Some(label.clone());

        let tx = self.tx.clone();
        let shared = Arc::clone(&self.generation);
        thread::spawn(move || {
            let result = lookup().map_err(|e| e.to_string());
            // checked before sending: a user who pressed the key twice
            // should not pay for the first answer being delivered.
            if shared.load(Ordering::SeqCst) != generation {
                return;
            }
            // a dead receiver only means the app is shutting down.
            let _ = tx.send(Fetched {
                generation,
                track: label,
                result,
            });
        });
    }

    /// The track a fetch is waiting on, if any.
    ///
    /// Still read by tests only: both front-ends report "fetching
    /// lyrics for X" from their own copy of the label at the keypress
    /// instead, so nothing on screen is derived from this. Annotated
    /// here rather than over the whole impl, which is what hid
    /// `invalidate` having no caller.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn pending(&self) -> Option<&str> {
        self.pending.as_deref()
    }

    /// Takes delivery of a reply for the current request, dropping any
    /// that belongs to a superseded one. Returns the label it was
    /// requested under alongside the answer.
    pub fn poll(&mut self) -> Option<(String, Result<Option<Lyrics>, String>)> {
        let current = self.generation.load(Ordering::SeqCst);
        loop {
            match self.rx.try_recv() {
                Ok(fetched) => {
                    if fetched.generation != current {
                        // a reply from a request the user moved past.
                        continue;
                    }
                    self.pending = None;
                    return Some((fetched.track, fetched.result));
                }
                Err(TryRecvError::Empty) | Err(TryRecvError::Disconnected) => return None,
            }
        }
    }

    /// Disowns whatever is in flight: the lyrics view was closed, or
    /// the track changed, so an answer for it must not arrive later.
    ///
    /// `core::track_started` is the track-changed caller. The
    /// view-closed one is the front-end's: `LyricsPane::close` drops
    /// the lines and knows nothing about the loader, so a host that
    /// closes the pane has to call this in the same breath or the next
    /// reply reopens it.
    pub fn invalidate(&mut self) {
        self.generation.fetch_add(1, Ordering::SeqCst);
        self.pending = None;
    }

    /// Hands a reply straight to the loader as if a worker had sent
    /// it, so the generation rules are testable without a network.
    /// Same `deliver` precedent `coverart::Cache` uses.
    #[cfg(test)]
    fn deliver(&self, generation: u64, track: &str, result: Result<Option<Lyrics>, String>) {
        let _ = self.tx.send(Fetched {
            generation,
            track: track.to_string(),
            result,
        });
    }

    #[cfg(test)]
    fn generation(&self) -> u64 {
        self.generation.load(Ordering::SeqCst)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // captured from a real lrclib.net /api/get response, trimmed;
    // exercises the real field names/shape without hitting the network
    // in tests.
    const SAMPLE_RESPONSE: &str = r#"{
        "id": 496,
        "name": "Creep",
        "trackName": "Creep",
        "artistName": "Radiohead",
        "albumName": "Pablo Honey",
        "duration": 239.0,
        "instrumental": false,
        "hasWordSync": false,
        "plainLyrics": "When you were here before\n",
        "syncedLyrics": "[00:19.16] When you were here before\n"
    }"#;

    #[test]
    fn parses_a_real_response_shape() {
        let lyrics: Lyrics = serde_json::from_str(SAMPLE_RESPONSE).unwrap();
        assert!(!lyrics.instrumental);
        assert_eq!(lyrics.plain.unwrap(), "When you were here before\n");
        assert!(lyrics.synced.unwrap().starts_with("[00:19.16]"));
    }

    #[test]
    fn instrumental_track_can_have_no_lyrics_fields() {
        let json = r#"{"instrumental": true, "plainLyrics": null, "syncedLyrics": null}"#;
        let lyrics: Lyrics = serde_json::from_str(json).unwrap();
        assert!(lyrics.instrumental);
        assert!(lyrics.plain.is_none());
        assert!(lyrics.synced.is_none());
    }

    // -------------------------------------------------------- loader

    fn some_lyrics() -> Lyrics {
        Lyrics {
            plain: Some("a line".into()),
            synced: None,
            instrumental: false,
            source: String::new(),
            attribution: None,
            tracking_url: None,
            track_name: None,
            artist_name: None,
            album_name: None,
            duration: None,
        }
    }

    #[test]
    fn parses_lrc_offsets_repeated_timestamps_and_fractions() {
        let parsed =
            parse_synced("[ar:Artist]\n[offset:-100]\n[00:01.2][00:03.45]same\n[01:02.003]next");
        assert_eq!(parsed.len(), 3);
        assert!((parsed[0].at - 1.1).abs() < 0.001);
        assert!((parsed[1].at - 3.35).abs() < 0.001);
        assert!((parsed[2].at - 61.903).abs() < 0.001);
        assert_eq!(parsed[0].text, "same");
        assert_eq!(parsed[2].text, "next");
    }

    #[test]
    fn local_lrc_sidecar_is_a_synced_source() {
        let dir = std::env::temp_dir().join(format!("wim-lyrics-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let audio = dir.join("song.flac");
        let sidecar = dir.join("song.lrc");
        std::fs::write(&audio, "fixture").unwrap();
        std::fs::write(&sidecar, "[00:01.00]first\n[00:02.00]second\n").unwrap();
        let found = local_lyrics(&audio).unwrap();
        assert_eq!(found.source, "local .lrc");
        assert_eq!(parse_synced(found.synced.as_deref().unwrap()).len(), 2);
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn opensubsonic_structured_lyrics_keep_timestamps() {
        let value = serde_json::json!({
            "lyricsList": { "structuredLyrics": [{
                "synced": true,
                "offset": -100,
                "line": [
                    {"start": 1000, "value": "first"},
                    {"start": 2500, "value": "second"}
                ]
            }] }
        });
        let found = subsonic_lyrics(&value).unwrap();
        let lines = parse_synced(found.synced.as_deref().unwrap());
        assert_eq!(lines[0].text, "first");
        assert!((lines[0].at - 0.9).abs() < 0.001);
        assert_eq!(found.source, "OpenSubsonic");
    }

    #[test]
    fn a_reply_for_the_current_request_lands() {
        let mut l = Loader::new();
        l.request(&Client::new(), "crest", "bladee", "crest", "crest", None);
        let gen = l.generation();
        assert_eq!(l.pending(), Some("crest"));

        l.deliver(gen, "crest", Ok(Some(some_lyrics())));
        let (track, result) = l.poll().expect("a current reply should land");
        assert_eq!(track, "crest");
        assert!(result.unwrap().is_some());
        assert_eq!(l.pending(), None, "it is no longer in flight");
    }

    /// The guard that matters: press the key on one track, then
    /// another, and the first answer must not appear over the second.
    #[test]
    fn a_reply_from_a_superseded_request_is_dropped() {
        let mut l = Loader::new();
        let client = Client::new();
        l.request(&client, "first", "a", "first", "al", None);
        let stale = l.generation();
        l.request(&client, "second", "b", "second", "al", None);

        l.deliver(stale, "first", Ok(Some(some_lyrics())));
        assert!(
            l.poll().is_none(),
            "the first track's lyrics landed on the second"
        );
        assert_eq!(l.pending(), Some("second"), "the live fetch is untouched");
    }

    /// Closing the view, or the track changing, disowns what is in
    /// flight so it cannot arrive afterwards.
    #[test]
    fn invalidate_disowns_whatever_is_in_flight() {
        let mut l = Loader::new();
        l.request(&Client::new(), "crest", "bladee", "crest", "crest", None);
        let gen = l.generation();
        l.invalidate();

        l.deliver(gen, "crest", Ok(Some(some_lyrics())));
        assert!(l.poll().is_none());
        assert_eq!(l.pending(), None);
    }

    #[test]
    fn an_error_is_delivered_rather_than_swallowed() {
        let mut l = Loader::new();
        l.request(&Client::new(), "crest", "bladee", "crest", "crest", None);
        let gen = l.generation();
        l.deliver(gen, "crest", Err("502 bad gateway".into()));
        let (_, result) = l.poll().expect("an error is still an answer");
        assert!(result.unwrap_err().contains("502"));
    }

    #[test]
    fn polling_an_idle_loader_is_none() {
        let mut l = Loader::new();
        assert!(l.poll().is_none());
        assert_eq!(l.pending(), None);
    }
}
