//! Unified fuzzy search: one ranked list across local music, local video,
//! and every Navidrome entity type, rather than per-source silos. This is
//! the single most-emphasized requirement in the spec; searching "bla"
//! should surface matching artists, albums, songs, playlists, and local
//! files together, ranked by actual relevance, not just substring hits.

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;

use nucleo_matcher::pattern::{CaseMatching, Normalization, Pattern};
use nucleo_matcher::{Matcher, Utf32Str};

use crate::library::{LocalTrack, MediaKind};
use crate::subsonic::{
    Album, Artist, Client, Playlist, SearchResult as SubsonicSearchResult, Song,
};

/// Results-list section a `SearchItem` groups under; see
/// `SearchItem::category`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Category {
    Artists,
    Albums,
    Songs,
    Videos,
    Playlists,
}

impl Category {
    pub fn label(self) -> &'static str {
        match self {
            Category::Artists => "ARTISTS",
            Category::Albums => "ALBUMS",
            Category::Songs => "SONGS",
            Category::Videos => "VIDEOS",
            Category::Playlists => "PLAYLISTS",
        }
    }
}

/// `(index, category)` for every point a results list's category
/// changes, always including index 0 if `items` is non-empty; a
/// section header goes immediately before each of these. Shared by
/// `ui::results_list`'s own row layout and main.rs's sidebar-divider
/// junction marks so the two can never disagree about where a header
/// actually lands on screen.
pub fn group_boundaries(items: &[SearchItem]) -> Vec<(usize, Category)> {
    let mut boundaries = Vec::new();
    let mut last = None;
    for (i, item) in items.iter().enumerate() {
        let cat = item.category();
        if last != Some(cat) {
            boundaries.push((i, cat));
            last = Some(cat);
        }
    }
    boundaries
}

#[derive(Debug, Clone)]
pub enum SearchItem {
    LocalMusic {
        path: PathBuf,
        title: String,
        artist: Option<String>,
        album: Option<String>,
    },
    LocalVideo {
        path: PathBuf,
        title: String,
    },
    // every remote variant carries the client it came from; with
    // multiple navidrome servers merged into one corpus and no "active
    // server" concept, this is the only way playback later knows which
    // server's credentials to build a stream url with.
    Artist {
        id: String,
        name: String,
        client: Arc<Client>,
    },
    Album {
        id: String,
        name: String,
        artist: String,
        client: Arc<Client>,
    },
    Song {
        id: String,
        title: String,
        artist: String,
        album: String,
        client: Arc<Client>,
    },
    Playlist {
        id: String,
        name: String,
        client: Arc<Client>,
    },
}

impl SearchItem {
    /// The text the fuzzy matcher scores against; every relevant field
    /// joined, so "search bla" matches on title, artist, or album alike.
    fn haystack(&self) -> String {
        match self {
            SearchItem::LocalMusic {
                title,
                artist,
                album,
                ..
            } => [Some(title.as_str()), artist.as_deref(), album.as_deref()]
                .into_iter()
                .flatten()
                .collect::<Vec<_>>()
                .join(" "),
            SearchItem::LocalVideo { title, .. } => title.clone(),
            SearchItem::Artist { name, .. } => name.clone(),
            SearchItem::Album { name, artist, .. } => format!("{name} {artist}"),
            SearchItem::Song {
                title,
                artist,
                album,
                ..
            } => format!("{title} {artist} {album}"),
            SearchItem::Playlist { name, .. } => name.clone(),
        }
    }

    /// `Some((kind_tag, id))` for remote items, used to dedup against
    /// what's already in the corpus; reconnecting the same server
    /// (manually, or via periodic auto-resync) must not just keep
    /// appending duplicates of every artist/album/playlist every time.
    /// `None` for local items, which aren't added repeatedly the same
    /// way (the whole corpus is rebuilt fresh on a library rescan).
    fn remote_key(&self) -> Option<(u8, &str)> {
        match self {
            SearchItem::Artist { id, .. } => Some((0, id.as_str())),
            SearchItem::Album { id, .. } => Some((1, id.as_str())),
            SearchItem::Song { id, .. } => Some((2, id.as_str())),
            SearchItem::Playlist { id, .. } => Some((3, id.as_str())),
            SearchItem::LocalMusic { .. } | SearchItem::LocalVideo { .. } => None,
        }
    }

