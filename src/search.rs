//! Unified fuzzy search: one ranked list across local music, local video,
//! and every Navidrome entity type, rather than per-source silos. This is
//! the single most-emphasized requirement in the spec; searching "bla"
//! should surface matching artists, albums, songs, playlists, and local
//! files together, ranked by actual relevance, not just substring hits.
//!
//! This runs on every keystroke over the whole library, so the hot
//! path carries several exact short circuits plus a load-bearing
//! reason *not* to narrow the previous result set; read
//! docs/search-performance.md before adding a short circuit, a cache,
//! or a cap here.

use std::borrow::Cow;
use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::LazyLock;
use std::sync::{Arc, OnceLock};
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
    boundaries_of(items.iter().map(|item| item.category()))
}

/// The run-length scan both `group_boundaries` and
/// `Ranking::group_boundaries` are, over categories alone.
///
/// Split out because a header's position depends on nothing but the
/// sequence of categories, which is what lets the ranked-index caller
/// answer the question without building a single `SearchItem`; two
/// copies of this loop could disagree about where a header lands.
fn boundaries_of(cats: impl Iterator<Item = Category>) -> Vec<(usize, Category)> {
    let mut boundaries = Vec::new();
    let mut last = None;
    for (i, cat) in cats.enumerate() {
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
    /// A timed silence in the queue: the maintainer's "let me put a 15m
    /// pause in my queue and it'll just take that into account".
    ///
    /// A real entry rather than a flag beside the queue, because
    /// `PlayQueue`'s invariant is that `entries` *is* mpv's playlist,
    /// and mpv will load `av://lavfi:anullsrc=d=N` as a file like any
    /// other. So every absolute-index path, the queue view, reordering,
    /// removal and the desync guards all keep working with no notion of
    /// this variant at all.
    ///
    /// It is deliberately a variant and not a `LocalMusic` with an
    /// `av://` path, which would have cost no match arms: the sites
    /// that must treat it differently are scrobbling, starring,
    /// downloading and cover art, and each of those would have
    /// silently done the wrong thing (a silence scrobbled to
    /// listenbrainz) instead of failing to compile.
    Silence {
        secs: u64,
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
            // never in the corpus, so never scored; answered rather
            // than left unreachable because a silence does have a
            // name and this is where names live.
            SearchItem::Silence { .. } => "silence".to_string(),
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
            SearchItem::LocalMusic { .. }
            | SearchItem::LocalVideo { .. }
            | SearchItem::Silence { .. } => None,
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
            // the duration is the identity: two silences of different
            // lengths are different entries, and two of the same
            // length are interchangeable.
            SearchItem::Silence { secs } => (6, Cow::Owned(secs.to_string())),
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
            // bare on purpose: the duration is decoration and belongs
            // in `results_list::label`, which is also the only form
            // that can own a formatted string.
            SearchItem::Silence { .. } => "silence",
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
            // unreachable: a silence is built into a queue, never
            // added to the corpus, so it is never sectioned. songs
            // rather than a new `Category` variant, which would add
            // an empty heading to every sidebar for a row that cannot
            // appear in it.
            SearchItem::Silence { .. } => Category::Songs,
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
            SearchItem::LocalMusic { .. }
            | SearchItem::LocalVideo { .. }
            | SearchItem::Silence { .. } => None,
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
            SearchItem::LocalMusic { .. }
            | SearchItem::LocalVideo { .. }
            | SearchItem::Silence { .. } => None,
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
        // folded here, once per item at index time, rather than per
        // keystroke. a folded Romanian haystack is pure ascii, so it
        // also stops opting out of both prefilters below.
        let haystack = fold_diacritics(&item.haystack()).into_owned();
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

/// Accented letters by the ascii letter they fold to, lowercase only;
/// the uppercase half is derived, so the two can never disagree.
///
/// Latin-1 Supplement and Latin Extended-A, plus the Latin Extended-B
/// letters Romanian actually uses. The multi-letter expansions are
/// deliberately single letters (`ß` -> `s`, `æ` -> `a`, `œ` -> `o`):
/// this feeds a *fuzzy* matcher, so "strasse" still finds "straße"
/// through one inserted char, and a char-to-char fold keeps the
/// haystack's length and word boundaries intact, which both prefilters
/// depend on.
const FOLD: &[(char, &str)] = &[
    ('a', "àáâãäåāăąǎæ"),
    ('c', "çćĉċč"),
    ('d', "ďđð"),
    ('e', "èéêëēĕėęě"),
    ('g', "ĝğġģ"),
    ('h', "ĥħ"),
    ('i', "ìíîïĩīĭįıǐ"),
    ('j', "ĵ"),
    ('k', "ķ"),
    ('l', "ĺļľŀł"),
    ('n', "ñńņňŉ"),
    ('o', "òóôõöøōŏőœǒ"),
    ('r', "ŕŗř"),
    // `ș` (U+0219, comma below) is the one that mattered: nucleo's own
    // normalization covers Latin-1 and Extended-A but not these, so
    // `si` scored `None` against `Și` before this existed.
    ('s', "śŝşšșß"),
    ('t', "ţťŧțþ"),
    ('u', "ùúûüũūŭůűųǔ"),
    ('w', "ŵ"),
    ('y', "ýÿŷ"),
    ('z', "źżž"),
];

static FOLDED: LazyLock<HashMap<char, char>> = LazyLock::new(|| {
    let mut map = HashMap::new();
    for (base, accents) in FOLD {
        for accent in accents.chars() {
            map.insert(accent, *base);
            // `ß` uppercases to two chars and has no single-char
            // capital, so there is nothing to insert for it.
            let mut upper = accent.to_uppercase();
            if let (Some(u), None) = (upper.next(), upper.next()) {
                map.insert(u, base.to_ascii_uppercase());
            }
        }
    }
    map
});

/// Strips diacritics, preserving case and length.
///
/// **Why this exists rather than leaning on nucleo.** nucleo
/// normalizes the haystack itself, so "blade" does find "blàde", and
/// that covers Latin-1 and Latin Extended-A. It does **not** cover
/// Latin Extended-B, which is exactly where Romanian's `ș` and `ț`
/// live: measured, `Pattern::score("si", "Și")` is `None`, as are
/// `tara`/`Țara` and `taranu`/`Țăranu`, while `imparat`/`Împărat`
/// scores fine because `î` and `ă` are in the ranges nucleo does know.
/// Short queries appeared to work only because the edit-distance
/// fallback was absorbing them as typos.
///
/// Borrowed for ascii input, which is the overwhelming majority of
/// both queries and haystacks, so the common path allocates nothing.
pub fn fold_diacritics(text: &str) -> Cow<'_, str> {
    if text.is_ascii() {
        return Cow::Borrowed(text);
    }
    Cow::Owned(
        text.chars()
            .map(|c| {
                if c.is_ascii() {
                    c
                } else {
                    FOLDED.get(&c).copied().unwrap_or(c)
                }
            })
            .collect(),
    )
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
    /// Lowercased terms from `!term` tokens; a match containing any of
    /// them is dropped. See `split_exclusions`.
    excluded: Vec<String>,
}

/// Splits a raw query into the part that must match and the lowercased
/// `!term` parts that must not.
///
/// nucleo 0.3 parses `!foo` but `Pattern::score` does not honour it: a
/// `!foo` query scores identically to `'foo`, so the exclusion silently
/// did nothing while reading like working syntax. Handled here instead
/// of being handed to nucleo. A bare `!` is not an exclusion (there is
/// no term), so it stays in the positive text as the literal the user
/// typed.
fn split_exclusions(query: &str) -> (String, Vec<String>) {
    let mut positive: Vec<&str> = Vec::new();
    let mut excluded: Vec<String> = Vec::new();
    for token in query.split_whitespace() {
        match token.strip_prefix('!') {
            Some(term) if !term.is_empty() => excluded.push(term.to_lowercase()),
            _ => positive.push(token),
        }
    }
    (positive.join(" "), excluded)
}

impl Needle {
    fn new(query: &str) -> Self {
        let (positive, excluded) = split_exclusions(query);
        // the query is folded to match the folded haystacks; nucleo
        // gets the folded text too, since it cannot fold these itself.
        let positive = fold_diacritics(&positive).into_owned();
        let query = positive.as_str();
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
            excluded,
        }
    }

    /// Whether `haystack_lower` holds any excluded term. Substring, not
    /// word-boundary, matching: `!live` should drop "Live at Leeds" and
    /// "Alive" alike is the wrong call, but a word-boundary rule would
    /// keep "(Live)" only by luck of the punctuation, so the blunter
    /// rule is the predictable one.
    fn excludes(&self, haystack_lower: &str) -> bool {
        self.excluded.iter().any(|t| haystack_lower.contains(t))
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

/// A persisted-catalog load running on its own thread. Started before
/// the first blocking thing `main` does and `take`n where the corpus is
/// actually needed, so the disk read overlaps the mpv spawn instead of
/// landing on top of it.
pub struct WarmingCorpus(thread::JoinHandle<Corpus>);

impl WarmingCorpus {
    /// Blocks until the load finishes; an empty corpus if its thread
    /// panicked, same best-effort stance as every other cache read.
    pub fn take(self) -> Corpus {
        self.0.join().unwrap_or_default()
    }
}

/// Reads every configured server's persisted catalog in the
/// background; see `Corpus::warm_from_cache` for what it builds.
pub fn spawn_warm_from_cache(servers: &[NavidromeConfig]) -> WarmingCorpus {
    let servers = servers.to_vec();
    WarmingCorpus(thread::spawn(move || Corpus::warm_from_cache(&servers)))
}

/// A ranked result list, as indices into the corpus state `rev`.
///
/// The revision travels *with* the order rather than beside it,
/// because a holder that has to remember to keep them together is the
/// failure the revision exists to prevent: an order read against a
/// different state resolves to real-looking but wrong rows. `rows_of`
/// is the only way back to items, and it refuses on a mismatch.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Ranking {
    rev: u64,
    order: Vec<u32>,
}

impl Ranking {
    pub fn len(&self) -> usize {
        self.order.len()
    }

    pub fn is_empty(&self) -> bool {
        self.order.is_empty()
    }

    /// Whether this ranking still addresses `corpus`. A holder checks
    /// this once per read rather than per row.
    pub fn addresses(&self, corpus: &Corpus) -> bool {
        self.rev == corpus.revision()
    }

    /// One row, resolved against the corpus the order was built from.
    /// `None` both for an index past the end and for the wrong corpus
    /// entirely, since a stale index that happens to be in range is
    /// the worse of the two and must not read as a row.
    pub fn row<'a>(&self, corpus: &'a Corpus, at: usize) -> Option<&'a SearchItem> {
        if !self.addresses(corpus) {
            return None;
        }
        corpus.item_at(*self.order.get(at)?)
    }

    /// Where a section header lands, without materializing a row.
    ///
    /// The draw path's one real need for the whole list was this, and
    /// it only ever depended on the *categories*: cloning a row to
    /// ask `item.category()` is precisely the cost the index list
    /// exists to avoid. Empty for a corpus this ranking does not
    /// address, matching `rows`.
    pub fn group_boundaries(&self, corpus: &Corpus) -> Vec<(usize, Category)> {
        if !self.addresses(corpus) {
            return Vec::new();
        }
        boundaries_of(
            self.order
                .iter()
                .filter_map(|&at| corpus.item_at(at))
                .map(|item| item.category()),
        )
    }

    /// Every row in order, or nothing at all when the corpus has moved
    /// on; a partial list would read as "the search found less".
    pub fn rows<'a>(&self, corpus: &'a Corpus) -> impl Iterator<Item = &'a SearchItem> + use<'a> {
        let order = if self.addresses(corpus) {
            self.order.clone()
        } else {
            Vec::new()
        };
        order.into_iter().filter_map(move |at| corpus.item_at(at))
    }
}

