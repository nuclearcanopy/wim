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
            .post(&format!("{BASE_URL}/1/submit-listens"))
            .set("Authorization", &format!("Token {}", self.token))
            .send_json(body)?;
        Ok(())
    }
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
