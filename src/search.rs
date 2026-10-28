//! Unified fuzzy search: one ranked list across local music, local video,
//! and every Navidrome entity type, rather than per-source silos. This is
//! the single most-emphasized requirement in the spec; searching "bla"
//! should surface matching artists, albums, songs, playlists, and local
//! files together, ranked by actual relevance, not just substring hits.

use std::borrow::Cow;
use std::cell::RefCell;
use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::{Arc, OnceLock};
use std::thread;

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

/// A corpus item plus what `search` would otherwise rebuild for every
/// item on every keystroke: the matcher text in its original case
/// (nucleo's smart-case matching reads it), a lowercased copy the
/// edit-distance fallback splits into words, and the char mask both
/// prefilters test against.
#[derive(Debug, Clone)]
struct Entry {
    item: SearchItem,
    haystack: String,
    haystack_lower: String,
    /// `char_mask` of `haystack_lower`, or `u64::MAX` when the
    /// haystack is not ascii. nucleo matches an ascii pattern against
    /// *normalized* text ("blade" hits "blàde"), so a raw char mask
    /// would reject accented haystacks it shouldn't; `MAX` opts those
    /// out of both prefilters instead of guessing.
    mask: u64,
    /// Bit `n` set when some word of the haystack is `n` chars long,
    /// anything from 31 up sharing bit 31. The typo pass needs a word
    /// within the budget of the query's length, so one and-test
    /// retires an entry whose words are all the wrong size.
    word_lens: u32,
    /// Whether the haystack is ascii, letting the typo pass split on
    /// bytes and take `len()` as the char count. Unicode whitespace
    /// outside ascii splits into different words, so this is a real
    /// precondition rather than only a fast path.
    ascii: bool,
}

impl Entry {
    fn new(item: SearchItem) -> Self {
        let haystack = item.haystack();
        let haystack_lower = haystack.to_lowercase();
        let ascii = haystack_lower.is_ascii();
        Self {
            mask: if ascii {
                char_mask(haystack_lower.chars())
            } else {
                u64::MAX
            },
            word_lens: haystack_lower
                .split_whitespace()
                .fold(0u32, |bits, word| bits | 1 << char_count(word).min(31)),
            ascii,
            item,
            haystack,
            haystack_lower,
        }
    }
}

/// One bit per character, `ch as u32 % 64`, which gives every ascii
/// letter and digit its own bit and lets anything else collide. Two
/// cheap necessary conditions ride on this: a subsequence match needs
/// every pattern char present in the haystack, and an edit distance is
/// at least the number of distinct chars one side has and the other
/// lacks. Collisions only ever drop a bit from the "missing" set, so
/// they weaken the filters and can never reject a real match.
fn char_mask(chars: impl Iterator<Item = char>) -> u64 {
    chars.fold(0u64, |mask, ch| mask | 1u64 << (ch as u32 % 64))
}

/// Everything derived from the query that is identical for every item,
/// hoisted out of the per-item loop.
#[derive(Debug)]
struct Needle {
    pattern: Pattern,
    /// Lowercased query text, what the edit-distance pass compares
    /// against each haystack word.
    lower: String,
    /// `true` when `lower` is ascii, so the distance fast path can
    /// treat bytes as chars.
    ascii: bool,
    len: usize,
    budget: usize,
    /// Chars every fuzzy match must contain, or 0 (prefilter off) when
    /// the query holds anything nucleo's pattern parser might read as
    /// syntax rather than as literal haystack text.
    fuzzy_mask: u64,
    /// Chars of the whole query, for the edit-distance lower bound.
    typo_mask: u64,
    /// Word lengths that could come in under budget, in `Entry`'s
    /// `word_lens` encoding.
    length_window: u32,
}

impl Needle {
    fn new(query: &str) -> Self {
        let lower = query.to_lowercase();
        let len = lower.chars().count();
        // nucleo reads `^ $ ! ' \ |` and whitespace as pattern syntax
        // (anchors, negation, exact-match, escapes), so only a plain
        // alphanumeric query is guaranteed to be literal text that a
        // match has to actually contain.
        let literal = lower.chars().all(|c| c.is_ascii_alphanumeric() || c == ' ');
        let budget = edit_distance_budget(len);
        Self {
            pattern: Pattern::parse(query, CaseMatching::Smart, Normalization::Smart),
            ascii: lower.is_ascii(),
            len,
            budget,
            length_window: length_window(len, budget),
            fuzzy_mask: if literal {
                char_mask(lower.chars().filter(|c| *c != ' '))
            } else {
                0
            },
            typo_mask: char_mask(lower.chars()),
            lower,
        }
    }

    /// How many distinct chars `self` has that `mask` lacks; a lower
    /// bound on the edit distance (every one of them needs at least an
    /// insertion or a substitution), so over budget means no match.
    fn missing_from(&self, mask: u64) -> usize {
        (self.typo_mask & !mask).count_ones() as usize
    }
}

/// Smallest length difference between `len` and any word the haystack
/// has, read straight off `Entry::word_lens`. A lower bound on the best
/// edit distance the typo pass could possibly report, and so (through
/// `typo_score`) an upper bound on the score it could add: when the
/// fuzzy score already beats that, the pass cannot change the answer
/// and is skipped. The saturating 31-char bucket makes this an
/// underestimate for very long words, which only ever means running a
/// pass that could have been skipped.
fn min_length_gap(word_lens: u32, len: usize) -> usize {
    if word_lens == 0 {
        // no words at all: 32 is past every bucket, and scores as 0,
        // so a haystack like this never reaches the typo pass.
        return 32;
    }
    let target = len.min(31) as u32;
    let at_or_below = word_lens & (!0u32 >> (31 - target));
    let down = if at_or_below == 0 {
        u32::MAX
    } else {
        target - (31 - at_or_below.leading_zeros())
    };
    let up = (word_lens >> target).trailing_zeros();
    down.min(up) as usize
}

