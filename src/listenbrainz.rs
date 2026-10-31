//! ListenBrainz scrobbling client. Submits "playing_now" (fired when a
//! track starts) and "single" (fired once the configured scrobble
//! threshold is crossed) listens to ListenBrainz's submit-listens
//! endpoint.
//!
//! Unlike `opensubtitles.rs`, this shape is verified against the real
//! docs, not guessed: the endpoint, auth header, and `listen_type`
//! values are from
//! <https://listenbrainz.readthedocs.io/en/latest/users/api/core.html#post--1-submit-listens>;
//! the payload/`track_metadata` shape (including that `listened_at`
//! must be omitted for `playing_now` but is a required unix timestamp
//! for `single`) is from
//! <https://listenbrainz.readthedocs.io/en/latest/users/json.html>.

use std::time::{SystemTime, UNIX_EPOCH};

use serde::Serialize;

use crate::config::{Config, ScrobbleConfig};

const BASE_URL: &str = "https://api.listenbrainz.org";
const USER_AGENT: &str = "wim v0.1.0";

/// What main.rs maps a `SearchItem` down to before handing it here;
/// this module knows nothing about `SearchItem` itself, same split as
/// `subsonic.rs` not knowing about the app's own playback state.
#[derive(Debug, Clone)]
pub struct TrackMetadata {
    pub artist_name: String,
    pub track_name: String,
    pub release_name: Option<String>,
}

pub struct Client {
    agent: ureq::Agent,
    token: String,
}

impl std::fmt::Debug for Client {
    /// Hand-written so the token never ends up in a `{:?}` log line,
    /// same reasoning as `subsonic::Client`'s password.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Client").finish()
    }
}

impl Client {
    pub fn new(token: String) -> Self {
        Client {
            agent: ureq::AgentBuilder::new().user_agent(USER_AGENT).build(),
            token,
        }
    }

    /// `None` if no token is configured (plaintext or file); scrobbling
    /// is simply disabled in that case, the same "neither set means the
    /// feature never runs" convention as `opensubtitles_api_key`.
    pub fn from_config(cfg: &Config) -> Option<Self> {
        Some(Client::new(cfg.resolve_listenbrainz_user_token()?))
    }

    /// Fired when a track starts. `listened_at` must be omitted
    /// entirely for `playing_now`, per the json docs linked above.
    pub fn submit_playing_now(&self, meta: &TrackMetadata) -> anyhow::Result<()> {
        self.submit("playing_now", meta, None)
    }

    /// Fired once per track, the moment the scrobble threshold is
    /// crossed. `listened_at` is a required unix timestamp for
    /// `single`; callers should pass the time the track actually
    /// started (not the time this call happens to fire).
    pub fn submit_single(&self, meta: &TrackMetadata, listened_at: i64) -> anyhow::Result<()> {
        self.submit("single", meta, Some(listened_at))
    }

    fn submit(
        &self,
        listen_type: &str,
        meta: &TrackMetadata,
        listened_at: Option<i64>,
    ) -> anyhow::Result<()> {
        let body = build_submit_body(listen_type, meta, listened_at);
        self.agent
            .post(&format!("{}/1/submit-listens", base_url()))
            .set("Authorization", &format!("Token {}", self.token))
            .send_json(body)?;
        Ok(())
    }
}

/// Where listens go: the real api in every build, a local collector
/// under test.
///
/// The seam exists because the risk here is *which items reach a
/// submission at all*, and that is decided in `core::fire_scrobble` /
/// `core::fire_playing_now`, not in this file: without it the only
/// honest test of the wiring would be a public ListenBrainz profile.
/// `pub(crate)` for the same reason `TrackResolver`'s test seam wants
/// to be: the code under test lives in another module.
#[cfg(test)]
static BASE_URL_OVERRIDE: std::sync::OnceLock<String> = std::sync::OnceLock::new();

/// Set once per test process, never unset: a submission that escaped
/// to the real api would be a listen on someone's profile.
#[cfg(test)]
pub(crate) fn redirect_submissions_to(url: &str) {
    let _ = BASE_URL_OVERRIDE.set(url.to_string());
}

fn base_url() -> String {
    #[cfg(test)]
    {
        if let Some(url) = BASE_URL_OVERRIDE.get() {
            return url.clone();
        }
    }
    BASE_URL.to_string()
}

