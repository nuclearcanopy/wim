//! Shared online-media models and presentation policy for Watch and shellswim.
//! Catalog metadata is deliberately separate from playable sources and
//! subtitle candidates: the existence of an episode says nothing about whether
//! a playable source exists.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MediaKind {
    Movie,
    Series,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MediaItem {
    pub id: String,
    pub kind: MediaKind,
    pub title: String,
    pub year: Option<u16>,
    #[serde(default)]
    pub genres: Vec<String>,
    pub poster: Option<String>,
    pub background: Option<String>,
    pub description: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Episode {
    pub id: String,
    pub season: Option<u32>,
    pub number: Option<u32>,
    pub title: Option<String>,
    pub overview: Option<String>,
    pub thumbnail: Option<String>,
    pub air_date: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Season {
    pub number: Option<u32>,
    pub episodes: Vec<Episode>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct SpoilerPolicy {
    /// Explicit opt-in; safe labels and suppressed episode artwork are the
    /// default in every consumer.
    pub reveal_episode_details: bool,
}

impl Episode {
    /// A neutral, accessible row label unless the user explicitly reveals
    /// episode details. Missing numbering is never fabricated.
    pub fn display_label(&self, policy: SpoilerPolicy) -> String {
        if policy.reveal_episode_details {
            if let Some(title) = self.title.as_deref().filter(|title| !title.is_empty()) {
                return match (self.season, self.number) {
                    (Some(season), Some(number)) => {
                        format!("S{season:02}E{number:02} · {title}")
                    }
                    _ => title.to_owned(),
                };
            }
        }

        match (self.season, self.number) {
            (Some(season), Some(number)) => format!("S{season:02}E{number:02} · Episode {number}"),
            (_, Some(number)) => format!("Episode {number}"),
            _ => "Episode · number unavailable".to_owned(),
        }
    }

    pub fn display_overview<'a>(&'a self, policy: SpoilerPolicy) -> Option<&'a str> {
        policy
            .reveal_episode_details
            .then_some(self.overview.as_deref())
            .flatten()
    }

    pub fn display_thumbnail<'a>(&'a self, policy: SpoilerPolicy) -> Option<&'a str> {
        policy
            .reveal_episode_details
            .then_some(self.thumbnail.as_deref())
            .flatten()
    }
}

/// Group and sort catalog videos without inventing numbering. Unknown seasons
/// and episode numbers sort after known values while retaining stable input
/// order for ties.
pub fn seasons_from_episodes(episodes: impl IntoIterator<Item = Episode>) -> Vec<Season> {
    let mut seasons: Vec<Season> = Vec::new();
    for episode in episodes {
        let key = episode.season;
        let index = seasons.iter().position(|season| season.number == key);
        match index {
            Some(index) => seasons[index].episodes.push(episode),
            None => seasons.push(Season {
                number: key,
                episodes: vec![episode],
            }),
        }
    }
    seasons.sort_by_key(|season| season.number.unwrap_or(u32::MAX));
    for season in &mut seasons {
        season
            .episodes
            .sort_by_key(|episode| episode.number.unwrap_or(u32::MAX));
    }
    seasons
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Source {
    pub id: String,
    pub provider: String,
    pub label: String,
    pub quality: Option<String>,
    pub size_bytes: Option<u64>,
    pub cached: Option<bool>,
    pub transport: SourceTransport,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SourceTransport {
    Http,
    Https,
    TorBox,
    Unsupported(String),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SubtitleCandidate {
    pub id: String,
    pub provider: String,
    pub language: String,
    pub label: Option<String>,
    pub format: Option<String>,
    pub url: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlaybackIdentity {
    pub media_id: String,
    pub episode_id: Option<String>,
    pub source_id: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn episode(season: Option<u32>, number: Option<u32>) -> Episode {
        Episode {
            id: format!("id-{season:?}-{number:?}"),
            season,
            number,
            title: Some("THE HERO DIES — SPOILER".into()),
            overview: Some("A SECRET REVEAL THAT MUST STAY HIDDEN".into()),
            thumbnail: Some("https://image.invalid/spoiler.jpg".into()),
            air_date: None,
        }
    }

    #[test]
    fn spoiler_safe_episode_surfaces_are_neutral() {
        let item = episode(Some(2), Some(3));
        let safe = SpoilerPolicy::default();
        assert_eq!(item.display_label(safe), "S02E03 · Episode 3");
        assert!(!item.display_label(safe).contains("SPOILER"));
        assert_eq!(item.display_overview(safe), None);
        assert_eq!(item.display_thumbnail(safe), None);
    }

    #[test]
    fn explicit_reveal_exposes_episode_details() {
        let item = episode(Some(2), Some(3));
        let reveal = SpoilerPolicy {
            reveal_episode_details: true,
        };
        assert_eq!(
            item.display_label(reveal),
            "S02E03 · THE HERO DIES — SPOILER"
        );
        assert_eq!(item.display_overview(reveal), item.overview.as_deref());
        assert_eq!(item.display_thumbnail(reveal), item.thumbnail.as_deref());
    }

    #[test]
    fn missing_episode_number_is_not_invented_and_grouping_is_ordered() {
        let seasons = seasons_from_episodes([
            episode(Some(2), Some(4)),
            episode(None, None),
            episode(Some(1), Some(2)),
            episode(Some(2), Some(1)),
        ]);
        assert_eq!(
            seasons.iter().map(|s| s.number).collect::<Vec<_>>(),
            [Some(1), Some(2), None]
        );
        assert_eq!(
            seasons[1]
                .episodes
                .iter()
                .map(|e| e.number)
                .collect::<Vec<_>>(),
            [Some(1), Some(4)]
        );
        assert_eq!(
            episode(None, None).display_label(SpoilerPolicy::default()),
            "Episode · number unavailable"
        );
    }
}