/// Every word length within `budget` of `len`, in `Entry::word_lens`'s
/// encoding. Lengths from 31 up all share bit 31, which only ever
/// widens the window (a 40-char word still tests as a candidate for a
/// 33-char query), so the exact per-word length check still has to run.
fn length_window(len: usize, budget: usize) -> u32 {
    let lo = len.saturating_sub(budget).min(31) as u32;
    let hi = (len + budget).min(31) as u32;
    (!0u32 >> (31 - (hi - lo))) << lo
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
    /// One scratch per scoring thread (index 0 doubles as the
    /// single-threaded one), kept for the corpus's whole life: a fresh
    /// `Matcher` per fan-out thread per keystroke meant allocating and
    /// touching ~1mb of dp matrices on every typed character.
    ///
    /// `search` takes `&self` (it is called on shared corpus bindings,
    /// and only ever from the input thread), so these live behind a
    /// `RefCell` instead of forcing `&mut` on every caller; `search`
    /// is not reentrant, so the borrow can't conflict.
    scratches: RefCell<Vec<Scratch>>,
}

impl Corpus {
    pub fn new() -> Self {
        Self::default()
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
        let scored = self.score_all(query);
        rank(scored)
    }

    /// Phase one of `search`: every item that either signal fired on,
    /// with its score, in corpus order. Split out from the ranking so
    /// the two can be benchmarked separately.
    fn score_all(&self, query: &str) -> Vec<(u32, &SearchItem)> {
        self.score_all_maybe_parallel(query, self.items.len() >= PARALLEL_SCORING_FLOOR)
    }

    /// `score_all` with the fan-out decision forced, so a benchmark can
    /// measure the per-item cost single-threaded (immune to whatever
    /// else is loading the machine) as well as end to end.
    fn score_all_maybe_parallel(&self, query: &str, parallel: bool) -> Vec<(u32, &SearchItem)> {
        if query.trim().is_empty() {
            return Vec::new();
        }
        let needle = Needle::new(query);
        let mut scratches = self.scratches.borrow_mut();
        if scratches.is_empty() {
            scratches.resize_with(thread_count().max(1), Scratch::default);
        }

        if parallel {
            // chunks are joined back in corpus order, so the stable
            // sort below still breaks ties exactly as it does
            // single-threaded; each thread takes one of the corpus's
            // own scratches rather than building its own.
            let chunk = self.items.len().div_ceil(scratches.len());
            let parts: Vec<Vec<(u32, &SearchItem)>> = thread::scope(|scope| {
                self.items
                    .chunks(chunk)
                    .zip(scratches.iter_mut())
                    .map(|(part, scratch)| scope.spawn(|| score_chunk(part, &needle, scratch)))
                    .collect::<Vec<_>>()
                    .into_iter()
                    .map(|handle| handle.join().unwrap_or_default())
                    .collect()
            });
            parts.concat()
        } else {
            score_chunk(&self.items, &needle, &mut scratches[0])
        }
    }
}

/// Cores to fan scoring out over, asked once rather than per keystroke:
/// `available_parallelism` reads the current affinity mask/cgroup quota
/// every call, and the answer cannot change under us in a way that
/// matters here.
fn thread_count() -> usize {
    static COUNT: OnceLock<usize> = OnceLock::new();
    *COUNT.get_or_init(|| thread::available_parallelism().map_or(1, |n| n.get()))
}

/// Phase two of `search`: sort, cut weak matches, group by category,
/// materialize. Takes the scored candidates in corpus order, which the
/// stable sort relies on for tie-breaking.
fn rank(mut scored: Vec<(u32, &SearchItem)>) -> Vec<SearchItem> {
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
    //
    // Cut before sorting, not after: the threshold depends only on the
    // best score, so one max pass finds it, and a broad query ("bla"
    // over a 40k library) then sorts the few thousand survivors
    // instead of the tens of thousands of candidates. `retain` keeps
    // corpus order, which the stable sort below needs for ties, so
    // this is the same list in the same order either way.
    let Some(top_score) = scored.iter().map(|&(score, _)| score).max() else {
        return Vec::new();
    };
    let threshold = (top_score as f64 * WEAK_MATCH_CUTOFF_FRACTION).round() as u32;
    scored.retain(|(s, _)| *s >= threshold);
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
    let mut out = Vec::with_capacity(scored.len());
    for cat in group_order {
        out.extend(
            scored
                .iter()
                .filter(|(_, item)| item.category() == cat)
                .map(|(_, item)| (*item).clone()),
        );
    }
    out
}

