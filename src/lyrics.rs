//! Auto lyrics lookup via lrclib.net; free, no api key required, so
//! there's no reason to ship without this by default. See
//! docs/features.md.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::sync::Arc;
use std::thread;

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

#[derive(Clone)]
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
        let label = label.into();
        // bumping first is what makes the previous fetch stale; its
        // worker sees the change and drops its own result.
        let generation = self.generation.fetch_add(1, Ordering::SeqCst) + 1;
        self.pending = Some(label.clone());

        let client = client.clone();
        let tx = self.tx.clone();
        let shared = Arc::clone(&self.generation);
        let (artist, title, album) = (artist.to_string(), title.to_string(), album.to_string());
        thread::spawn(move || {
            let result = client
                .lookup(&artist, &title, &album, duration_secs)
                .map_err(|e| e.to_string());
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
        }
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