/// Now, as a unix timestamp; used as the `single` submission's
/// `listened_at` when the caller has no better (track-start) time on
/// hand.
pub fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// ListenBrainz's own convention: scrobble once at least half the
/// track has played, or 4 minutes have elapsed, whichever comes first,
/// provided the track itself is longer than 30 seconds (a fixed floor
/// below which nothing is ever worth scrobbling; not itself exposed as
/// a setting). The percent/seconds thresholds this checks against are
/// `cfg`'s tunable values, which default to exactly this behavior.
pub fn should_scrobble(cfg: &ScrobbleConfig, elapsed_secs: f64, duration_secs: f64) -> bool {
    const MIN_TRACK_SECS: f64 = 30.0;
    if duration_secs <= MIN_TRACK_SECS {
        return false;
    }
    let by_percent = elapsed_secs >= duration_secs * (cfg.threshold_percent / 100.0);
    let by_seconds = elapsed_secs >= cfg.threshold_seconds;
    by_percent || by_seconds
}

#[derive(Serialize)]
struct SubmitBody<'a> {
    listen_type: &'a str,
    payload: Vec<ListenPayload<'a>>,
}

#[derive(Serialize)]
struct ListenPayload<'a> {
    #[serde(skip_serializing_if = "Option::is_none")]
    listened_at: Option<i64>,
    track_metadata: TrackMetadataPayload<'a>,
}

#[derive(Serialize)]
struct TrackMetadataPayload<'a> {
    artist_name: &'a str,
    track_name: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    release_name: Option<&'a str>,
}

/// Pure payload construction, pulled out of `submit()` so the exact
/// json shape is testable without a live server.
fn build_submit_body<'a>(
    listen_type: &'a str,
    meta: &'a TrackMetadata,
    listened_at: Option<i64>,
) -> SubmitBody<'a> {
    SubmitBody {
        listen_type,
        payload: vec![ListenPayload {
            listened_at,
            track_metadata: TrackMetadataPayload {
                artist_name: &meta.artist_name,
                track_name: &meta.track_name,
                release_name: meta.release_name.as_deref(),
            },
        }],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn meta() -> TrackMetadata {
        TrackMetadata {
            artist_name: "Bladee".into(),
            track_name: "Icedancer".into(),
            release_name: Some("Cold Visions".into()),
        }
    }

    #[test]
    fn playing_now_omits_listened_at_entirely() {
        let m = meta();
        let body = build_submit_body("playing_now", &m, None);
        let v = serde_json::to_value(&body).unwrap();
        assert_eq!(v["listen_type"], "playing_now");
        assert!(v["payload"][0].get("listened_at").is_none());
        assert_eq!(v["payload"][0]["track_metadata"]["artist_name"], "Bladee");
        assert_eq!(v["payload"][0]["track_metadata"]["track_name"], "Icedancer");
        assert_eq!(
            v["payload"][0]["track_metadata"]["release_name"],
            "Cold Visions"
        );
    }

    #[test]
    fn single_includes_the_required_listened_at_timestamp() {
        let m = meta();
        let body = build_submit_body("single", &m, Some(1_443_521_965));
        let v = serde_json::to_value(&body).unwrap();
        assert_eq!(v["listen_type"], "single");
        assert_eq!(v["payload"][0]["listened_at"], 1_443_521_965);
    }

    #[test]
    fn release_name_is_omitted_when_unknown_rather_than_null() {
        let mut m = meta();
        m.release_name = None;
        let body = build_submit_body("single", &m, Some(1));
        let v = serde_json::to_value(&body).unwrap();
        assert!(v["payload"][0]["track_metadata"]
            .get("release_name")
            .is_none());
    }

    #[test]
    fn should_scrobble_fires_at_half_the_track_by_default() {
        let cfg = ScrobbleConfig::default();
        assert!(!should_scrobble(&cfg, 90.0, 200.0)); // 45%, under half
        assert!(should_scrobble(&cfg, 101.0, 200.0)); // just past half
    }

    #[test]
    fn should_scrobble_fires_at_four_minutes_for_a_long_track_by_default() {
        let cfg = ScrobbleConfig::default();
        // half of a 20-minute track is 10 minutes, far later than the
        // 4-minute (240s) cap; the cap should win.
        assert!(!should_scrobble(&cfg, 239.0, 1200.0));
        assert!(should_scrobble(&cfg, 241.0, 1200.0));
    }

    #[test]
    fn should_scrobble_never_fires_for_tracks_at_or_under_thirty_seconds() {
        let cfg = ScrobbleConfig::default();
        assert!(!should_scrobble(&cfg, 30.0, 30.0));
        assert!(!should_scrobble(&cfg, 29.0, 29.0));
    }

    #[test]
    fn custom_thresholds_are_honored_over_the_listenbrainz_default() {
        let cfg = ScrobbleConfig {
            threshold_percent: 90.0,
            threshold_seconds: 10.0,
        };
        // 10s threshold is crossed well before 90% of a 200s track.
        assert!(should_scrobble(&cfg, 11.0, 200.0));
        assert!(!should_scrobble(&cfg, 9.0, 200.0));
    }
}

