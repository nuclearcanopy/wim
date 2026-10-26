//! Unified fuzzy search: one ranked list across local music, local video,
//! and every Navidrome entity type, rather than per-source silos. This is
//! the single most-emphasized requirement in the spec — searching "bla"
//! should surface matching artists, albums, songs, playlists, and local
//! files together, ranked by actual relevance, not just substring hits.

use std::path::PathBuf;

use nucleo_matcher::pattern::{CaseMatching, Normalization, Pattern};
use nucleo_matcher::{Matcher, Utf32Str};

use crate::library::{LocalTrack, MediaKind};
use crate::subsonic::{Album, Artist, Playlist, SearchResult as SubsonicSearchResult, Song};

#[derive(Debug, Clone)]
pub enum SearchItem {
    LocalMusic { path: PathBuf, title: String, artist: Option<String>, album: Option<String> },
    LocalVideo { path: PathBuf, title: String },
    Artist { id: String, name: String },
    Album { id: String, name: String, artist: String },
    Song { id: String, title: String, artist: String, album: String },
    Playlist { id: String, name: String },
}

impl SearchItem {
    /// The text the fuzzy matcher scores against — every relevant field
    /// joined, so "search bla" matches on title, artist, or album alike.
    fn haystack(&self) -> String {
        match self {
            SearchItem::LocalMusic { title, artist, album, .. } => {
                [Some(title.as_str()), artist.as_deref(), album.as_deref()]
                    .into_iter()
                    .flatten()
                    .collect::<Vec<_>>()
                    .join(" ")
            }
            SearchItem::LocalVideo { title, .. } => title.clone(),
            SearchItem::Artist { name, .. } => name.clone(),
            SearchItem::Album { name, artist, .. } => format!("{name} {artist}"),
            SearchItem::Song { title, artist, album, .. } => format!("{title} {artist} {album}"),
            SearchItem::Playlist { name, .. } => name.clone(),
        }
    }

    pub fn kind_label(&self) -> &'static str {
        match self {
            SearchItem::LocalMusic { .. } => "local",
            SearchItem::LocalVideo { .. } => "video",
            SearchItem::Artist { .. } => "artist",
            SearchItem::Album { .. } => "album",
            SearchItem::Song { .. } => "song",
            SearchItem::Playlist { .. } => "playlist",
        }
    }
}

/// The merged corpus to search over, built once per library scan /
/// Navidrome sync and re-searched on every keystroke.
#[derive(Debug, Clone, Default)]
pub struct Corpus {
    items: Vec<SearchItem>,
}

impl Corpus {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn add_local_tracks(&mut self, tracks: impl IntoIterator<Item = LocalTrack>) {
        for t in tracks {
            self.items.push(match t.kind {
                MediaKind::Music => SearchItem::LocalMusic {
                    path: t.path,
                    title: t.title,
                    artist: t.artist,
                    album: t.album,
                },
                MediaKind::Video => SearchItem::LocalVideo { path: t.path, title: t.title },
            });
        }
    }

    pub fn add_subsonic_search_result(&mut self, r: SubsonicSearchResult) {
        for Artist { id, name } in r.artists {
            self.items.push(SearchItem::Artist { id, name });
        }
        for Album { id, name, artist } in r.albums {
            self.items.push(SearchItem::Album { id, name, artist });
        }
        for Song { id, title, artist, album, .. } in r.songs {
            self.items.push(SearchItem::Song { id, title, artist, album });
        }
    }

    pub fn add_playlists(&mut self, playlists: impl IntoIterator<Item = Playlist>) {
        for Playlist { id, name } in playlists {
            self.items.push(SearchItem::Playlist { id, name });
        }
    }

    pub fn len(&self) -> usize {
        self.items.len()
    }

    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    /// Ranked matches for `query` across the whole corpus, best first.
    /// Returns owned items (the corpus is small; cloning keeps the caller
    /// free of borrowing it across an event loop).
    ///
    /// Two signals are combined: nucleo's subsequence fuzzy score (fast,
    /// handles substrings/skips/case well), and a per-word edit-distance
    /// fallback (catches genuine typos like transpositions, e.g. "lesna"
    /// matching "Selena", where the letters aren't even in subsequence
    /// order so nucleo alone would miss it). An item counts as a match if
    /// *either* signal fires, so a typo never silently returns nothing.
    pub fn search(&self, query: &str) -> Vec<SearchItem> {
        if query.trim().is_empty() {
            return Vec::new();
        }
        let mut matcher = Matcher::default();
        let pattern = Pattern::parse(query, CaseMatching::Smart, Normalization::Smart);
        let query_lower = query.to_lowercase();
        let typo_budget = edit_distance_budget(query_lower.chars().count());

        let mut scored: Vec<(u32, &SearchItem)> = self
            .items
            .iter()
            .filter_map(|item| {
                let haystack = item.haystack();

                let mut buf = Vec::new();
                let utf32 = Utf32Str::new(&haystack, &mut buf);
                let fuzzy_score = pattern.score(utf32, &mut matcher);

                let typo_score = haystack
                    .split_whitespace()
                    .map(|word| strsim::damerau_levenshtein(&query_lower, &word.to_lowercase()))
                    .filter(|&dist| dist <= typo_budget)
                    .map(|dist| typo_score(dist))
                    .max();

                match (fuzzy_score, typo_score) {
                    (None, None) => None,
                    (a, b) => Some((a.unwrap_or(0).max(b.unwrap_or(0)), item)),
                }
            })
            .collect();

        scored.sort_by(|a, b| b.0.cmp(&a.0));
        scored.into_iter().map(|(_, item)| item.clone()).collect()
    }
}

