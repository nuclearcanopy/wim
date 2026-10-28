//! Unified fuzzy search: one ranked list across local music, local video,
//! and every Navidrome entity type, rather than per-source silos. This is
//! the single most-emphasized requirement in the spec; searching "bla"
//! should surface matching artists, albums, songs, playlists, and local
//! files together, ranked by actual relevance, not just substring hits.

use std::borrow::Cow;
use std::cell::RefCell;
use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;
use std::thread;

use nucleo_matcher::pattern::{CaseMatching, Normalization, Pattern};
use nucleo_matcher::{Matcher, Utf32Str};

use crate::cache::{self, ServerCatalog};
use crate::config::NavidromeConfig;
use crate::library::{LocalTrack, MediaKind};
use crate::subsonic::{
    Album, Artist, Client, Playlist, SearchResult as SubsonicSearchResult, Song,
};
use crate::trace;

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

    /// Stable "is this the same row as before" identity: remote items
    /// key on their server-side id, local ones on their path, with a
    /// kind tag so ids of different entity types can't collide. Used
    /// to restore a selection after the results list is re-filtered
    /// underneath the user (see `App::resync_results`); `SearchItem`
    /// can't just be `PartialEq`, it carries an `Arc<Client>`.
    pub fn identity(&self) -> (u8, Cow<'_, str>) {
        match self {
            SearchItem::Artist { id, .. } => (0, Cow::Borrowed(id)),
            SearchItem::Album { id, .. } => (1, Cow::Borrowed(id)),
            SearchItem::Song { id, .. } => (2, Cow::Borrowed(id)),
            SearchItem::Playlist { id, .. } => (3, Cow::Borrowed(id)),
            SearchItem::LocalMusic { path, .. } => (4, path.to_string_lossy()),
            SearchItem::LocalVideo { path, .. } => (5, path.to_string_lossy()),
        }
    }

    /// The item's own name with no metadata decoration: a song's
    /// title, an album's or artist's name. `results_list::label` is
    /// the decorated form, for display; this is the bare one, for
    /// anything that has to put the name back as text (shellsong's
    /// completion candidates, which are inserted into a command line).
    pub fn primary_name(&self) -> &str {
        match self {
            SearchItem::LocalMusic { title, .. }
            | SearchItem::LocalVideo { title, .. }
            | SearchItem::Song { title, .. } => title,
            SearchItem::Artist { name, .. }
            | SearchItem::Album { name, .. }
            | SearchItem::Playlist { name, .. } => name,
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

    /// `None` for local items; the originating server's client for
    /// everything else, for `Corpus::remote_clients`.
    fn client(&self) -> Option<&Arc<Client>> {
        match self {
            SearchItem::Artist { client, .. }
            | SearchItem::Album { client, .. }
            | SearchItem::Song { client, .. }
            | SearchItem::Playlist { client, .. } => Some(client),
            SearchItem::LocalMusic { .. } | SearchItem::LocalVideo { .. } => None,
        }
    }
}

/// A corpus item plus the two haystacks `search` would otherwise
/// rebuild for every item on every keystroke: the matcher text in its
/// original case (nucleo's smart-case matching reads it) and a
/// lowercased copy the edit-distance fallback splits into words.
#[derive(Debug, Clone)]
struct Entry {
    item: SearchItem,
    haystack: String,
    haystack_lower: String,
}

impl Entry {
    fn new(item: SearchItem) -> Self {
        let haystack = item.haystack();
        let haystack_lower = haystack.to_lowercase();
        Self {
            item,
            haystack,
            haystack_lower,
        }
    }
}

/// Scoring scratch, reused instead of reallocated per item: nucleo's
/// matcher eagerly allocates ~135kb of dp matrices and is explicitly
/// meant to be reused across calls, and `buf` is `Utf32Str`'s
/// codepoint conversion buffer for non-ascii haystacks.
#[derive(Debug, Clone, Default)]
struct Scratch {
    matcher: Matcher,
    buf: Vec<char>,
}

/// The merged corpus to search over, built once per library scan /
/// Navidrome sync and re-searched on every keystroke.
#[derive(Debug, Clone, Default)]
pub struct Corpus {
    items: Vec<Entry>,
    /// Remote ids already indexed, split per `SearchItem::remote_key`
    /// kind tag so a lookup can borrow the id instead of allocating a
    /// key. Kept for the corpus's whole life rather than recomputed
    /// per incoming batch, which cloned every indexed id every time.
    remote_keys: [HashSet<String>; 4],
    /// `search` takes `&self` (it is called on shared corpus bindings,
    /// and only ever from the input thread), so the scratch lives
    /// behind a `RefCell` instead of forcing `&mut` on every caller;
    /// `search` is not reentrant, so the borrow can't conflict.
    scratch: RefCell<Scratch>,
}

impl Corpus {
    pub fn new() -> Self {
        Self::default()
    }

    /// A corpus already holding every configured server's catalog as
    /// of its last session, read off the disk (`cache::load_catalog`)
    /// with no network at all, so the first frame paints a full
    /// library instead of "connecting to navidrome...". The background
    /// sync then reconciles each server against what it really has
    /// (see `reconcile_server`), which is what keeps this from being a
    /// library frozen at whatever was true last launch.
    ///
    /// Also drops catalogs for servers no longer configured; this runs
    /// exactly once per launch and is the one place holding the live
    /// server list.
    pub fn warm_from_cache(servers: &[NavidromeConfig]) -> Corpus {
        let mut corpus = Corpus::new();
        for nav in servers {
            let Some(catalog) = cache::load_catalog(nav) else {
                continue;
            };
            if catalog.is_empty() {
                continue;
            }
            // offline: `Client::new` only resolves the password and
            // builds an http agent, it never talks to the server.
            let Ok(client) = Client::new(nav) else {
                continue;
            };
            corpus.add_catalog(catalog, &Arc::new(client));
        }
        cache::sweep_foreign_catalogs(servers);
        trace::mark(&format!(
            "navidrome: {} cached catalog rows loaded",
            corpus.items.len()
        ));
        corpus
    }

    /// Folds one server's full catalog in, dropping whatever it no
    /// longer lists: the reconciliation half of `warm_from_cache`.
    /// Without it, an artist or album deleted on the server would be
    /// listed by every later launch for as long as the corpus lived,
    /// since `add_subsonic_search_result` only ever adds.
    ///
    /// Songs are deliberately untouched. Nothing indexes them up front
    /// (they arrive from `search3`, opened albums and artist pages), so
    /// a catalog fetch is not an authoritative song list to prune
    /// against; see docs/startup-cache.md.
    pub fn reconcile_server(&mut self, catalog: ServerCatalog, client: &Arc<Client>) {
        let url = client.base_url();
        let live: [HashSet<&str>; 3] = [
            catalog.artists.iter().map(|a| a.id.as_str()).collect(),
            catalog.albums.iter().map(|a| a.id.as_str()).collect(),
            catalog.playlists.iter().map(|p| p.id.as_str()).collect(),
        ];
        let gone = |item: &SearchItem| {
            if item.source() != Some(url) {
                return false;
            }
            match item.remote_key() {
                Some((0, id)) => !live[0].contains(id),
                Some((1, id)) => !live[1].contains(id),
                Some((3, id)) => !live[2].contains(id),
                _ => false,
            }
        };
        let dropped: Vec<(u8, String)> = self
            .items
            .iter()
            .filter(|e| gone(&e.item))
            .filter_map(|e| e.item.remote_key().map(|(k, id)| (k, id.to_string())))
            .collect();
        if !dropped.is_empty() {
            for (kind, id) in &dropped {
                self.remote_keys[*kind as usize].remove(id);
            }
            self.items.retain(|e| !gone(&e.item));
        }
        self.add_catalog(catalog, client);
    }

    fn add_catalog(&mut self, catalog: ServerCatalog, client: &Arc<Client>) {
        self.add_subsonic_search_result(
            SubsonicSearchResult {
                artists: catalog.artists,
                albums: catalog.albums,
                songs: Vec::new(),
            },
            client,
        );
        self.add_playlists(catalog.playlists, client);
    }

    /// Single insertion point: every path that adds an item has to go
    /// through here so the cached haystacks can't be left unbuilt.
    fn push(&mut self, item: SearchItem) {
        if let Some((kind, id)) = item.remote_key() {
            self.remote_keys[kind as usize].insert(id.to_string());
        }
        self.items.push(Entry::new(item));
    }

    pub fn add_local_tracks(&mut self, tracks: impl IntoIterator<Item = LocalTrack>) {
        for t in tracks {
            self.push(match t.kind {
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

    pub fn add_subsonic_search_result(&mut self, r: SubsonicSearchResult, client: &Arc<Client>) {
        for Artist { id, name } in r.artists {
            if !self.remote_keys[0].contains(id.as_str()) {
                self.push(SearchItem::Artist {
                    id,
                    name,
                    client: client.clone(),
                });
            }
        }
        for Album { id, name, artist } in r.albums {
            if !self.remote_keys[1].contains(id.as_str()) {
                self.push(SearchItem::Album {
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
            if !self.remote_keys[2].contains(id.as_str()) {
                self.push(SearchItem::Song {
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
        for Playlist { id, name } in playlists {
            if self.remote_keys[3].contains(id.as_str()) {
                continue;
            }
            self.push(SearchItem::Playlist {
                id,
                name,
                client: client.clone(),
            });
        }
    }

    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    /// Drops every local file entry, keeping everything indexed from a
    /// server. A rescan re-walks the library from scratch (local items
    /// have no `remote_key`, so `add_local_tracks` would otherwise
    /// append a second copy of every file), and dropping only the
    /// local half means a rescan does not also wipe the navidrome
    /// catalog until the next 45s sync lands.
    pub fn drop_local(&mut self) {
        self.items.retain(|e| e.item.source().is_some());
    }

    /// Every indexed item, in insertion order. Read-only whole-corpus
    /// access for things that aren't a keyword search at all; the
    /// suggestion engine builds its catalog off this (see
    /// `suggest::Catalog::ingest_corpus`).
    pub fn items(&self) -> impl Iterator<Item = &SearchItem> {
        self.items.iter().map(|e| &e.item)
    }

    /// Every distinct navidrome server already represented in the
    /// corpus (deduped by base url), for firing a live `search3`
    /// request against; a server only shows up here once something
    /// from it has actually been indexed (artists/albums/playlists via
    /// `fetch_navidrome_catalog`), which is also exactly when it's
    /// safe to assume the connection is good.
    pub fn remote_clients(&self) -> Vec<Arc<Client>> {
        let mut seen = HashSet::new();
        let mut clients = Vec::new();
        for item in self.items.iter().map(|e| &e.item) {
            if let Some(client) = item.client() {
                if seen.insert(client.base_url().to_string()) {
                    clients.push(client.clone());
                }
            }
        }
        clients
    }

    /// Local count plus a per-navidrome-server count, in first-seen
    /// order; computed on demand ('I') rather than announced on every
    /// startup/rescan.
    pub fn breakdown(&self) -> Vec<(String, usize)> {
        let mut local = 0usize;
        let mut by_server: Vec<(String, usize)> = Vec::new();
        for item in self.items.iter().map(|e| &e.item) {
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
        let pattern = Pattern::parse(query, CaseMatching::Smart, Normalization::Smart);
        let query_lower = query.to_lowercase();
        let query_len = query_lower.chars().count();
        let typo_budget = edit_distance_budget(query_len);

        let mut scored: Vec<(u32, &SearchItem)> = if self.items.len() >= PARALLEL_SCORING_FLOOR {
            // chunks are joined back in corpus order, so the stable
            // sort below still breaks ties exactly as it does
            // single-threaded; each thread needs its own scratch.
            let threads = thread::available_parallelism().map_or(1, |n| n.get());
            let chunk = self.items.len().div_ceil(threads);
            let parts: Vec<Vec<(u32, &SearchItem)>> = thread::scope(|scope| {
                self.items
                    .chunks(chunk)
                    .map(|part| {
                        scope.spawn(|| {
                            let mut scratch = Scratch::default();
                            score_chunk(
                                part,
                                &pattern,
                                &query_lower,
                                query_len,
                                typo_budget,
                                &mut scratch,
                            )
                        })
                    })
                    .collect::<Vec<_>>()
                    .into_iter()
                    .map(|handle| handle.join().unwrap_or_default())
                    .collect()
            });
            parts.concat()
        } else {
            let mut scratch = self.scratch.borrow_mut();
            score_chunk(
                &self.items,
                &pattern,
                &query_lower,
                query_len,
                typo_budget,
                &mut scratch,
            )
        };

        scored.sort_by_key(|a| std::cmp::Reverse(a.0));

        // relative score cutoff: nucleo's own subsequence matcher will
        // happily return *some* nonzero score for almost any haystack
        // against a short query (e.g. "crest" is a valid, if scattered,
        // subsequence of "Cigarettes After Sex"), and with no floor at
        // all every one of those sparse, low-quality matches rendered
        // right alongside a genuine exact title hit. Anything scoring
        // under `WEAK_MATCH_CUTOFF_FRACTION` of the *best* match in
        // this search is almost certainly that kind of noise rather
        // than something the user meant, so drop it. This is
        // self-limiting rather than needing a separate "don't leave it
        // empty" guard: the top result's score always clears its own
        // threshold, so the list can never be pruned to nothing, and
        // when every candidate is in the same (even if low) ballpark
        // (no single standout match), they're all close enough to the
        // top score to clear the cutoff together, so weak guesses
        // still show exactly when there's nothing better on offer.
        if let Some(&(top_score, _)) = scored.first() {
            let threshold = (top_score as f64 * WEAK_MATCH_CUTOFF_FRACTION).round() as u32;
            scored.retain(|(s, _)| *s >= threshold);
        }

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

/// Scores one slice of the corpus against `pattern`, reusing a single
/// `Scratch` for every item; the whole per-item hot path lives here so
/// the threaded and single-threaded paths can't score differently.
/// Keeps corpus order, which the caller's stable sort relies on.
fn score_chunk<'a>(
    entries: &'a [Entry],
    pattern: &Pattern,
    query_lower: &str,
    query_len: usize,
    typo_budget: usize,
    scratch: &mut Scratch,
) -> Vec<(u32, &'a SearchItem)> {
    let Scratch { matcher, buf } = scratch;
    let mut scored = Vec::new();
    for entry in entries {
        let utf32 = Utf32Str::new(&entry.haystack, buf);
        let fuzzy_score = pattern.score(utf32, matcher);

        // two free skips of the edit-distance pass, both no-ops on the
        // final score: a typo can never beat `typo_score(0)`, and edit
        // distance is at least the length difference, so a word that
        // far off can't come in under budget.
        let typo_score = if fuzzy_score.is_some_and(|s| s >= typo_score(0)) {
            None
        } else {
            entry
                .haystack_lower
                .split_whitespace()
                .filter(|word| char_count(word).abs_diff(query_len) <= typo_budget)
                .map(|word| strsim::damerau_levenshtein(query_lower, word))
                .filter(|&dist| dist <= typo_budget)
                .map(typo_score)
                .max()
        };

        match (fuzzy_score, typo_score) {
            (None, None) => {}
            (a, b) => scored.push((a.unwrap_or(0).max(b.unwrap_or(0)), &entry.item)),
        }
    }
    scored
}

/// `chars().count()` with the ascii fast path spelled out; the typo
/// prefilter calls this once per word per item, and metadata is mostly
/// ascii, where the length is already known.
fn char_count(s: &str) -> usize {
    if s.is_ascii() {
        s.len()
    } else {
        s.chars().count()
    }
}

/// Corpus size at which `search` fans per-item scoring out across
/// threads. Below it the fixed cost of the fan-out (spawn/join plus a
/// fresh ~135kb matcher per thread, measured at ~0.8ms on 8 cores)
/// dominates: at 100 items it made every query ~8x slower, at 500 it
/// broke even, and from ~2k up it's a consistent ~2x win.
const PARALLEL_SCORING_FLOOR: usize = 2_000;

/// Below this fraction of the best match's score in a given search, a
/// result is treated as noise rather than a real candidate; see the
/// cutoff's own call site in `Corpus::search` for the "crest" bug this
/// exists to fix. Picked empirically against that exact bug report:
/// nucleo's word-start bonus means a purely coincidental scattered
/// match (e.g. "crest" landing on word-initial letters scattered
/// across "...And Justice for All"/"The College Dropout") still scores
/// 60%+ of a genuine exact-title hit, so the cutoff has to sit above
/// that to actually remove it; 0.65 does, while an equally-weighted
/// second match (e.g. two near-identical candidates, or two albums by
/// the same artist that are each a comparably good hit) still clears
/// it since neither is *that* far below the other.
const WEAK_MATCH_CUTOFF_FRACTION: f64 = 0.65;

/// How many edits (insert/delete/substitute/adjacent-transpose) we'll
/// tolerate for a query of this length. Deliberately tight and scaling
/// slowly: a loose budget (the old `query_len / 2`) meant a 5-letter
/// query like "crest" tolerated 3 edits, which is enough to "correct"
/// into almost any other short word, defeating the point of a typo
/// fallback. One edit covers genuine single-letter typos/transpositions
/// regardless of query length; only noticeably longer queries get a
/// second.
fn edit_distance_budget(query_len: usize) -> usize {
    1 + query_len.saturating_sub(1) / 6
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
                    genres: Vec::new(),
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

    fn other_client() -> Arc<Client> {
        Arc::new(
            Client::new(&NavidromeConfig {
                url: "https://other.invalid".into(),
                username: "test".into(),
                password: Some("test".into()),
                password_file: None,
                max_bitrate_kbps: None,
                transcode_format: None,
            })
            .unwrap(),
        )
    }

    fn catalog(artists: &[&str], albums: &[&str], playlists: &[&str]) -> ServerCatalog {
        ServerCatalog {
            artists: artists
                .iter()
                .map(|id| Artist {
                    id: (*id).into(),
                    name: format!("artist {id}"),
                })
                .collect(),
            albums: albums
                .iter()
                .map(|id| Album {
                    id: (*id).into(),
                    name: format!("album {id}"),
                    artist: "artist".into(),
                })
                .collect(),
            playlists: playlists
                .iter()
                .map(|id| Playlist {
                    id: (*id).into(),
                    name: format!("playlist {id}"),
                })
                .collect(),
        }
    }

    fn ids(c: &Corpus) -> Vec<String> {
        c.items()
            .map(|i| i.identity().1.into_owned())
            .collect::<Vec<_>>()
    }

    /// The whole reason a persisted catalog is safe to load: what the
    /// server no longer has stops being listed, rather than lingering
    /// for the rest of the session (and every session after it).
    #[test]
    fn reconciling_drops_what_the_server_no_longer_lists() {
        let client = test_client();
        let mut c = Corpus::new();
        c.reconcile_server(catalog(&["a1", "a2"], &["al1"], &["p1"]), &client);
        assert_eq!(c.items().count(), 4);

        c.reconcile_server(catalog(&["a2"], &["al1", "al2"], &[]), &client);
        let left = ids(&c);
        assert!(!left.contains(&"a1".to_string()), "deleted artist stayed");
        assert!(!left.contains(&"p1".to_string()), "deleted playlist stayed");
        assert!(left.contains(&"a2".to_string()));
        assert!(left.contains(&"al1".to_string()));
        assert!(left.contains(&"al2".to_string()), "new album never landed");
        assert_eq!(left.len(), 3);

        // and the dedup bookkeeping went with them: a server that
        // brings a dropped id back must be able to re-add it.
        c.reconcile_server(catalog(&["a1", "a2"], &["al1", "al2"], &["p1"]), &client);
        assert_eq!(c.items().count(), 5);
    }

    /// Nothing indexes songs up front, so a catalog fetch is not an
    /// authoritative song list; dropping songs here would throw away
    /// every tracklist an opened album had taught the corpus.
    #[test]
    fn reconciling_leaves_songs_and_local_tracks_alone() {
        let mut c = corpus();
        let client = test_client();
        c.reconcile_server(catalog(&["a1"], &["al1"], &["p1"]), &client);
        let left = ids(&c);
        assert!(left.contains(&"s1".to_string()), "a known song was dropped");
        assert!(
            left.iter().any(|id| id.contains("blade runner ost.flac")),
            "a local file was dropped"
        );
    }

    #[test]
    fn reconciling_one_server_does_not_touch_another() {
        let mut c = Corpus::new();
        let first = test_client();
        let second = other_client();
        c.reconcile_server(catalog(&["a1"], &["al1"], &[]), &first);
        c.reconcile_server(catalog(&["b1"], &["bl1"], &[]), &second);
        assert_eq!(c.items().count(), 4);

        // the first server emptied out; the second one's entries are
        // not its business.
        c.reconcile_server(catalog(&[], &[], &[]), &first);
        let left = ids(&c);
        assert_eq!(left.len(), 2);
        assert!(left.contains(&"b1".to_string()));
        assert!(left.contains(&"bl1".to_string()));
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

    /// Regression test for the reported bug: searching "crest" (an
    /// exact album title) returned "Crest" ranked first, immediately
    /// followed by dozens of unrelated albums that merely contain
    /// "crest"'s letters as a scattered subsequence somewhere across
    /// their (often multi-word) title. With a real top match on the
    /// board, that noise should be cut, not rendered alongside it.
    #[test]
    fn one_great_match_suppresses_unrelated_noise() {
        let client = test_client();
        let mut c = Corpus::new();
        c.add_subsonic_search_result(
            SubsonicSearchResult {
                artists: vec![],
                albums: vec![
                    Album {
                        id: "al1".into(),
                        name: "Crest".into(),
                        artist: "Bladee".into(),
                    },
                    Album {
                        id: "al2".into(),
                        name: "I.".into(),
                        artist: "Various Artists".into(),
                    },
                    Album {
                        id: "al3".into(),
                        name: "Affection".into(),
                        artist: "Someone".into(),
                    },
                    Album {
                        id: "al4".into(),
                        name: "Anna Karenina".into(),
                        artist: "Someone Else".into(),
                    },
                    Album {
                        id: "al5".into(),
                        name: "Bubblegum".into(),
                        artist: "Another Artist".into(),
                    },
                    Album {
                        id: "al6".into(),
                        name: "Cigarettes After Sex".into(),
                        artist: "Cigarettes After Sex".into(),
                    },
                    Album {
                        id: "al7".into(),
                        name: "X's".into(),
                        artist: "Someone".into(),
                    },
                    Album {
                        id: "al8".into(),
                        name: "...And Justice for All".into(),
                        artist: "Metallica".into(),
                    },
                    Album {
                        id: "al9".into(),
                        name: "The College Dropout".into(),
                        artist: "Kanye West".into(),
                    },
                ],
                songs: vec![],
            },
            &client,
        );

        let results = c.search("crest");
        let names: Vec<&str> = results
            .iter()
            .map(|i| match i {
                SearchItem::Album { name, .. } => name.as_str(),
                _ => unreachable!(),
            })
            .collect();

        assert_eq!(
            names,
            vec!["Crest"],
            "the exact title should suppress every unrelated scattered-subsequence/typo match: got {names:?}"
        );
    }

    /// Deterministic (no rng) synthetic corpus for `bench_search`,
    /// with realistic multi-word artist/album/title haystacks so the
    /// whitespace-split typo pass sees a realistic word count per item.
    fn synthetic_corpus(n: usize) -> Corpus {
        const ARTISTS: [&str; 12] = [
            "Bladee",
            "Vangelis",
            "Black Sabbath",
            "Scorpions",
            "Cigarettes After Sex",
            "Metallica",
            "Kanye West",
            "Branimir Stulic",
            "Selena Gomez",
            "Aphex Twin",
            "Boards of Canada",
            "Godspeed You! Black Emperor",
        ];
        const ALBUMS: [&str; 12] = [
            "Cold Visions",
            "Blade Runner",
            "Paranoid",
            "Blackout",
            "Crest",
            "...And Justice for All",
            "The College Dropout",
            "Selected Ambient Works",
            "Music Has the Right to Children",
            "Lift Your Skinny Fists",
            "Eversince",
            "Icedancer",
        ];
        const TITLES: [&str; 12] = [
            "Blade Runner Blues",
            "Be Nice 2 Me",
            "Who Goes There",
            "Rewind",
            "Tears of Joy",
            "Obedient",
            "Hotel Breakfast",
            "Into Dust",
            "Sunset Mall",
            "Western Union",
            "Decay",
            "Drownin",
        ];
        let mut c = Corpus::new();
        c.add_local_tracks((0..n).map(|i| LocalTrack {
            path: PathBuf::from(format!("/music/{i:06}.flac")),
            title: format!("{} {i}", TITLES[i % TITLES.len()]),
            artist: Some(ARTISTS[(i / 7) % ARTISTS.len()].to_string()),
            album: Some(ALBUMS[(i / 13) % ALBUMS.len()].to_string()),
            duration_secs: None,
            kind: MediaKind::Music,
        }));
        c
    }

    /// Not an assertion: prints per-query wall time, since absolute
    /// numbers are machine-dependent. Run with
    /// `cargo test --release -- --ignored --nocapture bench_search`.
    /// "b" is the worst case for match count, "brainmir" for the
    /// edit-distance fallback.
    #[test]
    #[ignore = "timing benchmark, run manually"]
    fn bench_search() {
        for size in [10_000usize, 100_000] {
            let c = synthetic_corpus(size);
            for query in ["b", "bla", "blade", "blade runner", "brainmir"] {
                let iters: u32 = if size >= 100_000 { 5 } else { 20 };
                let mut hits = 0usize;
                let start = std::time::Instant::now();
                for _ in 0..iters {
                    hits += c.search(query).len();
                }
                let per = start.elapsed() / iters;
                println!(
                    "{size:>7} items  {:<14} {per:>12.3?}  ({} hits)",
                    query,
                    hits / iters as usize
                );
            }
        }
    }

    /// Mirror of the above: when nothing in the corpus is a strong
    /// match, the relative cutoff must not prune the list down to
    /// nothing/near-nothing; weak guesses are exactly what should show
    /// when there's nothing better on offer.
    #[test]
    fn weak_matches_still_show_when_nothing_is_a_great_match() {
        let client = test_client();
        let mut c = Corpus::new();
        c.add_subsonic_search_result(
            SubsonicSearchResult {
                artists: vec![],
                albums: vec![
                    Album {
                        id: "al1".into(),
                        name: "Zzyzx".into(),
                        artist: "Someone".into(),
                    },
                    Album {
                        id: "al2".into(),
                        name: "Zzyzx Road".into(),
                        artist: "Someone Else".into(),
                    },
                    Album {
                        id: "al3".into(),
                        name: "Zzyzx Junction".into(),
                        artist: "Another Artist".into(),
                    },
                ],
                songs: vec![],
            },
            &client,
        );

        // none of these clearly beats the others (all three contain
        // "zyzx" as a word); with no standout leader, all three should
        // stay close enough in score to the top one to clear the
        // relative cutoff together, rather than the two runners-up
        // getting cut just for not being *the* best.
        let results = c.search("zyzx");
        assert_eq!(
            results.len(),
            3,
            "no standout match among near-equal candidates; none should be cut: got {results:?}"
        );
    }
}
