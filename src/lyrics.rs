//! Auto lyrics lookup via lrclib.net — free, no api key required, so
//! there's no reason to ship without this by default. See
//! docs/features.md.

use serde::Deserialize;

const BASE_URL: &str = "https://lrclib.net/api";

#[derive(Debug, Clone, Deserialize)]
pub struct Lyrics {
    #[serde(rename = "plainLyrics")]
    pub plain: Option<String>,
    #[serde(rename = "syncedLyrics")]
    pub synced: Option<String>,
    pub instrumental: bool,
}

pub struct Client {
    agent: ureq::Agent,
}

impl Default for Client {
    fn default() -> Self {
        Self::new()
    }
}

impl Client {
    pub fn new() -> Self {
        Client {
            agent: ureq::AgentBuilder::new()
                .user_agent("wim/0.1 (+https://github.com/nuclearcanopy/wim)")
                .build(),
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
            Ok(r) => Ok(Some(r.into_json()?)),
            Err(ureq::Error::Status(404, _)) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    /// Fuzzy fallback for when we don't have an exact duration (e.g. a
    /// local file lofty couldn't read duration from) or `get` found
    /// nothing — never dead-end on a missing duration tag the way an
    /// exact-match-only lookup would. Returns the first match, if any.
    pub fn search(&self, artist: &str, track: &str) -> anyhow::Result<Option<Lyrics>> {
        let results: Vec<Lyrics> = self
            .agent
            .get(&format!("{BASE_URL}/search"))
            .query("artist_name", artist)
            .query("track_name", track)
            .call()?
            .into_json()?;
        Ok(results.into_iter().next())
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
        if let Some(d) = duration_secs {
            if let Some(found) = self.get(artist, track, album, d)? {
                return Ok(Some(found));
            }
        }
        self.search(artist, track)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // captured from a real lrclib.net /api/get response, trimmed —
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
}