/// Scores one slice of the corpus against `pattern`, reusing a single
/// `Scratch` for every item; the whole per-item hot path lives here so
/// the threaded and single-threaded paths can't score differently.
/// Keeps corpus order, which the caller's stable sort relies on.
fn score_chunk<'a>(
    entries: &'a [Entry],
    needle: &Needle,
    scratch: &mut Scratch,
) -> Vec<(u32, &'a SearchItem)> {
    let Scratch { matcher, buf } = scratch;
    let mut scored = Vec::new();
    for entry in entries {
        // a subsequence match needs every pattern char somewhere in
        // the haystack, so one mask test retires the items nucleo
        // would walk the whole haystack only to reject.
        let fuzzy_score = if needle.fuzzy_mask & !entry.mask != 0 {
            None
        } else {
            let utf32 = Utf32Str::new(&entry.haystack, buf);
            needle.pattern.score(utf32, matcher)
        };

        // four free skips of the edit-distance pass, none of which can
        // change the final score: the distance is at least the length
        // difference, so a haystack with no word anywhere near the
        // query's length is out, and where the closest-length word
        // still bounds the best typo score below what the fuzzy pass
        // already scored, the pass cannot win; and the distance is
        // also at least the number of query chars the haystack lacks
        // entirely.
        let typo = if entry.word_lens & needle.length_window == 0
            || fuzzy_score
                .is_some_and(|s| s >= typo_score(min_length_gap(entry.word_lens, needle.len)))
            || needle.missing_from(entry.mask) > needle.budget
        {
            None
        } else {
            best_typo_score(entry, needle)
        };

        match (fuzzy_score, typo) {
            (None, None) => {}
            (a, b) => scored.push((a.unwrap_or(0).max(b.unwrap_or(0)), &entry.item)),
        }
    }
    scored
}

/// Best (lowest-distance) typo score over the haystack's words, or
/// `None` if no word comes in under budget.
fn best_typo_score(entry: &Entry, needle: &Needle) -> Option<u32> {
    let mut best: Option<usize> = None;
    // `true` to stop: distance 0 is the best score there is.
    let mut consider = |word: &str, len: usize| {
        if len.abs_diff(needle.len) > needle.budget {
            return false;
        }
        let dist = if needle.budget == 1 && needle.ascii && entry.ascii {
            distance_at_most_one(needle.lower.as_bytes(), word.as_bytes())
        } else if word_is_reachable(word, needle) {
            let dist = strsim::damerau_levenshtein(&needle.lower, word);
            (dist <= needle.budget).then_some(dist)
        } else {
            None
        };
        match dist {
            Some(dist) if best.is_none_or(|b| dist < b) => {
                best = Some(dist);
                dist == 0
            }
            _ => false,
        }
    };
    // an ascii haystack has no unicode whitespace to split on, so the
    // byte-wise split sees exactly the same words for a fraction of
    // the work, and the word length is just its byte length.
    if entry.ascii {
        for word in entry.haystack_lower.split_ascii_whitespace() {
            if consider(word, word.len()) {
                break;
            }
        }
    } else {
        for word in entry.haystack_lower.split_whitespace() {
            if consider(word, word.chars().count()) {
                break;
            }
        }
    }
    best.map(typo_score)
}

/// Whether `word` is close enough to the query to be worth the full
/// distance computation: each distinct char one side has and the other
/// lacks costs at least one edit, in both directions.
fn word_is_reachable(word: &str, needle: &Needle) -> bool {
    let mask = char_mask(word.chars());
    needle.missing_from(mask) <= needle.budget
        && (mask & !needle.typo_mask).count_ones() as usize <= needle.budget
}

