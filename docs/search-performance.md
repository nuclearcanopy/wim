# search performance

the design record for the hot path in `src/search.rs`. read this before
adding a short circuit, a cache, or a cap to `Corpus::search`; the
comments in the code point back here rather than repeating the
reasoning inline.

## what the budget is

`Corpus::search` runs on every keystroke, from the input thread,
against the whole merged corpus (local files plus every navidrome
entity from every configured server). a 40k-item library is the
assumed size and a 150k one is the stress size. the only thing that
matters is that typing one more character feels like nothing happened,
so the target is single-digit milliseconds per keystroke at 40k.

## results may not change

search ranking is a product decision with its own regression tests
(the "crest" noise bug, the weak-match cutoff, the typo fallback). no
speedup is allowed to change what comes back or the order it comes
back in. three guards hold that line, all in `search.rs`'s test module:

- `scorers_agree_on_every_query_shape` keeps a deliberately naive
  copy of the pre-optimization scorer and compares the real hot path
  against it item by item, score by score, over a 4k mixed corpus plus
  items built to break the prefilters (accented text, unicode
  whitespace, a word past the 31-char length bucket). this is the
  oracle: every short circuit has to be *exact*, not approximate.
- `search_results_are_unchanged` pins `(count, fnv-1a of the result
  identities in order)` per query shape. re-blessing the table is a
  deliberate act.
- `threaded_and_sequential_scoring_agree` checks the fan-out changes
  nothing, which is why chunks are rejoined in corpus order before the
  stable sort rather than as they finish.

every short circuit listed below was also checked by deliberately
breaking it and confirming a guard fails. two gaps found that way are
worth remembering, because an end-to-end test cannot see either: a
budget-1 adjacent transposition (nothing in the corpus had one), and
the top edge of the word-length window, where a word longer than the
query by the whole budget is only reachable by insertions, which makes
the query a subsequence of it, so the fuzzy pass hides a broken window
from any membership check. both are now covered white-box
(`distance_at_most_one_matches_strsim_exactly`,
`length_window_covers_exactly_the_reachable_word_lengths`).

## measure in cpu time

wall clock on a developer machine varies 2-3x between identical bench
runs, which is more than most of the wins being chased. `bench_search`
reports threaded wall time (what the user feels) and `bench_cpu_search`
reports single-threaded cpu time with the fan-out forced off (what
actually changed). the second is the number to compare across commits.
all benches are `#[ignore]`d; run them with
`cargo test --release -- --ignored --nocapture bench_`.

## the two signals, and why one of them is a trap

an item matches if *either* nucleo's subsequence fuzzy score fires or
a per-word damerau-levenshtein distance comes in under
`edit_distance_budget`. the fuzzy half is monotone under appending to
the query: a fuzzy atom needs its needle to be a subsequence of the
haystack, so if `q` is not, `q + c` is not either, and the smart-case
and normalization decisions nucleo makes per atom only ever get
stricter as an atom grows.

the typo half is **not** monotone, and this is the thing to not get
wrong. the budget grows with query length, so a longer query can reach
*more* words than a shorter one:

- `brainm` is 3 edits from "branimir" with a budget of 1, so it misses;
  `brainmir` is 1 edit away with a budget of 2, so it hits.
- `ob` is 2 edits from "be"; `obe` is 1.
- `god` is 2 edits from "goes"; `gods` is 1.

`probe_monotonicity` (an `#[ignore]`d diagnostic) finds these
automatically against a 20k corpus, and
`appending_a_char_can_add_candidates` pins the first one as a test.

so the obvious type-ahead optimization, "typing `blade` after `blad`
can only shrink the result set, so narrow the previous one", is
**unsound** here and is deliberately not implemented. narrowing the
previous *post-cutoff* result set would be wrong for a second,
independent reason: the weak-match cutoff is relative to the best
score in that search, so an item cut while a strong match was on the
board can legitimately come back when a longer query drops that strong
match.

narrowing the fuzzy half alone *would* be sound (cache the previous
query's fuzzy-match bitset, only re-score those, keep a full scan for
the typo half). it was measured and not taken: after the short circuits
below, the fuzzy pass is no longer the dominant cost, the first
keystroke of any query gains nothing, and a bug in it loses results
rather than just time.

## the short circuits

each `Entry` caches, alongside its haystacks, a `u64` char mask, a
`u32` word-length mask, and an ascii flag. all three are built once at
insertion and all three are *exact* necessary conditions, so none of
them can reject a real match:

1. **fuzzy char mask.** a subsequence match needs every pattern char
   present in the haystack, so `fuzzy_mask & !entry.mask != 0` means no
   match, without nucleo walking the haystack. off (mask 0) unless the
   query is plain alphanumeric-and-spaces, because nucleo reads
   `^ $ ! ' \ |` as pattern syntax rather than as text to find. off per
   entry (mask `u64::MAX`) for non-ascii haystacks, because nucleo
   matches an ascii pattern against *normalized* text, so "blade" has
   to keep hitting "blàde".
2. **word-length window.** an edit distance is at least the length
   difference, so a haystack with no word within the budget of the
   query's length cannot produce a typo match. lengths from 31 up share
   one bit, which only widens the window.
3. **nearest-length word bounds the typo score.** the smallest length
   difference between the query and any word the haystack has
   lower-bounds the best distance the typo pass could report, and so
   upper-bounds the score it could add; when the fuzzy score already
   clears that, the pass cannot change the answer. this replaced the
   old "skip if the fuzzy score beats a *perfect* typo" test, which
   almost never fired.
4. **char mask again, for the typo pass.** an edit distance is at least
   the number of distinct chars one side has that the other lacks
   (every one of them costs an insertion or a substitution). applied
   once per entry against the whole-haystack mask, then per word in
   both directions.
5. **`distance_at_most_one`.** where the budget is one edit, an O(n)
   byte comparison replaces strsim's dp table and position hashmap.
   exact there, not approximate: a single edit is trivially an edit no
   substring sees twice, so the restricted and unrestricted distances
   agree at 1, and every way to be one edit apart is a substitution, an
   adjacent transposition, an insertion or a deletion. a 2-edit budget
   keeps strsim, where that equivalence fails (classically "ca" to
   "abc": 2 unrestricted, 3 restricted).

ascii haystacks also split on bytes rather than on unicode whitespace
and take `len()` as the char count. that is equivalence, not
approximation: ascii text contains no unicode whitespace to split on,
and the non-ascii path is kept for haystacks that do.

## threading

`PARALLEL_SCORING_FLOOR` is a measured number, not a guess; re-measure
it with `bench_parallel_floor` after any change to the per-item cost,
because it moves. the short circuits above cut that cost enough to move
the floor from 2k to 16k: sequential scoring wins at 4k for every query
shape and at 8k for all but one. a query that matches nothing is now so
cheap that no corpus size pays back the ~0.4ms of spawn/join.

the corpus owns one scoring scratch per thread for its whole life.
nucleo's `Matcher` eagerly allocates ~135kb of dp matrices, so building
one per fan-out thread meant touching ~1mb of fresh memory before
scoring anything, on every keystroke.

## what is left

for a broad query over a large library, the remaining cost is split
between nucleo scoring every item (irreducible without the unsound
narrowing above) and `rank` cloning the retained items into the
returned `Vec<SearchItem>`. at 40k with ~15k retained rows the clone is
~3-5ms of a ~7ms search; each `SearchItem` holds three or four strings
and an `Arc<Client>`.

cutting that means not materializing rows nobody has scrolled to, and
it cannot be done inside `search.rs`: `App::results` is a
`Vec<SearchItem>` that main.rs reads directly for row drawing,
`G`/`select_bottom`, the result counter, shellsong's `Target::Index`
addressing, and `resync_results`'s identity scan. a cap would be
visible at every one of those. doing it properly means `App` owning the
ranked index list and materializing a growing prefix on demand, with
those call sites going through accessors; that is an api change across
files search.rs does not own, so it is a maintainer decision rather
than a search-local optimization.

## the other whole-corpus walk

`Corpus::remote_clients` answers "which servers are live" by walking
every item and deduping by base url. it is called per keypress (the
starred view, the profile manager, `load_queue`), never per frame, and
it is left as a walk on purpose: it is a pure derivation of the
corpus, so there is nothing to invalidate, while a memo would need a
hook on every removal path (`reconcile_server`, `drop_local`) and a
missed one would name a server with nothing indexed, which is the
documented contract broken rather than a slow frame.

the walk was never what it cost, though. deduping through a
`HashSet<String>` allocated one `String` per *item*, while the dedup
is over one or two servers; comparing `&str` against a `Vec` of the
servers found so far allocates nothing. measured by
`search::tests::bench_remote_clients`, release, on a loaded 8-core
machine (so both columns are inflated):

| items | `HashSet<String>` | `Vec` scan |
| --- | --- | --- |
| 40k | 4.9ms | 0.8-1.8ms |
| 150k | 24.3ms | 3.9-4.9ms |

4.9ms per keypress at 40k was not invisible, and 24ms at 150k would
have been a stutter. `the_render_layer_never_asks_which_servers_are_live`
is the guard that keeps the walk off the frame path: a source scan of
`src/ui`, because what has to be prevented is a new call site, not a
slow machine.