    /// The results-list section this item groups under; local music
    /// and remote songs share one "songs" section rather than
    /// splitting local-vs-remote further, since that distinction
    /// already shows up per-row (`results_list::source_code`).
    pub fn category(&self) -> Category {
        match self {
            SearchItem::Artist { .. } => Category::Artists,
            SearchItem::Album { .. } => Category::Albums,
            SearchItem::LocalMusic { .. } | SearchItem::Song { .. } => Category::Songs,
            SearchItem::LocalVideo { .. } => Category::Videos,
            SearchItem::Playlist { .. } => Category::Playlists,
        }
    }

    /// `None` for local items; the originating server's base url for
    /// everything else, for the on-demand index breakdown.
    fn source(&self) -> Option<&str> {
        match self {
            SearchItem::Artist { client, .. }
            | SearchItem::Album { client, .. }
            | SearchItem::Song { client, .. }
            | SearchItem::Playlist { client, .. } => Some(client.base_url()),
            SearchItem::LocalMusic { .. } | SearchItem::LocalVideo { .. } => None,
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
                MediaKind::Video => SearchItem::LocalVideo {
                    path: t.path,
                    title: t.title,
                },
            });
        }
    }

    fn existing_remote_keys(&self) -> HashSet<(u8, String)> {
        self.items
            .iter()
            .filter_map(|i| i.remote_key())
            .map(|(k, id)| (k, id.to_string()))
            .collect()
    }

    pub fn add_subsonic_search_result(&mut self, r: SubsonicSearchResult, client: &Arc<Client>) {
        let mut seen = self.existing_remote_keys();
        for Artist { id, name } in r.artists {
            if seen.insert((0, id.clone())) {
                self.items.push(SearchItem::Artist {
                    id,
                    name,
                    client: client.clone(),
                });
            }
        }
        for Album { id, name, artist } in r.albums {
            if seen.insert((1, id.clone())) {
                self.items.push(SearchItem::Album {
                    id,
                    name,
                    artist,
                    client: client.clone(),
                });
            }
        }
        for Song {
            id,
            title,
            artist,
            album,
            ..
        } in r.songs
        {
            if seen.insert((2, id.clone())) {
                self.items.push(SearchItem::Song {
                    id,
                    title,
                    artist,
                    album,
                    client: client.clone(),
                });
            }
        }
    }

    pub fn add_playlists(
        &mut self,
        playlists: impl IntoIterator<Item = Playlist>,
        client: &Arc<Client>,
    ) {
        let mut seen = self.existing_remote_keys();
        for Playlist { id, name } in playlists {
            if !seen.insert((3, id.clone())) {
                continue;
            }
            self.items.push(SearchItem::Playlist {
                id,
                name,
                client: client.clone(),
            });
        }
    }

    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    /// Local count plus a per-navidrome-server count, in first-seen
    /// order; computed on demand ('I') rather than announced on every
    /// startup/rescan.
    pub fn breakdown(&self) -> Vec<(String, usize)> {
        let mut local = 0usize;
        let mut by_server: Vec<(String, usize)> = Vec::new();
        for item in &self.items {
            match item.source() {
                None => local += 1,
                Some(url) => match by_server.iter_mut().find(|(u, _)| u == url) {
                    Some((_, count)) => *count += 1,
                    None => by_server.push((url.to_string(), 1)),
                },
            }
        }
        let mut result = vec![("local".to_string(), local)];
        result.extend(by_server);
        result
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
                    .map(typo_score)
                    .max();

                match (fuzzy_score, typo_score) {
                    (None, None) => None,
                    (a, b) => Some((a.unwrap_or(0).max(b.unwrap_or(0)), item)),
                }
            })
            .collect();

        scored.sort_by_key(|a| std::cmp::Reverse(a.0));

        // group same-category items together (section headers in the
        // results list), but order the groups themselves by each
        // one's single best match rather than a fixed kind ordering:
        // the first item of a category in this already best-first
        // list *is* that category's best score, so the order
        // categories are first seen in is already "best match leads".
        // searching "bladee" with a 100% artist match still puts
        // Artists first even though a song titled "Rewind feat.
        // Bladee" might outscore the rest of that artist's own songs.
        let mut group_order: Vec<Category> = Vec::new();
        for (_, item) in &scored {
            let cat = item.category();
            if !group_order.contains(&cat) {
                group_order.push(cat);
            }
        }
        group_order
            .into_iter()
            .flat_map(|cat| {
                scored
                    .iter()
                    .filter(move |(_, item)| item.category() == cat)
                    .map(|(_, item)| (*item).clone())
            })
            .collect()
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
    use crate::config::NavidromeConfig;

    /// Never dialed; `Client::new` just builds the struct (ping/fetch
    /// calls are separate), so this is safe to construct in tests with
    /// no server and no network access.
    fn test_client() -> Arc<Client> {
        Arc::new(
            Client::new(&NavidromeConfig {
                url: "https://example.invalid".into(),
                username: "test".into(),
                password: Some("test".into()),
                password_file: None,
                max_bitrate_kbps: None,
                transcode_format: None,
            })
            .unwrap(),
        )
    }

    fn corpus() -> Corpus {
        let client = test_client();
        let mut c = Corpus::new();
        c.add_local_tracks([LocalTrack {
            path: PathBuf::from("/music/blade runner ost.flac"),
            title: "Blade Runner Blues".into(),
            artist: Some("Vangelis".into()),
            album: Some("Blade Runner".into()),
            duration_secs: None,
            kind: MediaKind::Music,
        }]);
        c.add_subsonic_search_result(
            SubsonicSearchResult {
                artists: vec![Artist {
                    id: "a1".into(),
                    name: "Black Sabbath".into(),
                }],
                albums: vec![Album {
                    id: "al1".into(),
                    name: "Blackout".into(),
                    artist: "Scorpions".into(),
                }],
                songs: vec![Song {
                    id: "s1".into(),
                    title: "Unrelated Track".into(),
                    artist: "Someone Else".into(),
                    album: "Other Album".into(),
                    duration: None,
                }],
            },
            &client,
        );
        c.add_playlists(
            [Playlist {
                id: "p1".into(),
                name: "black metal essentials".into(),
            }],
            &client,
        );
        c
    }

    #[test]
    fn reconnecting_the_same_server_does_not_duplicate_entries() {
        let client = test_client();
        let mut c = Corpus::new();
        let result = || SubsonicSearchResult {
            artists: vec![Artist {
                id: "a1".into(),
                name: "Bladee".into(),
            }],
            albums: vec![Album {
                id: "al1".into(),
                name: "Cold Visions".into(),
                artist: "Bladee".into(),
            }],
            songs: vec![],
        };
        c.add_subsonic_search_result(result(), &client);
        c.add_subsonic_search_result(result(), &client); // simulates a reconnect/periodic resync
        c.add_playlists(
            [Playlist {
                id: "p1".into(),
                name: "Drain Gang".into(),
            }],
            &client,
        );
        c.add_playlists(
            [Playlist {
                id: "p1".into(),
                name: "Drain Gang".into(),
            }],
            &client,
        );

        let total: usize = c.breakdown().iter().map(|(_, n)| n).sum();
        assert_eq!(total, 3, "one artist + one album + one playlist, not six");
    }

    #[test]
    fn search_merges_every_source_for_a_matching_keyword() {
        let c = corpus();
        let results = c.search("bla");

        // "bla" should hit the local track (via title/artist/album text),
        // the artist, the album, and the playlist; but not the unrelated
        // song, proving this is one merged ranked list, not per-source
        // silos that happen to sit next to each other.
        assert!(results
            .iter()
            .any(|i| matches!(i, SearchItem::LocalMusic { .. })));
        assert!(results
            .iter()
            .any(|i| matches!(i, SearchItem::Artist { .. })));
        assert!(results
            .iter()
            .any(|i| matches!(i, SearchItem::Album { .. })));
        assert!(results
            .iter()
            .any(|i| matches!(i, SearchItem::Playlist { .. })));
        assert!(!results
            .iter()
            .any(|i| matches!(i, SearchItem::Song { id, .. } if id == "s1")));
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
        c.add_subsonic_search_result(
            SubsonicSearchResult {
                artists: vec![Artist {
                    id: "a1".into(),
                    name: "Branimir".into(),
                }],
                albums: vec![],
                songs: vec![],
            },
            &test_client(),
        );

        // "brnimir": valid letter subsequence of "branimir" (drop the
        // 'a'), so nucleo's own fuzzy matcher already catches this one.
        let dropped_letter = c.search("brnimir");
        assert!(
            dropped_letter
                .iter()
                .any(|i| matches!(i, SearchItem::Artist { name, .. } if name == "Branimir")),
            "subsequence-fuzzy typo should match"
        );

        // "brainmir": NOT a valid subsequence of "branimir" (the 'n' here
        // comes after 'i', but in "branimir" the only 'n' is before every
        // 'i'), so this only matches via the edit-distance fallback.
        let transposed = c.search("brainmir");
        assert!(
            transposed
                .iter()
                .any(|i| matches!(i, SearchItem::Artist { name, .. } if name == "Branimir")),
            "transposition typo should still match via edit-distance fallback"
        );
    }
}