/// Which items reach a submission at all, through the two functions
/// that actually fire one (`core::fire_playing_now` /
/// `core::fire_scrobble`), against a local collector.
///
/// `build_submit_body` was fully covered and this was not, which is
/// the gap that mattered: `Silence` exists as its own `SearchItem`
/// variant *specifically* so a fifteen-minute gap in the queue is
/// never scrobbled as a track called "silence", and nothing checked
/// it anywhere. A listen is also irreversible from here: it lands on
/// a public profile.
#[cfg(test)]
mod submission_wiring {
    use super::*;
    use std::io::{BufRead, BufReader, Read, Write};
    use std::net::{TcpListener, TcpStream};
    use std::sync::{Arc, Mutex, OnceLock};
    use std::thread;
    use std::time::{Duration, Instant};

    use crate::browse::fixtures::test_client;
    use crate::search::SearchItem;

    /// One submission as the server saw it.
    #[derive(Debug, Clone)]
    struct Listen {
        auth: String,
        body: serde_json::Value,
    }

    impl Listen {
        fn track(&self) -> String {
            self.body["payload"][0]["track_metadata"]["track_name"]
                .as_str()
                .unwrap_or_default()
                .to_string()
        }
    }

    /// Minimal http/1.1 collector, same shape as `download`'s
    /// `ByteServer` and `subsonic`'s `MockServer`: it answers 200 and
    /// keeps what it was sent.
    struct Collector {
        url: String,
        listens: Arc<Mutex<Vec<Listen>>>,
    }

    impl Collector {
        fn start() -> Collector {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let url = format!("http://{}", listener.local_addr().unwrap());
            let listens: Arc<Mutex<Vec<Listen>>> = Arc::new(Mutex::new(Vec::new()));
            let seen = listens.clone();
            // detached: the test binary exits without waiting on it
            thread::spawn(move || {
                for stream in listener.incoming().flatten() {
                    let seen = seen.clone();
                    thread::spawn(move || serve(stream, &seen));
                }
            });
            Collector { url, listens }
        }

        fn all(&self) -> Vec<Listen> {
            self.listens.lock().unwrap().clone()
        }

        /// Waits for the listen naming `track` and returns it. Every
        /// assertion that nothing was submitted needs one of these
        /// first: it is the positive control that proves the pipeline
        /// was working at the moment the other item was refused.
        fn wait_for(&self, track: &str) -> Listen {
            let deadline = Instant::now() + Duration::from_secs(5);
            while Instant::now() < deadline {
                if let Some(found) = self.all().into_iter().find(|l| l.track() == track) {
                    return found;
                }
                thread::sleep(Duration::from_millis(10));
            }
            panic!("no listen for {track:?} arrived; got {:?}", self.all());
        }
    }