/// `Some(damerau_levenshtein(a, b))` when that distance is 0 or 1,
/// `None` when it is more, over ascii bytes and without strsim's dp
/// table and position hashmap. Exact rather than approximate: a single
/// edit is trivially an edit no substring sees twice, so the restricted
/// and unrestricted distances agree at 1, and every way to be one edit
/// apart is one of the four cases below. Only used where the budget
/// *is* 1; a budget of 2 keeps the full strsim distance, where that
/// equivalence no longer holds (classically "ca" to "abc": 2
/// unrestricted, 3 restricted).
fn distance_at_most_one(a: &[u8], b: &[u8]) -> Option<usize> {
    if a.len().abs_diff(b.len()) > 1 {
        return None;
    }
    let common = a.iter().zip(b).take_while(|(x, y)| x == y).count();
    if common == a.len() && common == b.len() {
        return Some(0);
    }
    let (ra, rb) = (&a[common..], &b[common..]);
    if a.len() == b.len() {
        // one substitution, or one adjacent transposition
        let sub = ra[1..] == rb[1..];
        let swap = ra.len() >= 2 && ra[0] == rb[1] && ra[1] == rb[0] && ra[2..] == rb[2..];
        return (sub || swap).then_some(1);
    }
    // one insertion or deletion: drop the extra char from the longer side
    let (long, short) = if a.len() > b.len() {
        (ra, rb)
    } else {
        (rb, ra)
    };
    (long[1..] == *short).then_some(1)
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
/// threads. Below it the fixed ~0.4ms of spawn/join dominates, and the
/// char-mask prefilters cut the per-item cost enough that the floor had
/// to move up an order of magnitude: measured on 8 cores
/// (`bench_parallel_floor`), sequential wins at 4k for every query
/// shape and at 8k for all but one, threads win at 16k and up for
/// everything except a query that matches nothing at all, which is so
/// cheap that no corpus size pays back the fan-out. Below the floor,
/// scoring a whole 8k corpus sequentially is still only ~2ms.
const PARALLEL_SCORING_FLOOR: usize = 16_000;

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

        // "brainmirr": the same transposition plus a doubled last
        // letter, so distance 2, which only a query this long has the
        // budget for. Pins that queries over the one-edit budget keep
        // the full distance rather than the budget-1 fast path.
        let two_edits = c.search("brainmirr");
        assert!(
            two_edits
                .iter()
                .any(|i| matches!(i, SearchItem::Artist { name, .. } if name == "Branimir")),
            "a 9-char query has a 2-edit budget and must still match"
        );
    }

    /// `min_length_gap` has to agree with the obvious loop over the
    /// set bits, for every single- and two-word length mask.
    #[test]
    fn min_length_gap_finds_the_nearest_word_length() {
        let expected = |mask: u32, len: usize| -> usize {
            let target = len.min(31);
            (0..32)
                .filter(|bit| mask & (1 << bit) != 0)
                .map(|bit: usize| bit.abs_diff(target))
                .min()
                .unwrap_or(32)
        };
        let masks = (0..32u32)
            .flat_map(|a| (0..32u32).map(move |b| (1 << a) | (1 << b)))
            .chain([0, !0]);
        for mask in masks {
            for len in 0..40usize {
                assert_eq!(
                    min_length_gap(mask, len),
                    expected(mask, len),
                    "mask {mask:#034b} len {len}"
                );
            }
        }
    }

    /// The word-length window has to keep every length the budget can
    /// actually reach (a dropped length silently loses typo matches)
    /// and, below the saturating 31-char bucket, nothing else. Checked
    /// white-box because at the window's top edge the two signals
    /// overlap: a word longer than the query by the whole budget is
    /// only reachable by insertions, which makes the query a
    /// subsequence of it, so the fuzzy pass hides the bug from any
    /// end-to-end membership check.
    #[test]
    fn length_window_covers_exactly_the_reachable_word_lengths() {
        for len in 0..40usize {
            for budget in 0..4usize {
                let window = length_window(len, budget);
                for word_len in 0..60usize {
                    let kept = window & (1 << word_len.min(31)) != 0;
                    if word_len.abs_diff(len) <= budget {
                        assert!(
                            kept,
                            "len {len} budget {budget} drops {word_len}-char words"
                        );
                    } else if word_len < 31 && len + budget < 31 {
                        assert!(
                            !kept,
                            "len {len} budget {budget} keeps {word_len}-char words"
                        );
                    }
                }
            }
        }
    }

    /// Exhaustive proof of the fast distance's equivalence to strsim
    /// at a budget of 1: every pair of strings up to 4 chars over a
    /// 3-letter alphabet (plus a few longer hand-picked ones), which
    /// covers every substitution, insertion, deletion and adjacent
    /// transposition shape there is.
    #[test]
    fn distance_at_most_one_matches_strsim_exactly() {
        let mut words: Vec<String> = vec![String::new()];
        for _ in 0..4 {
            let grown: Vec<String> = words
                .iter()
                .flat_map(|w| "abc".chars().map(move |c| format!("{w}{c}")))
                .collect();
            words.extend(grown);
        }
        words.extend(
            [
                "branimir",
                "brainmir",
                "icedancer",
                "icedacnre",
                "blade",
                "balde",
            ]
            .map(String::from),
        );
        for a in &words {
            for b in &words {
                let real = strsim::damerau_levenshtein(a, b);
                let fast = distance_at_most_one(a.as_bytes(), b.as_bytes());
                let want = (real <= 1).then_some(real);
                assert_eq!(fast, want, "distance {a:?} -> {b:?} is {real}");
            }
        }
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

    /// Second synthetic corpus, same generator shape but a *wide*
    /// vocabulary and all five item kinds, so match counts and word
    /// shapes look like a real 40k library rather than 12 artists
    /// repeated 3000 times each. `synthetic_corpus` is the worst case
    /// for result-set size; this one is the realistic case.
    fn varied_corpus(n: usize) -> Corpus {
        const WORDS: [&str; 48] = [
            "blade",
            "runner",
            "blues",
            "cold",
            "visions",
            "crest",
            "icedancer",
            "eversince",
            "obedient",
            "rewind",
            "decay",
            "drownin",
            "sunset",
            "mall",
            "western",
            "union",
            "hotel",
            "breakfast",
            "into",
            "dust",
            "tears",
            "joy",
            "paranoid",
            "blackout",
            "selected",
            "ambient",
            "works",
            "music",
            "right",
            "children",
            "lift",
            "skinny",
            "fists",
            "college",
            "dropout",
            "justice",
            "metal",
            "essentials",
            "midnight",
            "city",
            "velvet",
            "mirror",
            "static",
            "garden",
            "ghost",
            "parade",
            "winter",
            "signal",
        ];
        // deterministic lcg; a real library's vocabulary is wide and
        // its distribution lumpy, and a fixed stride would correlate
        // every field of an item with every other.
        let mut state = 0x2545_F491_4F6C_DD1Du64;
        let mut next = move |m: usize| {
            state = state
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            ((state >> 33) as usize) % m
        };
        let mut words = move |k: usize| {
            (0..k)
                .map(|_| WORDS[next(WORDS.len())])
                .collect::<Vec<_>>()
                .join(" ")
        };
        let client = test_client();
        let mut c = Corpus::new();
        // same mix a real library has: mostly songs, then albums,
        // artists, a few playlists and videos.
        let songs = n * 80 / 100;
        let albums = n * 12 / 100;
        let artists = n * 6 / 100;
        c.add_local_tracks((0..songs / 2).map(|i| LocalTrack {
            path: PathBuf::from(format!("/music/{i:06}.flac")),
            title: words(2),
            artist: Some(words(2)),
            album: Some(words(2)),
            duration_secs: None,
            kind: MediaKind::Music,
        }));
        c.add_local_tracks((0..n / 100).map(|i| LocalTrack {
            path: PathBuf::from(format!("/video/{i:06}.mkv")),
            title: words(3),
            artist: None,
            album: None,
            kind: MediaKind::Video,
            duration_secs: None,
        }));
        c.add_subsonic_search_result(
            SubsonicSearchResult {
                artists: (0..artists)
                    .map(|i| Artist {
                        id: format!("ar{i}"),
                        name: words(2),
                    })
                    .collect(),
                albums: (0..albums)
                    .map(|i| Album {
                        id: format!("al{i}"),
                        name: words(2),
                        artist: words(2),
                    })
                    .collect(),
                songs: (0..songs / 2)
                    .map(|i| Song {
                        id: format!("s{i}"),
                        title: words(2),
                        artist: words(2),
                        album: words(2),
                        duration: None,
                        genres: Vec::new(),
                    })
                    .collect(),
            },
            &client,
        );
        c.add_playlists(
            (0..n / 200).map(|i| Playlist {
                id: format!("p{i}"),
                name: words(2),
            }),
            &client,
        );
        c
    }

    /// Every query shape the correctness guards sweep: short/broad
    /// prefixes, exact hits, multi-word, typos, no-match, mixed case,
    /// non-ascii, and the characters nucleo's own pattern parser
    /// treats as operators (`^ $ ! ' \ |`), which any prefilter must
    /// not assume are literal haystack text.
    const GUARD_QUERIES: [&str; 38] = [
        // one char short of the 5-char word it matches, i.e. a word
        // sitting on the top edge of the length window, where the
        // typo score (85) beats what nucleo gives a gappy match
        "blde",
        // the long word again with two letters transposed, so it is
        // reachable only through the edit-distance pass: proves the
        // word-length bucket saturates instead of wrapping
        "averyveryverylongsinglewordtitlethatoverflowsthebukcet",
        // longer than the 31-char word-length bucket, so the length
        // window saturates at its top bit
        "averyveryverylongsinglewordtitlethatoverflowsthebucket",
        // two substitutions over a word whose substituted letters are
        // unique to it, so both directions of the char-mask lower
        // bound sit exactly on a 2-edit budget: "midnight" and
        // "obedient" with two letters each swapped for letters
        // neither word contains
        "xidnighy",
        "obedixnz",
        // adjacent transpositions inside a 1-edit budget, the one
        // shape `distance_at_most_one` handles that a plain
        // substitution/insertion/deletion check would miss
        "balde",
        "clod",
        // 8 and 9 chars, where the budget is 2 and the fast path
        // hands back off to strsim: one edit from "obedient", two
        // (two transpositions) from "icedancer"
        "obedeint",
        "icedacnre",
        "b",
        "bl",
        "bla",
        "blad",
        "blade",
        "bladee",
        "blade runner",
        "blade runner blues",
        "cold visions",
        "crest",
        "icedancer",
        "obedient",
        "ob",
        "obe",
        "god",
        "gods",
        "brainm",
        "brainmi",
        "brainmir",
        "qqqxzz",
        "BLADE",
        "Blade Runner",
        "mIdNiGhT",
        "^blade",
        "blade$",
        "'blade",
        "!blade",
        "blade|runner",
        "blàdé",
    ];

    /// Order-and-membership fingerprint of a result list: fnv-1a over
    /// every item's identity in order. Pinned per query by
    /// `search_results_are_unchanged`, so any reordering, addition or
    /// removal shows up as a failure rather than as a "faster" search.
    fn fingerprint(items: &[SearchItem]) -> u64 {
        let mut hash = 0xcbf2_9ce4_8422_2325u64;
        let mut eat = |bytes: &[u8]| {
            for b in bytes {
                hash ^= u64::from(*b);
                hash = hash.wrapping_mul(0x100_0000_01b3);
            }
        };
        for item in items {
            let (tag, key) = item.identity();
            eat(&[tag]);
            eat(key.as_bytes());
            eat(b"\x1f");
        }
        hash
    }

    /// `(query, result count, fingerprint)` for `varied_corpus(3_000)`,
    /// blessed from the pre-optimization implementation. Any change to
    /// what search returns, or to the order it returns it in, breaks
    /// this; reranking is a product decision, so re-blessing has to be
    /// deliberate. Regenerate by running the test and pasting the table
    /// it prints.
    #[rustfmt::skip]
    const PINNED: [(&str, usize, u64); 38] = [
        ("blde", 485, 0xda38_f4b1_7cf9_c977),
        ("averyveryverylongsinglewordtitlethatoverflowsthebukcet", 0, 0xcbf2_9ce4_8422_2325),
        ("averyveryverylongsinglewordtitlethatoverflowsthebucket", 0, 0xcbf2_9ce4_8422_2325),
        ("xidnighy", 354, 0x304c_09f0_ed6e_ea13),
        ("obedixnz", 315, 0xdfad_bcd4_8e6c_8b68),
        ("balde", 387, 0x385d_1bdd_4dd7_c7df),
        ("clod", 474, 0x6a83_5d47_2ec9_e1c0),
        ("obedeint", 129, 0xb3b3_8077_be08_5dce),
        ("icedacnre", 73, 0xd229_2d69_866b_4280),
        ("b", 1094, 0xb7e9_d075_2c60_f452),
        ("bl", 867, 0xba5b_70fc_7a23_c14b),
        ("bla", 717, 0x2eda_b913_f7b3_caf4),
        ("blad", 501, 0xf650_49a4_b2d9_cccf),
        ("blade", 447, 0x162c_be8f_180a_1ccd),
        ("bladee", 271, 0xebb4_3859_f29c_f61c),
        ("blade runner", 45, 0x6ca9_aefc_081d_eb25),
        ("blade runner blues", 17, 0xce83_cec8_ffab_bee6),
        ("cold visions", 72, 0xad50_0026_04f9_a4a2),
        ("crest", 460, 0x7bc1_6525_b793_7c6d),
        ("icedancer", 333, 0x0ee3_890c_975c_e108),
        ("obedient", 315, 0xdfad_bcd4_8e6c_8b68),
        ("ob", 315, 0xdfad_bcd4_8e6c_8b68),
        ("obe", 316, 0xb7fa_9087_3781_aa47),
        ("god", 211, 0xde03_7bfb_b2cc_a968),
        ("gods", 82, 0xddec_aa68_23ad_5921),
        ("brainm", 30, 0xce74_4cdb_c837_b406),
        ("brainmi", 27, 0x2e81_9569_db6b_55c3),
        ("brainmir", 12, 0xf9e9_71d8_1237_8cc6),
        ("qqqxzz", 0, 0xcbf2_9ce4_8422_2325),
        ("BLADE", 307, 0xf8a9_24ad_cde1_23c5),
        ("Blade Runner", 0, 0xcbf2_9ce4_8422_2325),
        ("mIdNiGhT", 354, 0x304c_09f0_ed6e_ea13),
        ("^blade", 63, 0x27a1_0f63_2322_5e0b),
        ("blade$", 68, 0x3877_9407_b137_8259),
        ("'blade", 307, 0xf8a9_24ad_cde1_23c5),
        ("!blade", 307, 0xf8a9_24ad_cde1_23c5),
        ("blade|runner", 0, 0xcbf2_9ce4_8422_2325),
        ("bl\u{e0}d\u{e9}", 0, 0xcbf2_9ce4_8422_2325),
    ];

    /// Membership and order are the product; speed is not allowed to
    /// change either. See `PINNED`.
    #[test]
    fn search_results_are_unchanged() {
        let c = varied_corpus(3_000);
        let actual: Vec<(&str, usize, u64)> = GUARD_QUERIES
            .iter()
            .map(|q| {
                let r = c.search(q);
                (*q, r.len(), fingerprint(&r))
            })
            .collect();
        if actual.as_slice() != PINNED.as_slice() {
            let table: String = actual
                .iter()
                .map(|(q, n, h)| {
                    let (a, b, c, d) =
                        (h >> 48, (h >> 32) & 0xffff, (h >> 16) & 0xffff, h & 0xffff);
                    format!("        ({q:?}, {n}, 0x{a:04x}_{b:04x}_{c:04x}_{d:04x}),\n")
                })
                .collect();
            panic!("search results changed; fresh table:\n{table}");
        }
    }

    /// The pre-optimization scorer, kept verbatim as the oracle every
    /// prefilter and fast path has to agree with: no char-mask
    /// prefilters, always the full `strsim` distance. Deliberately
    /// naive; `scorers_agree_on_every_query_shape` compares the real
    /// hot path against it item by item.
    fn reference_score(
        entry: &Entry,
        pattern: &Pattern,
        query_lower: &str,
        query_len: usize,
        typo_budget: usize,
        matcher: &mut Matcher,
    ) -> Option<u32> {
        let mut buf = Vec::new();
        let fuzzy_score = pattern.score(Utf32Str::new(&entry.haystack, &mut buf), matcher);
        let typo_score_val = if fuzzy_score.is_some_and(|s| s >= typo_score(0)) {
            None
        } else {
            entry
                .haystack_lower
                .split_whitespace()
                .filter(|word| word.chars().count().abs_diff(query_len) <= typo_budget)
                .map(|word| strsim::damerau_levenshtein(query_lower, word))
                .filter(|&dist| dist <= typo_budget)
                .map(typo_score)
                .max()
        };
        match (fuzzy_score, typo_score_val) {
            (None, None) => None,
            (a, b) => Some(a.unwrap_or(0).max(b.unwrap_or(0))),
        }
    }

    /// The real hot path must score every item exactly as the naive
    /// reference does: same match/no-match decision, same score. This
    /// is the guard that catches a prefilter that rejects something it
    /// should have kept (verified by deliberately breaking one).
    /// Items built to break the prefilters: accented text nucleo
    /// matches an ascii pattern against anyway, unicode whitespace
    /// that splits into different words than ascii whitespace does, a
    /// word past the 31-char length bucket, and a bare digit.
    fn hostile_items(c: &mut Corpus) {
        for (i, title) in [
            "Blàdé Rünner Blües",
            "blade\u{a0}runner\u{3000}blues",
            "averyveryverylongsinglewordtitlethatoverflowsthebucket blade",
            "7",
            "BLADE RUNNER",
            "bladé",
        ]
        .into_iter()
        .enumerate()
        {
            c.add_local_tracks([LocalTrack {
                path: PathBuf::from(format!("/music/hostile{i}.flac")),
                title: title.to_string(),
                artist: None,
                album: None,
                duration_secs: None,
                kind: MediaKind::Music,
            }]);
        }
    }

    #[test]
    fn scorers_agree_on_every_query_shape() {
        let mut c = varied_corpus(4_000);
        hostile_items(&mut c);
        let mut matcher = Matcher::default();
        for query in GUARD_QUERIES {
            let needle = Needle::new(query);
            let mut scratch = Scratch::default();
            let fast = score_chunk(&c.items, &needle, &mut scratch);
            let expected: Vec<(u32, &SearchItem)> = c
                .items
                .iter()
                .filter_map(|e| {
                    reference_score(
                        e,
                        &needle.pattern,
                        &needle.lower,
                        needle.len,
                        needle.budget,
                        &mut matcher,
                    )
                    .map(|s| (s, &e.item))
                })
                .collect();
            assert_eq!(
                fast.len(),
                expected.len(),
                "{query:?}: candidate count differs from the reference scorer"
            );
            for (i, ((fs, fi), (es, ei))) in fast.iter().zip(expected.iter()).enumerate() {
                assert_eq!(
                    (fs, fi.identity()),
                    (es, ei.identity()),
                    "{query:?}: candidate {i} differs from the reference scorer"
                );
            }
        }
    }

    /// Fanning the scoring out across threads must not change a single
    /// result: chunks are rejoined in corpus order precisely so the
    /// stable sort breaks ties the same way at any thread count.
    #[test]
    fn threaded_and_sequential_scoring_agree() {
        let c = varied_corpus(5_000);
        assert!(
            c.items.len() > thread_count(),
            "needs more items than threads, or there is only one chunk to compare"
        );
        for query in GUARD_QUERIES {
            let par = rank(c.score_all_maybe_parallel(query, true));
            let seq = rank(c.score_all_maybe_parallel(query, false));
            assert_eq!(
                fingerprint(&par),
                fingerprint(&seq),
                "{query:?}: threaded scoring changed the result list"
            );
        }
    }

    /// Appending a character can *add* candidates, so a type-ahead
    /// optimization must not narrow the previous result set: the typo
    /// fallback's budget grows with query length, and a longer query
    /// can land inside a word's edit distance that a shorter one
    /// missed. Pinned as a test because the "just narrow the previous
    /// hits" shortcut is otherwise extremely tempting; see
    /// docs/search-performance.md.
    #[test]
    fn appending_a_char_can_add_candidates() {
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
        // "brainm" is 3 edits from "branimir" (budget 1 at that
        // length), "brainmir" is 1 (budget 2): the superset is the
        // longer query, not the shorter one.
        assert!(
            c.search("brainm").is_empty(),
            "precondition: the shorter query must miss"
        );
        assert!(
            !c.search("brainmir").is_empty(),
            "appending chars turned a miss into a hit; narrowing the \
             previous result set would have lost this match"
        );
    }

    /// Minimum of `iters` runs, plus the last run's value. The *minimum*
    /// rather than the mean on purpose: this machine's other cores are
    /// not idle during a bench run (parallel builds), and only the
    /// minimum is close to the uncontended cost.
    fn bench<T>(iters: u32, mut f: impl FnMut() -> T) -> (std::time::Duration, T) {
        let mut best = std::time::Duration::MAX;
        let mut last = f();
        for _ in 0..iters {
            let start = std::time::Instant::now();
            last = f();
            best = best.min(start.elapsed());
        }
        (best, last)
    }

    /// Cpu time, not wall time: this machine's other cores are busy
    /// during a bench run, so wall clock varies 2-3x between runs
    /// while the work done does not. Paired with `parallel = false` it
    /// is the one number that can be compared across commits.
    fn cpu_now() -> std::time::Duration {
        let mut ts = libc::timespec {
            tv_sec: 0,
            tv_nsec: 0,
        };
        // SAFETY: clock_gettime only writes the timespec we hand it.
        unsafe { libc::clock_gettime(libc::CLOCK_PROCESS_CPUTIME_ID, &mut ts) };
        std::time::Duration::new(
            u64::try_from(ts.tv_sec).unwrap_or(0),
            u32::try_from(ts.tv_nsec).unwrap_or(0),
        )
    }

    fn bench_cpu<T>(iters: u32, mut f: impl FnMut() -> T) -> (std::time::Duration, T) {
        let mut best = std::time::Duration::MAX;
        let mut last = f();
        for _ in 0..iters {
            let start = cpu_now();
            last = f();
            best = best.min(cpu_now().saturating_sub(start));
        }
        (best, last)
    }

    /// The whole of `search`, scored on one thread, in cpu time. The
    /// comparable-across-commits number; the threaded wall clock is in
    /// `bench_search`.
    #[test]
    #[ignore = "timing benchmark, run manually"]
    fn bench_cpu_search() {
        for (label, c) in [
            ("narrow-vocab 40k", synthetic_corpus(40_000)),
            ("varied 40k", varied_corpus(40_000)),
            ("varied 150k", varied_corpus(150_000)),
        ] {
            for query in BENCH_QUERIES {
                let (cpu, out) =
                    bench_cpu(8, || rank(c.score_all_maybe_parallel(query, false)).len());
                println!("{label:<18} {query:<14} cpu {cpu:>11.3?}  ({out} hits)");
            }
        }
    }

    const BENCH_QUERIES: [&str; 7] = [
        "b",
        "bla",
        "blade",
        "bladee",
        "blade runner",
        "brainmir",
        "qqqxzz",
    ];

    /// Not an assertion: prints per-query wall time, since absolute
    /// numbers are machine-dependent. Run with
    /// `cargo test --release -- --ignored --nocapture bench_search`.
    /// "b" is the worst case for match count, "brainmir" for the
    /// edit-distance fallback, "qqqxzz" for pure rejection cost.
    #[test]
    #[ignore = "timing benchmark, run manually"]
    fn bench_search() {
        for (label, c) in [
            ("narrow-vocab 40k", synthetic_corpus(40_000)),
            ("varied 40k", varied_corpus(40_000)),
            ("varied 150k", varied_corpus(150_000)),
        ] {
            for query in BENCH_QUERIES {
                let (per, hits) = bench(12, || c.search(query).len());
                println!("{label:<18} {query:<14} {per:>12.3?}  ({hits} hits)");
            }
        }
    }

    /// Where the time goes: scoring (fanned out, and forced
    /// single-threaded so the number is noise-immune) vs ranking
    /// (sort + weak-match cutoff + grouping + materializing).
    #[test]
    #[ignore = "timing benchmark, run manually"]
    fn bench_phases() {
        for (label, c) in [
            ("narrow-vocab 40k", synthetic_corpus(40_000)),
            ("varied 40k", varied_corpus(40_000)),
        ] {
            for query in BENCH_QUERIES {
                let (par, cand) = bench(12, || c.score_all(query).len());
                let (seq, _) = bench(6, || c.score_all_maybe_parallel(query, false).len());
                // ranking is timed on its own, with the scoring it
                // needs as input left outside the clock; subtracting
                // two independently-taken minima is not a measurement.
                let mut rank_t = std::time::Duration::MAX;
                let mut out = 0;
                for _ in 0..12 {
                    let scored = c.score_all(query);
                    let start = std::time::Instant::now();
                    let ranked = rank(scored);
                    rank_t = rank_t.min(start.elapsed());
                    out = ranked.len();
                }
                println!(
                    "{label:<18} {query:<14} score {par:>10.3?} (1 thread {seq:>10.3?})  \
                     rank {rank_t:>10.3?}   cand {cand:>6} out {out:>6}"
                );
            }
        }
    }

    /// Where fanning the scoring out across threads starts paying for
    /// its own spawn/join cost; sets `PARALLEL_SCORING_FLOOR`.
    #[test]
    #[ignore = "timing benchmark, run manually"]
    fn bench_parallel_floor() {
        for size in [500usize, 1_000, 2_000, 4_000, 8_000, 16_000, 32_000] {
            let c = varied_corpus(size);
            for query in ["b", "bladee", "brainmir", "qqqxzz"] {
                let (seq, _) = bench(20, || c.score_all_maybe_parallel(query, false).len());
                let (par, _) = bench(20, || c.score_all_maybe_parallel(query, true).len());
                let verdict = if par < seq {
                    "threads win"
                } else {
                    "sequential wins"
                };
                println!(
                    "{:>6} items  {query:<10} seq {seq:>10.3?}  par {par:>10.3?}  {verdict}",
                    c.items.len()
                );
            }
        }
    }

    /// The shape that actually matters: one keystroke at a time, each
    /// re-searching with one more char. Prints the per-keystroke cost
    /// (min over replays of the whole sequence).
    #[test]
    #[ignore = "timing benchmark, run manually"]
    fn bench_typeahead() {
        for (label, c) in [
            ("narrow-vocab 40k", synthetic_corpus(40_000)),
            ("varied 40k", varied_corpus(40_000)),
        ] {
            for typed in ["bladee", "blade runner", "qqqxzz"] {
                let chars: Vec<char> = typed.chars().collect();
                let mut best = vec![std::time::Duration::MAX; chars.len()];
                let mut hits = vec![0usize; chars.len()];
                for _ in 0..6 {
                    for (i, _) in chars.iter().enumerate() {
                        let prefix: String = chars[..=i].iter().collect();
                        let start = std::time::Instant::now();
                        let out = c.search(&prefix);
                        best[i] = best[i].min(start.elapsed());
                        hits[i] = out.len();
                    }
                }
                let total: std::time::Duration = best.iter().sum();
                println!("{label:<18} typing {typed:?}: total {total:>10.3?}");
                for (i, t) in best.iter().enumerate() {
                    let prefix: String = chars[..=i].iter().collect();
                    println!("    {:<14} {t:>10.3?}  ({} hits)", prefix, hits[i]);
                }
            }
        }
    }

    /// Throwaway probe: is the *candidate* set (pre-cutoff) monotone
    /// shrinking as a query grows by one char? Prints counterexamples.
    #[test]
    #[ignore = "diagnostic probe, run manually"]
    fn probe_monotonicity() {
        let c = synthetic_corpus(20_000);
        let words = [
            "blade runner blues",
            "bladee",
            "brainmir",
            "branimir stulic",
            "cigarettes",
            "icedancer",
            "selena gomez",
            "godspeed",
            "obedient",
            "zzyzx",
            "lesna",
            "crest",
            "music has the right",
        ];
        let mut breaks = 0usize;
        for w in words {
            let chars: Vec<char> = w.chars().collect();
            for len in 1..chars.len() {
                let short: String = chars[..len].iter().collect();
                let long: String = chars[..len + 1].iter().collect();
                let a: HashSet<(u8, String)> = c
                    .score_all(&short)
                    .into_iter()
                    .map(|(_, i)| {
                        let (t, k) = i.identity();
                        (t, k.into_owned())
                    })
                    .collect();
                let mut escaped = Vec::new();
                for (_, item) in c.score_all(&long) {
                    let (t, k) = item.identity();
                    let key = (t, k.into_owned());
                    if !a.contains(&key) {
                        escaped.push((key, item.haystack()));
                    }
                }
                if !escaped.is_empty() {
                    breaks += 1;
                    println!(
                        "NOT MONOTONE: {short:?} -> {long:?}: {} new candidates, e.g. {:?}",
                        escaped.len(),
                        &escaped[..escaped.len().min(3)]
                    );
                }
            }
        }
        println!("{breaks} non-monotone steps");
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