/// The merged corpus to search over, built once per library scan /
/// Navidrome sync and re-searched on every keystroke.
#[derive(Debug, Default)]
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
    /// Which corpus state this is, globally unique.
    ///
    /// A ranking is a list of indices into `items` (see
    /// `rank_indices`), so it is only meaningful against the state it
    /// was computed from: a later `drop_local` shifts every index past
    /// the first removal, which turns a held ranking from stale into
    /// silently *wrong* rows.
    ///
    /// Drawn from a process-global counter rather than counted per
    /// instance, because a per-instance count is not an identity: the
    /// onboarding flow replaces `app.corpus` wholesale, and a fresh
    /// corpus counting its own insertions reaches the same number as
    /// the old one did whenever it happens to index as many items, at
    /// which point a held order compares equal against a different
    /// item list. `Clone` takes a new one for the same reason.
    rev: u64,
}

/// Hands out `Corpus::rev`. Monotonic and never reused, so two corpus
/// states can never share a revision; `Relaxed` is enough because the
/// values are only ever compared for equality, never ordered.
fn next_revision() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(1);
    NEXT.fetch_add(1, Ordering::Relaxed)
}

/// Hand-written so the clone takes a *new* revision: a derived one
/// copies it, and the two corpora then diverge while still claiming
/// to be the same state.
impl Clone for Corpus {
    fn clone(&self) -> Self {
        Corpus {
            items: self.items.clone(),
            remote_keys: self.remote_keys.clone(),
            scratches: RefCell::new(Vec::new()),
            rev: next_revision(),
        }
    }
}