/// How many edits (insert/delete/substitute/adjacent-transpose) we'll
/// tolerate for a query of this length, scaled generously since missing a
/// result entirely is worse than a slightly loose match: roughly half the
/// query length, rounded up, minimum 1.
fn edit_distance_budget(query_len: usize) -> usize {
    query_len.div_ceil(2).max(1)
}

/// Converts an edit distance into a score comparable to nucleo's fuzzy
/// scores. Deliberately scored below what a real subsequence match gets
/// for reasonable-length input, so clean matches still rank above
/// typo-recovered ones when both exist; closer typos still outrank
/// farther ones among themselves.
fn typo_score(distance: usize) -> u32 {
    100u32.saturating_sub(distance as u32 * 15)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn corpus() -> Corpus {
        let mut c = Corpus::new();
        c.add_local_tracks([LocalTrack {
            path: PathBuf::from("/music/blade runner ost.flac"),
            title: "Blade Runner Blues".into(),
            artist: Some("Vangelis".into()),
            album: Some("Blade Runner".into()),
            duration_secs: None,
            kind: MediaKind::Music,
        }]);
        c.add_subsonic_search_result(SubsonicSearchResult {
            artists: vec![Artist { id: "a1".into(), name: "Black Sabbath".into() }],
            albums: vec![Album { id: "al1".into(), name: "Blackout".into(), artist: "Scorpions".into() }],
            songs: vec![Song {
                id: "s1".into(),
                title: "Unrelated Track".into(),
                artist: "Someone Else".into(),
                album: "Other Album".into(),
                duration: None,
            }],
        });
        c.add_playlists([Playlist { id: "p1".into(), name: "black metal essentials".into() }]);
        c
    }

    #[test]
    fn search_merges_every_source_for_a_matching_keyword() {
        let c = corpus();
        let results = c.search("bla");
        let kinds: Vec<&str> = results.iter().map(|i| i.kind_label()).collect();

        // "bla" should hit the local track (via title/artist/album text),
        // the artist, the album, and the playlist — but not the unrelated
        // song, proving this is one merged ranked list, not per-source
        // silos that happen to sit next to each other.
        assert!(kinds.contains(&"local"));
        assert!(kinds.contains(&"artist"));
        assert!(kinds.contains(&"album"));
        assert!(kinds.contains(&"playlist"));
        assert!(!results.iter().any(|i| matches!(i, SearchItem::Song { id, .. } if id == "s1")));
    }

    #[test]
    fn empty_query_returns_nothing() {
        let c = corpus();
        assert!(c.search("").is_empty());
        assert!(c.search("   ").is_empty());
    }

    #[test]
    fn typos_still_find_the_artist() {
        let mut c = Corpus::new();
        c.add_subsonic_search_result(SubsonicSearchResult {
            artists: vec![Artist { id: "a1".into(), name: "Branimir".into() }],
            albums: vec![],
            songs: vec![],
        });

        // "brnimir": valid letter subsequence of "branimir" (drop the
        // 'a'), so nucleo's own fuzzy matcher already catches this one.
        let dropped_letter = c.search("brnimir");
        assert!(
            dropped_letter.iter().any(|i| matches!(i, SearchItem::Artist { name, .. } if name == "Branimir")),
            "subsequence-fuzzy typo should match"
        );

        // "brainmir": NOT a valid subsequence of "branimir" (the 'n' here
        // comes after 'i', but in "branimir" the only 'n' is before every
        // 'i'), so this only matches via the edit-distance fallback.
        let transposed = c.search("brainmir");
        assert!(
            transposed.iter().any(|i| matches!(i, SearchItem::Artist { name, .. } if name == "Branimir")),
            "transposition typo should still match via edit-distance fallback"
        );
    }
}