    fn serve(stream: TcpStream, seen: &Mutex<Vec<Listen>>) {
        let mut reader = BufReader::new(stream.try_clone().unwrap());
        let mut writer = stream;
        loop {
            let mut line = String::new();
            if reader.read_line(&mut line).unwrap_or(0) == 0 {
                return;
            }
            let (mut length, mut auth) = (0usize, String::new());
            loop {
                let mut header = String::new();
                match reader.read_line(&mut header) {
                    Ok(0) => return,
                    Ok(_) if header.trim().is_empty() => break,
                    Ok(_) => {
                        let lower = header.to_ascii_lowercase();
                        if let Some(v) = lower.strip_prefix("content-length:") {
                            length = v.trim().parse().unwrap_or(0);
                        }
                        if lower.starts_with("authorization:") {
                            auth = header[header.find(':').unwrap() + 1..].trim().to_string();
                        }
                    }
                    Err(_) => return,
                }
            }
            let mut body = vec![0u8; length];
            if reader.read_exact(&mut body).is_err() {
                return;
            }
            let body = serde_json::from_slice(&body).unwrap_or(serde_json::Value::Null);
            seen.lock().unwrap().push(Listen { auth, body });
            let reply = "{\"status\":\"ok\"}";
            let head = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{reply}",
                reply.len()
            );
            if writer.write_all(head.as_bytes()).is_err() {
                return;
            }
        }
    }

    /// One collector for the whole module, because the redirect is
    /// process-wide; every test names its own tracks so the listens
    /// never have to be told apart by arrival order.
    fn collector() -> &'static Collector {
        static COLLECTOR: OnceLock<Collector> = OnceLock::new();
        COLLECTOR.get_or_init(|| {
            let c = Collector::start();
            redirect_submissions_to(&c.url);
            c
        })
    }

    fn configured() -> Config {
        Config {
            listenbrainz_user_token: Some("a-test-token".into()),
            ..Config::default()
        }
    }

    fn song(title: &str) -> SearchItem {
        SearchItem::Song {
            id: "song-1".into(),
            title: title.into(),
            artist: "Bladee".into(),
            album: "Cold Visions".into(),
            client: test_client(),
        }
    }

    /// The rule the `Silence` variant was invented for, at the only
    /// place that can break it: a queued gap must never become a
    /// listen on a public profile.
    ///
    /// Fired *before* the control song and asserted after the control
    /// has landed, so "nothing arrived" cannot mean "nothing had
    /// arrived yet".
    #[test]
    fn a_queued_silence_is_never_submitted_as_a_listen() {
        let server = collector();
        let cfg = configured();
        let fifteen_minutes = SearchItem::Silence { secs: 900 };

        crate::core::fire_playing_now(&cfg, &fifteen_minutes);
        crate::core::fire_scrobble(&cfg, &fifteen_minutes, 1_443_521_965);
        crate::core::fire_playing_now(&cfg, &song("the silence control"));

        server.wait_for("the silence control");
        thread::sleep(Duration::from_millis(100));
        let silences: Vec<String> = server
            .all()
            .iter()
            .map(|l| l.track())
            .filter(|t| t.to_lowercase().contains("silence") && t != "the silence control")
            .collect();
        assert!(
            silences.is_empty(),
            "a silence reached listenbrainz as {silences:?}"
        );
    }

    /// A real track still gets through, with the fields in the right
    /// places: nothing above is allowed to pass by submitting nothing
    /// at all.
    #[test]
    fn a_remote_song_is_submitted_with_its_own_metadata() {
        let server = collector();
        crate::core::fire_scrobble(&configured(), &song("Icedancer"), 1_443_521_965);

        let listen = server.wait_for("Icedancer");
        assert_eq!(listen.auth, "Token a-test-token");
        assert_eq!(listen.body["listen_type"], "single");
        assert_eq!(listen.body["payload"][0]["listened_at"], 1_443_521_965);
        let meta = &listen.body["payload"][0]["track_metadata"];
        assert_eq!(meta["artist_name"], "Bladee");
        assert_eq!(meta["track_name"], "Icedancer");
        assert_eq!(meta["release_name"], "Cold Visions");
    }

    /// `playing_now` is the other half of the pair, and it must not
    /// carry a timestamp (the api rejects the submission if it does).
    #[test]
    fn a_track_start_is_submitted_as_playing_now_with_no_timestamp() {
        let server = collector();
        crate::core::fire_playing_now(&configured(), &song("Be Nice 2 Me"));

        let listen = server.wait_for("Be Nice 2 Me");
        assert_eq!(listen.body["listen_type"], "playing_now");
        assert!(
            listen.body["payload"][0].get("listened_at").is_none(),
            "{:?}",
            listen.body
        );
    }

    /// No token means the feature is off, not "submitted anonymously":
    /// `from_config` is the gate, and the same song goes through once
    /// a token is there, which is what keeps this from passing for the
    /// wrong reason.
    #[test]
    fn nothing_is_submitted_when_no_token_is_configured() {
        let server = collector();
        let untitled = "Girls Just Want to Have Fun";
        crate::core::fire_scrobble(&Config::default(), &song(untitled), 1);
        crate::core::fire_playing_now(&configured(), &song("the token control"));

        server.wait_for("the token control");
        thread::sleep(Duration::from_millis(100));
        assert!(
            !server.all().iter().any(|l| l.track() == untitled),
            "submitted with no token: {:?}",
            server.all()
        );
    }

    /// A local file with no artist tag cannot be a listen either:
    /// `artist_name` is required, and a listen with an empty artist is
    /// a row nobody can read on their own profile.
    #[test]
    fn local_music_with_no_artist_tag_is_not_submitted() {
        let server = collector();
        let untagged = SearchItem::LocalMusic {
            path: std::path::PathBuf::from("/music/untagged.flac"),
            title: "an untagged rip".into(),
            artist: None,
            album: None,
        };
        crate::core::fire_scrobble(&configured(), &untagged, 1);
        crate::core::fire_playing_now(&configured(), &song("the artist control"));

        server.wait_for("the artist control");
        thread::sleep(Duration::from_millis(100));
        assert!(
            !server.all().iter().any(|l| l.track() == "an untagged rip"),
            "submitted with no artist: {:?}",
            server.all()
        );
    }
}