impl Corpus {
    pub fn new() -> Self {
        Corpus {
            rev: next_revision(),
            ..Default::default()
        }
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
    ///
    /// Call it through `spawn_warm_from_cache` rather than directly:
    /// tens of thousands of rows is tens of milliseconds, which the
    /// first frame shouldn't wait for when there is an mpv process
    /// being spawned anyway.
    fn warm_from_cache(servers: &[NavidromeConfig]) -> Corpus {
        let mut corpus = Corpus::new();
        for nav in servers {
            let Some((catalog, album_artists)) = cache::load_catalog(nav) else {
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
            // the persisted album-to-artist links, so a cold start
            // joins on ids before the first sync lands; see
            // `subsonic::IdIndex`.
            for (album_id, artist_id) in &album_artists {
                client.remember_album_artist(album_id, artist_id);
            }
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
    ///
    /// **Caller contract:** an empty `catalog` is read as the server
    /// claiming it has nothing, and empties it here. Never pass one
    /// built by defaulting a failed request to empty; that turns a
    /// network blip into deleting a server's library. See
    /// `fetch_navidrome_catalog`, which propagates every fetch error
    /// for exactly this reason.
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
            self.rev = next_revision();
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
        debug_assert!(
            u32::try_from(self.items.len()).is_ok(),
            "a ranking addresses items with a u32"
        );
        self.items.push(Entry::new(item));
        self.rev = next_revision();
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
        self.rev = next_revision();
    }

    /// Every indexed item, in insertion order. Read-only whole-corpus
    /// access for things that aren't a keyword search at all; the
    /// suggestion engine builds its catalog off this (see
    /// `suggest::Catalog::ingest_corpus`).
    pub fn items(&self) -> impl Iterator<Item = &SearchItem> {
        self.items.iter().map(|e| &e.item)
    }

    /// Which corpus state this is; see the field's own note. Only ever
    /// compared for equality, never for order.
    pub fn revision(&self) -> u64 {
        self.rev
    }

    /// One indexed item by its corpus index, as `rank_indices` hands
    /// them out. `None` for an index from a different revision that no
    /// longer exists, which is the honest answer; a stale index that
    /// still happens to be in range reads as whatever moved into that
    /// slot, which is why holders check the revision instead of
    /// relying on this.
    pub fn item_at(&self, at: u32) -> Option<&SearchItem> {
        self.items.get(at as usize).map(|e| &e.item)
    }

    /// Every distinct navidrome server already represented in the
    /// corpus (deduped by base url, first-seen order), for firing a
    /// live `search3` request against; a server only shows up here
    /// once something from it has actually been indexed
    /// (artists/albums/playlists via `fetch_navidrome_catalog`), which
    /// is also exactly when it's safe to assume the connection is
    /// good.
    ///
    /// Still a full walk, deliberately. It is a pure derivation of the
    /// corpus, so there is nothing to invalidate and nothing that can
    /// go stale; a memo would need a hook on every removal path
    /// (`reconcile_server`, `drop_local`) and a missed one means
    /// naming a server that no longer has anything indexed, which is
    /// the contract above broken rather than a slow frame. What was
    /// actually costing anything was the `String` per *item* the old
    /// dedup allocated; the inner scan is over servers, of which there
    /// are one or two, so it is a couple of comparisons per item and
    /// no allocation at all. See `bench_remote_clients`, and
    /// `the_render_layer_never_asks_which_servers_are_live` for the
    /// guard that keeps this off the frame path.
    pub fn remote_clients(&self) -> Vec<Arc<Client>> {
        let mut clients: Vec<Arc<Client>> = Vec::new();
        for client in self.items.iter().filter_map(|e| e.item.client()) {
            if !clients.iter().any(|c| c.base_url() == client.base_url()) {
                clients.push(client.clone());
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
        let ranking = self.rank(query);
        // indexed, not `item_at`: `rank_order` already resolved every
        // one of these to ask its category, so a `None` here is
        // impossible and a `filter_map` would only hide a shortened
        // list, which is exactly the "search is rank plus the clone"
        // contract broken.
        ranking
            .order
            .iter()
            .map(|&at| self.items[at as usize].item.clone())
            .collect()
    }

    /// `search`'s exact order, as corpus indices plus the revision
    /// they address.
    ///
    /// This is the whole point of the index list: `search` clones a
    /// `SearchItem` per match, which at 40k items and a broad query
    /// was 3-5ms of a ~7ms search, and a caller that only draws a
    /// screenful needs none of those clones. `search` is now this plus
    /// the clone, so the two cannot order differently.
    pub fn rank(&self, query: &str) -> Ranking {
        Ranking {
            rev: self.rev,
            order: rank_order(&self.items, self.score_all(query)),
        }
    }

    /// The primary names of the best matches, deduped and capped at
    /// `cap`. Exactly `search`'s ranking and order, since it scores and
    /// ranks through the same path; it just stops at `cap` names and
    /// clones a name rather than a whole `SearchItem` per match, which
    /// is what shellsong's completion wants on every keystroke.
    pub fn search_names(&self, query: &str, cap: usize) -> Vec<String> {
        let mut names: Vec<String> = Vec::new();
        let mut seen: HashSet<&str> = HashSet::new();
        for item in rank_borrowed(&self.items, self.score_all(query)) {
            let name = item.primary_name();
            if name.is_empty() || !seen.insert(name) {
                continue;
            }
            names.push(name.to_string());
            if names.len() >= cap {
                break;
            }
        }
        names
    }

    /// Phase one of `search`: every item that either signal fired on,
    /// with its score, in corpus order. Split out from the ranking so
    /// the two can be benchmarked separately.
    fn score_all(&self, query: &str) -> Vec<Scored> {
        self.score_all_maybe_parallel(query, self.items.len() >= PARALLEL_SCORING_FLOOR)
    }

    /// `score_all` with the fan-out decision forced, so a benchmark can
    /// measure the per-item cost single-threaded (immune to whatever
    /// else is loading the machine) as well as end to end.
    fn score_all_maybe_parallel(&self, query: &str, parallel: bool) -> Vec<Scored> {
        if query.trim().is_empty() {
            return Vec::new();
        }
        let needle = Needle::new(query);
        // "!foo" on its own leaves nothing to match on, so it ranks
        // nothing rather than ranking the whole corpus minus foo.
        if needle.lower.trim().is_empty() {
            return Vec::new();
        }
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
            let parts: Vec<Vec<Scored>> = thread::scope(|scope| {
                self.items
                    .chunks(chunk)
                    .zip(scratches.iter_mut())
                    .enumerate()
                    .map(|(nth, (part, scratch))| {
                        // each chunk scores its own slice but reports
                        // corpus indices, so the concat below is in
                        // corpus order with no fixup pass.
                        let base = (nth * chunk) as u32;
                        let needle = &needle;
                        scope.spawn(move || score_chunk(part, base, needle, scratch))
                    })
                    .collect::<Vec<_>>()
                    .into_iter()
                    .map(|handle| handle.join().unwrap_or_default())
                    .collect()
            });
            parts.concat()
        } else {
            score_chunk(&self.items, 0, &needle, &mut scratches[0])
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
/// One candidate: what it scored, and where it is in the corpus.
///
/// Two named fields rather than `(u32, u32)`, which the reference
/// version could not get wrong (`&SearchItem` is not a score) and the
/// index version silently can: `sort_by_key`, `max`, the cutoff and
/// the category lookup all read one or the other, and a swap compiles.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Scored {
    score: u32,
    at: u32,
}

/// The ranking proper, as corpus indices. `scored` carries indices
/// rather than references so the result can outlive the borrow and be
/// held across an event loop; `Corpus::rank_indices` is the caller.
fn rank_order(items: &[Entry], mut scored: Vec<Scored>) -> Vec<u32> {
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
    let Some(top_score) = scored.iter().map(|c| c.score).max() else {
        return Vec::new();
    };
    let category = |at: u32| items[at as usize].item.category();
    let threshold = (top_score as f64 * WEAK_MATCH_CUTOFF_FRACTION).round() as u32;
    scored.retain(|c| c.score >= threshold);
    scored.sort_by_key(|c| std::cmp::Reverse(c.score));

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
    for c in &scored {
        let cat = category(c.at);
        if !group_order.contains(&cat) {
            group_order.push(cat);
        }
    }
    let mut out = Vec::with_capacity(scored.len());
    for cat in group_order {
        out.extend(
            scored
                .iter()
                .filter(|c| category(c.at) == cat)
                .map(|c| c.at),
        );
    }
    out
}

/// `rank_order` resolved back to references, for the one caller that
/// wants every match's name in order and nothing else (shellsong's
/// completion). Goes through `rank_order` so it cannot order
/// differently.
fn rank_borrowed<'a>(items: &'a [Entry], scored: Vec<Scored>) -> Vec<&'a SearchItem> {
    rank_order(items, scored)
        .into_iter()
        .map(|at| &items[at as usize].item)
        .collect()
}

/// Scores one slice of the corpus against `pattern`, reusing a single
/// `Scratch` for every item; the whole per-item hot path lives here so
/// the threaded and single-threaded paths can't score differently.
/// Keeps corpus order, which the caller's stable sort relies on.
fn score_chunk(
    entries: &[Entry],
    base: u32,
    needle: &Needle,
    scratch: &mut Scratch,
) -> Vec<Scored> {
    let Scratch { matcher, buf } = scratch;
    let mut scored = Vec::new();
    for (nth, entry) in entries.iter().enumerate() {
        // checked before scoring: an excluded item cannot be rescued by
        // any score, so this is the cheapest possible rejection.
        if needle.excludes(&entry.haystack_lower) {
            continue;
        }
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
            (a, b) => scored.push(Scored {
                score: a.unwrap_or(0).max(b.unwrap_or(0)),
                at: base + nth as u32,
            }),
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

    /// One entry per server however many of its rows are indexed, in
    /// first-seen order, and local files name no server at all.
    #[test]
    fn remote_clients_is_one_entry_per_server_in_first_seen_order() {
        let mut c = Corpus::new();
        c.add_local_tracks([LocalTrack {
            path: PathBuf::from("/m/a.flac"),
            title: "Local".into(),
            artist: None,
            album: None,
            duration_secs: None,
            kind: MediaKind::Music,
        }]);
        assert!(
            c.remote_clients().is_empty(),
            "a local file is not a server"
        );

        c.reconcile_server(catalog(&["a1", "a2"], &["al1"], &["p1"]), &other_client());
        c.reconcile_server(catalog(&["b1"], &["bl1"], &[]), &test_client());
        let clients = c.remote_clients();
        let urls: Vec<&str> = clients.iter().map(|c| c.base_url()).collect();
        assert_eq!(
            urls,
            vec!["https://other.invalid", "https://example.invalid"],
            "five rows from the first server is still one entry, and the \
             order is the order they were indexed in"
        );
    }

    /// The scan is O(corpus) and uncached on purpose (see
    /// `remote_clients`), which is only safe while nothing on the
    /// render path asks. The render layer is `src/ui`; a source scan
    /// rather than a timing assertion, because the thing to prevent is
    /// a new call site, not a slow machine.
    #[test]
    fn the_render_layer_never_asks_which_servers_are_live() {
        let ui = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/ui");
        let mut offenders = Vec::new();
        for entry in std::fs::read_dir(&ui).expect("src/ui must exist").flatten() {
            let path = entry.path();
            if path.extension().is_none_or(|e| e != "rs") {
                continue;
            }
            let Ok(source) = std::fs::read_to_string(&path) else {
                continue;
            };
            if source.contains("remote_clients") {
                offenders.push(path.file_name().unwrap().to_string_lossy().into_owned());
            }
        }
        assert!(
            offenders.is_empty(),
            "a whole-corpus walk reached the render path: {offenders:?}. \
             it answers which servers are live, which a widget never needs \
             to recompute per frame; resolve it where the event is handled."
        );
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

    /// `rank_order` resolved to owned items, which is exactly what
    /// `Corpus::search` does. The tests below predate the index list
    /// and compare whole items; going through `rank_order` keeps them
    /// testing the real ranking rather than a second copy of it.
    fn ranked_items(c: &Corpus, scored: Vec<Scored>) -> Vec<SearchItem> {
        rank_order(&c.items, scored)
            .into_iter()
            .map(|at| c.items[at as usize].item.clone())
            .collect()
    }

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
        // was (307, 0xf8a9_24ad_cde1_23c5), byte-identical to
        // `'blade`'s row above, which is what "nucleo ignores
        // negation" looked like from here. now 0: the exclusion is
        // honoured, and `!blade` alone leaves nothing positive to rank.
        ("!blade", 0, 0xcbf2_9ce4_8422_2325),
        ("blade|runner", 0, 0xcbf2_9ce4_8422_2325),
        // was (0, 0xcbf2_9ce4_8422_2325): an accented query found
        // *nothing*. now byte-identical to `blade`'s row above,
        // because the query and every haystack are folded before
        // matching. nucleo normalizes the haystack but not the
        // pattern, and not Latin Extended-B either way; see
        // `fold_diacritics`. the only row this changed, of 38.
        ("bl\u{e0}d\u{e9}", 447, 0x162c_be8f_180a_1ccd),
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
            let fast = score_chunk(&c.items, 0, &needle, &mut scratch);
            // the reference gets the same exclusion the hot path
            // applies: `!term` is a filter over the corpus, not part of
            // scoring, so excluding it here keeps this an apples-to-
            // apples test of the *scorer*. that exclusion is correct is
            // what `an_excluded_term_drops_the_matches_that_contain_it`
            // and its neighbours are for.
            let expected: Vec<(u32, &SearchItem)> = c
                .items
                .iter()
                .filter(|e| !needle.excludes(&e.haystack_lower))
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
            for (i, (got, (es, ei))) in fast.iter().zip(expected.iter()).enumerate() {
                let fi = &c.items[got.at as usize].item;
                assert_eq!(
                    (&got.score, fi.identity()),
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
            let par = ranked_items(&c, c.score_all_maybe_parallel(query, true));
            let seq = ranked_items(&c, c.score_all_maybe_parallel(query, false));
            assert_eq!(
                fingerprint(&par),
                fingerprint(&seq),
                "{query:?}: threaded scoring changed the result list"
            );
        }
    }

    /// A revision is an identity, not a count of mutations.
    ///
    /// This is the hole a per-instance counter had, and it is not
    /// hypothetical: the onboarding flow replaces `app.corpus`
    /// wholesale, so a fresh corpus that indexes as many items as the
    /// old one reached the same number, and a held order then compared
    /// equal against a different item list. Two corpora built the same
    /// way must still be distinguishable.
    #[test]
    fn two_corpus_states_never_share_a_revision() {
        let track = |n: usize| LocalTrack {
            path: PathBuf::from(format!("/music/{n}.flac")),
            title: format!("track {n}"),
            artist: None,
            album: None,
            duration_secs: None,
            kind: MediaKind::Music,
        };
        let build = || {
            let mut c = Corpus::new();
            c.add_local_tracks([track(1), track(2)]);
            c
        };
        let (a, b) = (build(), build());
        assert_ne!(
            a.revision(),
            b.revision(),
            "same contents, same mutation count, different corpus"
        );
        // and a clone is a third state, not a second claim on the
        // first: the derived Clone copied the revision, and the two
        // then diverged while still comparing equal.
        let mut cloned = a.clone();
        assert_ne!(a.revision(), cloned.revision(), "a clone takes a new one");
        cloned.add_local_tracks([track(3)]);
        assert_ne!(a.revision(), cloned.revision());
    }

    /// A ranking read against the wrong corpus reads as no rows at
    /// all, never as rows.
    ///
    /// The whole hazard of an index list: a stale index that happens
    /// to still be in range resolves to whatever moved into that slot,
    /// which is a real-looking wrong row. `Ranking` carries the
    /// revision so this is checkable at all, and refuses rather than
    /// serving a partial list.
    #[test]
    fn a_ranking_refuses_the_corpus_it_was_not_built_from() {
        let mut c = varied_corpus(200);
        let ranking = c.rank("a");
        assert!(ranking.len() > 1, "need rows for this to mean anything");
        assert!(ranking.addresses(&c));
        assert!(ranking.row(&c, 0).is_some());

        // the mutation that makes an old index name a different row:
        // dropping the local items shifts everything after the first
        // removal.
        c.drop_local();
        assert!(!ranking.addresses(&c), "the corpus moved on");
        assert!(ranking.row(&c, 0).is_none(), "no row from the wrong state");
        assert_eq!(ranking.rows(&c).count(), 0, "and not a partial list");
        assert_eq!(ranking.len(), ranking.len(), "the order itself is intact");
    }

    /// The headers a ranking reports must land exactly where the
    /// list-of-items version puts them, since main.rs's sidebar
    /// divider draws its junction marks off one and the results list
    /// draws the headers off the other; a one-row disagreement is a
    /// junction mark against a blank row.
    #[test]
    fn ranked_group_boundaries_match_the_materialized_ones() {
        let c = varied_corpus(2_000);
        for query in GUARD_QUERIES {
            let ranking = c.rank(query);
            let items = c.search(query);
            assert_eq!(
                ranking.group_boundaries(&c),
                group_boundaries(&items),
                "{query:?}: headers land on different rows"
            );
        }
    }

    /// The per-chunk base arithmetic, stated directly.
    ///
    /// `threaded_and_sequential_scoring_agree` above also catches a
    /// shifted base (checked: zeroing the base fails both), but it
    /// does so second-hand, by noticing the resolved rows differ.
    /// This compares the index lists themselves, so a failure says
    /// "a chunk reported the wrong base" instead of "the result list
    /// changed", and it still holds if the rows a shifted base lands
    /// on ever happen to fingerprint the same.
    #[test]
    fn threaded_scoring_reports_the_same_corpus_indices() {
        let c = varied_corpus(5_000);
        // on a one-core box there is a single chunk at base 0, which
        // is correct by construction, so this would pass while
        // proving nothing. skip loudly rather than silently.
        if thread_count() < 2 {
            println!("one scoring thread here: no second chunk to check a base against");
            return;
        }
        for query in GUARD_QUERIES {
            let par = c.score_all_maybe_parallel(query, true);
            let seq = c.score_all_maybe_parallel(query, false);
            assert_eq!(par, seq, "{query:?}: a chunk reported the wrong base");
        }
    }

    /// Every index a ranking hands out resolves. `rank_order` and
    /// `rank_borrowed` index `items` directly, so an index that is out
    /// of range is a panic on the keystroke that produced it, not a
    /// missing row.
    #[test]
    fn every_ranked_index_resolves_to_an_item() {
        let c = varied_corpus(3_000);
        for query in GUARD_QUERIES {
            let ranking = c.rank(query);
            for at in 0..ranking.len() {
                assert!(
                    ranking.row(&c, at).is_some(),
                    "{query:?}: row {at} of {} does not resolve",
                    ranking.len()
                );
            }
        }
    }

    /// `search` is `rank_indices` plus a clone, and that is the only
    /// reason the index list is safe to introduce: a caller switching
    /// from one to the other must see the same rows in the same order.
    #[test]
    fn rank_indices_orders_exactly_like_search() {
        let c = varied_corpus(3_000);
        for query in GUARD_QUERIES {
            let cloned = c.search(query);
            let via_index: Vec<SearchItem> = c.rank(query).rows(&c).cloned().collect();
            assert_eq!(
                fingerprint(&cloned),
                fingerprint(&via_index),
                "{query:?}: the index list and search disagree"
            );
            assert_eq!(
                cloned.len(),
                via_index.len(),
                "{query:?}: different lengths"
            );
        }
    }

    /// A ranking is indices into one revision of the corpus, so every
    /// change to `items` has to bump it; a missed bump lets a holder
    /// keep an order whose indices now name different rows, which
    /// plays the wrong song rather than failing.
    #[test]
    fn every_change_to_the_corpus_changes_the_revision() {
        let mut c = Corpus::new();
        let empty = c.revision();
        c.add_local_tracks([LocalTrack {
            path: PathBuf::from("/music/a.flac"),
            title: "a".into(),
            artist: None,
            album: None,
            duration_secs: None,
            kind: MediaKind::Music,
        }]);
        let after_add = c.revision();
        assert_ne!(empty, after_add, "an insertion must bump");

        c.drop_local();
        assert_ne!(after_add, c.revision(), "a removal must bump");
        assert!(c.items.is_empty(), "the local track really did go");
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
                let (cpu, out) = bench_cpu(8, || {
                    ranked_items(&c, c.score_all_maybe_parallel(query, false)).len()
                });
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
                    let ranked = ranked_items(&c, scored);
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

    /// What the uncached whole-corpus walk in `remote_clients` costs,
    /// since the argument for leaving it uncached rests on the number.
    /// Run with `cargo test --release -- --ignored --nocapture
    /// bench_remote_clients`.
    #[test]
    #[ignore = "timing benchmark, run manually"]
    fn bench_remote_clients() {
        for size in [40_000usize, 150_000] {
            let c = varied_corpus(size);
            let (per, clients) = bench(200, || c.remote_clients().len());
            println!(
                "{:>7} items  remote_clients {per:>10.3?}  ({clients} servers)",
                c.items.len()
            );
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
                    .map(|cand| {
                        let (t, k) = c.items[cand.at as usize].item.identity();
                        (t, k.into_owned())
                    })
                    .collect();
                let mut escaped = Vec::new();
                for cand in c.score_all(&long) {
                    let item = &c.items[cand.at as usize].item;
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

    /// A corpus where one item carries a word the other does not, for
    /// the exclusion tests.
    fn exclusion_corpus() -> Corpus {
        let mut c = Corpus::new();
        c.add_local_tracks([
            LocalTrack {
                path: PathBuf::from("/m/a.flac"),
                title: "Blade Runner Blues".into(),
                artist: Some("Vangelis".into()),
                album: Some("Blade Runner".into()),
                duration_secs: None,
                kind: MediaKind::Music,
            },
            LocalTrack {
                path: PathBuf::from("/m/b.flac"),
                title: "Blade Runner Blues (Live)".into(),
                artist: Some("Vangelis".into()),
                album: Some("Blade Runner Live".into()),
                duration_secs: None,
                kind: MediaKind::Music,
            },
        ]);
        c
    }

    fn titles(items: &[SearchItem]) -> Vec<String> {
        items.iter().map(|i| i.primary_name().to_string()).collect()
    }

    /// The bug: nucleo parses `!foo` but `Pattern::score` ignores it, so
    /// this used to return both rows.
    #[test]
    fn an_excluded_term_drops_the_matches_that_contain_it() {
        let c = exclusion_corpus();
        let got = titles(&c.search("blade runner !live"));
        assert!(
            got.iter().any(|t| t == "Blade Runner Blues"),
            "the positive match should survive: {got:?}"
        );
        assert!(
            !got.iter().any(|t| t.contains("Live")),
            "the excluded row is still here: {got:?}"
        );
    }

    #[test]
    fn exclusion_ignores_case() {
        let c = exclusion_corpus();
        for q in ["blade runner !LIVE", "blade runner !Live"] {
            let got = titles(&c.search(q));
            assert!(!got.iter().any(|t| t.contains("Live")), "{q}: {got:?}");
            assert!(!got.is_empty(), "{q} dropped everything: {got:?}");
        }
    }

    /// Excluding something absent must not disturb the ranking, which
    /// is what says the positive half still goes through the normal path.
    #[test]
    fn excluding_an_absent_term_leaves_the_ranking_alone() {
        let c = exclusion_corpus();
        assert_eq!(
            titles(&c.search("blade runner !zzzznothing")),
            titles(&c.search("blade runner")),
        );
    }

    #[test]
    fn an_exclusion_only_query_ranks_nothing() {
        let c = exclusion_corpus();
        assert!(
            c.search("!live").is_empty(),
            "there is nothing positive to rank against"
        );
        assert!(c.search("  !live   ").is_empty());
    }

    /// A bare `!` has no term after it, so it is the literal the user
    /// typed rather than an exclusion of everything.
    #[test]
    fn a_bare_bang_is_not_an_exclusion() {
        let (positive, excluded) = split_exclusions("blade !");
        assert_eq!(positive, "blade !");
        assert!(excluded.is_empty());
    }

    #[test]
    fn several_exclusions_all_apply() {
        let (positive, excluded) = split_exclusions("blade !live !remix");
        assert_eq!(positive, "blade");
        assert_eq!(excluded, vec!["live".to_string(), "remix".to_string()]);
    }

    /// `search_names` reads the same ranking, so it has to honour the
    /// exclusion too; it would be its own bug if only `search` did.
    #[test]
    fn search_names_honours_exclusions() {
        let c = exclusion_corpus();
        let names = c.search_names("blade runner !live", 10);
        assert!(!names.is_empty(), "{names:?}");
        assert!(!names.iter().any(|n| n.contains("Live")), "{names:?}");
    }
}

#[cfg(test)]
mod diacritics {
    use super::*;

    fn one(title: &str, artist: &str, album: &str) -> Corpus {
        let mut c = Corpus::new();
        c.add_local_tracks([LocalTrack {
            path: PathBuf::from("/music/x.flac"),
            title: title.into(),
            artist: Some(artist.into()),
            album: Some(album.into()),
            duration_secs: None,
            kind: MediaKind::Music,
        }]);
        c
    }

    /// The reason `fold_diacritics` exists, pinned against nucleo
    /// itself so an upstream fix is visible rather than silently
    /// making the fold redundant.
    ///
    /// nucleo normalizes Latin-1 and Latin Extended-A, so
    /// `imparat`/`Împărat` scores. It does **not** reach Latin
    /// Extended-B, where Romanian's `ș` and `ț` live, so those score
    /// `None`. If these start returning `Some`, the fold can shrink.
    #[test]
    fn nucleo_alone_cannot_match_romanian_comma_below() {
        let mut m = Matcher::default();
        let mut buf = Vec::new();
        let score = |q: &str, hay: &str, m: &mut Matcher, buf: &mut Vec<char>| {
            Pattern::parse(q, CaseMatching::Smart, Normalization::Smart)
                .score(Utf32Str::new(hay, buf), m)
        };
        for (q, hay) in [("si", "Și"), ("tara", "Țara"), ("taranu", "Țăranu")] {
            assert!(
                score(q, hay, &mut m, &mut buf).is_none(),
                "nucleo now folds {hay:?}; the table can lose those letters"
            );
        }
        // and the half it does handle, for contrast.
        assert!(score("imparat", "Împărat", &mut m, &mut buf).is_some());
    }

    /// What the maintainer asked for: an ascii letter matches its
    /// accented form fully, not just as a one-char typo.
    #[test]
    fn ascii_finds_accented_text() {
        let c = one("Și Dacă Țara", "Țăranu", "Împărat");
        for q in [
            "si daca tara",
            "si",
            "tara",
            "daca",
            // two diacritics in one word, which the edit-distance
            // fallback could never absorb: its budget is one.
            "taranu",
            "imparat",
        ] {
            assert_eq!(c.search(q).len(), 1, "{q:?} found nothing");
        }
    }

    /// And the other direction: typing the diacritic still works.
    #[test]
    fn accented_queries_still_match() {
        let c = one("Și Dacă Țara", "Țăranu", "Împărat");
        assert_eq!(c.search("și").len(), 1);
        assert_eq!(c.search("țara").len(), 1);
    }

    #[test]
    fn folding_keeps_case_and_length() {
        assert_eq!(fold_diacritics("Șș Țț Ăă Îî Ââ"), "Ss Tt Aa Ii Aa");
        // length is load-bearing: both prefilters key off word
        // lengths and char counts.
        let folded = fold_diacritics("Țăranu");
        assert_eq!(folded.chars().count(), "Țăranu".chars().count());
        assert_eq!(folded, "Taranu");
    }

    /// Ascii in, ascii out, with no allocation; the overwhelming
    /// majority of queries and haystacks take this path.
    #[test]
    fn ascii_is_borrowed_not_rebuilt() {
        assert!(matches!(
            fold_diacritics("plain ascii"),
            std::borrow::Cow::Borrowed(_)
        ));
    }

    /// A European sweep, so the table is not quietly Romanian-only.
    #[test]
    fn the_table_covers_more_than_romanian() {
        for (q, title) in [
            ("bjork", "Björk"),
            ("sigur ros", "Sigur Rós"),
            ("motley crue", "Mötley Crüe"),
            ("beyonce", "Beyoncé"),
            ("zazdrosc", "Zazdrość"),
            ("dvorak", "Dvořák"),
        ] {
            let c = one(title, "x", "y");
            assert_eq!(c.search(q).len(), 1, "{q:?} did not find {title:?}");
        }
    }
}
