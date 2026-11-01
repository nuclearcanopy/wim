# testing conventions

Written because the same two mistakes kept being found by audit rather
than by the suite, three times each. Both are about tests that look
like coverage and are not.

## a test module is named for what losing it would stop covering

The failure mode is accretion: a module named for the first thing
tested in it collects whatever came next, and the name then lies.

Found three times. `catalog_fetch_tests` held three fetch tests plus
four client-lifetime and zeroize tests. `shellsong_wiring_tests` held
`is_starred` and `album_page` tests, which are not shellsong wiring.
`download_policy_tests` held a serde-alias test about `Config`'s
on-disk shape.

Why it matters beyond tidiness: the name is what a reader uses to
decide whether an area is covered. An `is_starred` test filed under
shellsong wiring reads as "shellsong is well tested" and as
"`is_starred` is untested", and both readings are wrong.

The rule is not "one module per function". It is that the module's
name has to answer "what would stop being covered if this were
deleted". `fetch_outcome_tests` and `kept_client_tests` both pass
that; `catalog_fetch_tests` holding both did not.

There is no automated check, and a fuzzy one would be worse than none:
deciding whether a test belongs is a judgement about subject, which is
exactly what a lint cannot make. It is a review habit, and this file is
where it is written down so it survives the next session.

## a test has to be able to fail

Four found in this repo's own history, and then two written by the loop
that fixed those, which is the real argument for the discipline below.

The ones found by audit:

- `assert!(candidates.len() <= MAX_CANDIDATES)` against a one-item
  corpus and a cap of 50.
- `assert!(artist.is_none())` on a value hardcoded `None` two lines
  above, with the one computed field left unasserted.
- a connection-count *ceiling* with no floor, which passed when the
  workers were deleted entirely.
- a 300ms literal standing in for `REMOTE_SEARCH_DEBOUNCE`, coupling
  neither constant.

The ones written while fixing them:

- a message test for `view(suggestions)`' refusal that left the
  refusal's *call site* unpinned, so deleting the `total == 0` check
  stayed green.
- a source guard for the eq-persist loop that scanned the whole file
  for a literal the test itself contained, so it could never fail.

So: **mutate the code a test guards and watch that test fail.** Not
when the fix feels risky, every time. Both of the loop-written cases
above were caught that way and by nothing else.

Two corollaries worth stating, since both were learned the hard way:

- A test of a pure helper does not cover the wiring that calls it.
  This repo records that pattern separately ("pinned the function, not
  the wiring"); `subsonic.rs`'s three tests of a dead function are its
  worst instance.
- If a test asserts "nothing happened", it needs a positive control
  that something *can* happen in the same run, or "nothing happened"
  cannot be told from "nothing had happened yet". The undo resolver
  test fires a control request first for exactly this reason.

## reading the source is a legitimate guard, within limits

Several hooks here cannot be reached without a live mpv and the whole
host loop: the pause keybind, the skip guard, `apply_op`'s undo hook,
`Snapshot::capture` making no ipc call. For those, a test that reads
`include_str!` and asserts the call site's shape is better than no
coverage, and it catches the one thing behavioural tests elsewhere
cannot: a *bypass*.

Two rules make them honest:

- Anchor inside real code, not test code. Check that the anchor's
  first occurrence in the file precedes the first `#[cfg(test)]`, and
  slice to the enclosing function rather than searching the whole
  file. The eq-persist guard broke this and could not fail.
- Assert the shape that matters, not merely that a name appears. "It
  computes the live curve" passed while the comparison was deleted;
  "it compares *and* saves" did not.

## a timing test needs the machine to itself

`cargo test -- --ignored` runs every benchmark at once across the
thread pool, and they inflate each other. A console frame measured
51-62us that way and 11.5-13.9us with `--test-threads=1`: a **5x**
error, in the direction that invents a bottleneck.

So: run a timing bench with `--test-threads=1`, or alone by name. And
the standing rule it supports, which this repo has relearned twice:
**alternate batches of the two implementations in one process and take
the best of each.** Sequential runs on this box differ 1.6-2.1x under
load, so two numbers from two runs are not a comparison. Both sides of
the pair must also be ordinary functions; an "old" implementation
written as a branch inside the timing loop gets inlined and
specialized, which once read as 19% slower and became 7% faster when
both sides became functions.

## mpv-gated tests

`CoreCtx` holds a live mpv handle, so behavioural tests through it are
`#[ignore]`d by convention. Two things follow.

Run them explicitly (`cargo test -- --ignored`) rather than trusting
them, because a default `cargo test` verifies none of them. The undo
seam was once covered *only* this way, so CI checked none of its
decisions; those are pure and are now tested outright, leaving only the
snapshot to the gated half.

And they must not reach real user state. An `--ignored` run once
overwrote a real `~/.cache/wim/index.ndjson` with a fixture, orphaning
already-downloaded tracks, because `note_playback_started` resolved
`cache::cache_dir()` itself. `cache_dir` now refuses a non-temp path
under `cfg(test)`, but the fix that matters is injection:
`DownloadCtx::persist` and `toggle_star`'s `persist` are function
pointers, so a caller cannot write anywhere it did not name. Prefer
that over a guard.
