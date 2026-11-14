# open questions

Decisions the build loop took rather than blocking for, plus the few
things that genuinely need the maintainer.

**Status is the point of this file.** It previously listed a video
question as "BLOCKING the video items" and asked for a choice between
three subsystems, when later work had already made the whole premise
moot. A stale entry here reads as a debt you owe, which is worse than
no entry at all. So this is split by status, and anything answered says
what answered it.

---

## needs you

### opensubtitles api key

`src/opensubtitles.rs` is built and wired in (search, download,
sub-add, behind `config.opensubtitles_api_key`, off by default), but
the REST endpoint paths and response shapes are a best-effort guess,
not verified against live docs or a real account: there is no key here
and no network access to check with. Treat the request/response shapes
as **unverified** until someone runs it against a real key.

### decoded cover resolution ceiling

`MAX_DECODED_EDGE = 640`. Covers the quadrant grid with margin (a
400x200 cell pane samples 400x400) and the `ratatui-image` path at a
~1920px-wide terminal. On a 4K display with a very large art panel the
`full-color` path may look slightly soft. 1024 would cost 71MB for 24
cached covers instead of 28MB. Pure taste against memory.

### parallel scoring floor

`PARALLEL_SCORING_FLOOR` = 16_000 items before search fans out across
threads, up from 2_000. Measured on 8 cores under load ~10, where
sequential won at 4k for every query shape and at 8k for all but one;
on an idle machine threads would likely win earlier. Chosen to avoid
paying 0.4ms of spawn and join on small libraries, where the whole
sequential search is ~2ms worst case. `bench_parallel_floor` retunes
it.

---

### your play index was overwritten by a test run, and the repair is yours

While running this repo's `#[ignore]`d mpv-gated tests (which I had
told agents to run rather than trust), `note_playback_started` reached
the real `~/.cache/wim` and replaced the play index with the test
harness's single fixture row. The root cause is fixed: `cache_dir()`
now refuses a non-temp path under `cfg(test)`.

**What survived:** the two downloaded files under `~/.cache/wim/tracks/`
are intact, and their filenames are the song ids, so the
"already cached, do not download again" link is reconstructible.

**What is gone:** `play_count` and `play_days` for everything. That is
what the auto-download threshold counts ("coming back to it on another
day"), so until it rebuilds, nothing will auto-cache.

**Why I did not just fix it.** Writing an index with `play_count: 0`
and invented timestamps is not restoring your data, it is replacing it
with a guess, and the two tracks would then look never-played. I
started to do exactly that and the sandbox stopped me, correctly.

Options, your call: leave it (the history rebuilds as you listen, and
the two files get relinked the next time they are played); or have me
relink just the two `cached_path` entries and leave the counts empty,
which stops them being re-downloaded without inventing a history.

### two shellsong behaviours the audit found and left alone

Both are deliberate-looking and pinned by tests, so changing either is
a design call rather than a fix.

`paste` joins a multi-line paste with no separator, so pasting
`play(a)` and `next()` together gives `play(a)next()`. The doc comment
says the text goes in literally and a test pins exactly that. Inserting
`;` would make a paste do something the user did not type, which is why
it was not done unasked.

`Ty::Count` shares `MAX_REPEAT` between `repeat` and `skip`, so
`skip(300)` is refused with "a count above 256 is refused". The range
is documented as the type's in `docs/shellsong.md`, so this reads as a
shared limit rather than a wrong message; a separate skip ceiling is
yours to name if you want one.

### a faster hasher for the two hot maps, which trades away DoS resistance

`Catalog::by_key` and `CacheIndex`'s entry map are keyed by 32-char
navidrome ids and local paths, and `std`'s SipHash-1-3 is about 4x the
cost of a non-cryptographic hasher on keys that shape. Measured in one
process at release: hashing 10266 ids **1350us to 388us**, `by_key`
probes **2900us to 2120us (1.37x)** and 1145us to 479us (2.39x),
`cache.entry` per track **5255us to 3506us (1.50x)**. `by_key` probes
are now 1086-3028us of a 2201-5294us suggestions stage, i.e. the
largest remaining cost in the menu.

**Not done, and it needs you.** It removes a security property, which
is not mine to remove on a perf argument. The case for it is that
neither map takes a key from a stranger: the ids come from your own
navidrome server and the paths from your own disk, so there is no
adversary to be resistant to. The case against is that this is exactly
the reasoning that is wrong later, when something keys a map on a value
a remote peer chose, and the hasher is already there and already looks
fine.

Both maps are in `cache.rs` and `suggest.rs`, and neither has a
load-bearing iteration order (the ndjson line order is not pinned by
any test). If you want it, the shape is `rustc-hash`'s `FxHasher`
behind a `FastMap` alias, with a doc stating the rule that anything
keyed on remote input keeps the default. A sandbox check stopped me
writing it unprompted, correctly.

---

## defaults taken, recorded so they are not re-litigated

### a completion matched by folding types the ascii, not the diacritics

`Corpus::search_names` folds diacritics, so typing `tar` in the console
offers `Țara`. Accepting it appends the *remainder* (`a`), so the line
becomes `play(tara)` rather than `play(Țara)`.

Left that way on purpose. `Suggestion`'s own doc states the invariant
("Always a pure extension of what was typed, never a replacement") and
a test enforces it, because a completion that rewrites what you already
typed is the thing that invariant exists to prevent. The folded line
also resolves: the resolver folds on the same table, which is why
submitting `play(tara)` always worked even while the completer showed
nothing.

**If you would rather accepting a folded match replace the token with
the real title**, that is a deliberate break in that invariant and
needs your call, since it changes what every completion is allowed to
do and not just this case. The narrow version (replace only when the
match required folding) is doable but means the invariant becomes
"pure extension, except when folded", which is harder to reason about
than either rule on its own.

### navidrome password storage

It cannot be hashed away: subsonic auth is `md5(password + salt)` with
a fresh salt per request, so the client needs the cleartext for every
call. Options offered were agenix plus `password_file` (supported, and
the only one the home-manager module exposes), opensubsonic
`apiKeyAuthentication` (spec verified, but navidrome support is an
unmerged PR so there is no server to talk to), or app-side hardening.
**Maintainer chose app-side hardening only**: 0600, zeroize on drop,
and a warning when a plaintext password is persisted. Rotation of the
credential that was echoed into transcripts was **declined**.

### mpv `--no-config`

Pinned unconditionally rather than made configurable. wim drives mpv as
a backend and sets its own af chain, replaygain, volume and vo; a user
`mpv.conf` layering on top produces symptoms indistinguishable from a
bug in wim, which happened once already. Honouring a user's mpv.conf
would want an explicit opt-out field, not silent inheritance.

Related, found later by measurement: `--no-terminal` **breaks vo
auto-sizing** (mpv never queries the tty and falls back to 80x25), so
wim pushes the video geometry itself. See `docs/video.md`.

### suggestions menu keybind and scope

`R` for recommendations: `s` and `S` were already shuffle and starred,
and `R` keeps the shift-letter convention the other view-openers use.
Remappable like anything else.

Shipped against local play history only; the network-backed generators
stay quiet until their fetches are wired. Deliberate rather than
blocking the whole menu on network plumbing, and the view carries an
`enriching` flag so a thin list says "so far" rather than presenting
itself as complete.

### mouse: the text-entry tiers do not swallow clicks

Eight of the ten input tiers swallow a click aimed elsewhere; the two
text-entry ones (`editing_query`, `bv.filtering`) deliberately do not.
A keystroke has no location, so a text box must claim every key or lose
some; a click names its target and cannot leak that way. So the swallow
set is exactly the keyboard tiers that paint over the panes. A click
elsewhere takes the keyboard out of the box; a click *into* a box
claims it, since otherwise clicking a visible search box would read as
a click on the pane around it and close it.

**Confirmed by the later mouse audit**, which checked specifically
whether this caused the maintainer's reported symptoms. It did not.

### album art `Fit::Contain`

A cover that is not square letterboxes rather than stretching. For a
square cover it is a no-op, since the layouts already size the rect
square against the same `cell_aspect`. `Fit::Stretch` restores the old
behaviour, one argument away.

**Corrected:** this entry used to say Contain also made "the video pane
aspect-correct instead of stretched". That half is obsolete. wim does
not rasterize video at all any more; mpv draws it with its own vo and
handles its own aspect.

---

## answered by later work, kept only so the reasoning is not lost

### video rendering architecture

Resolved: mpv owns video rendering in its native GPU-backed window; Wim
does not extract or rasterize video frames. See `docs/video.md`.

### download trigger policy: shipped

`cache_listens_threshold` counted plays, so playing a song twice in one
sitting triggered a download. It is "played on two distinct days" now,
with the config field and the nix option renamed to match, since a
field whose name lies is worse than a rename.

### shellsong language design: superseded, deliberately

The syntax went through three revisions in one sitting (producer
argument dropped, then barewords dropped entirely), landing on a
call-only grammar with typed positional parameters, `[]` always a list
of plain unquoted strings, and single values interchangeable with
one-element lists. One deliberate asymmetry worth not "fixing":
`play(...)` accepts songs or albums, `queue(...)` is song-preferring,
`queue_aa(...)` widens to albums.

The entry used to end "the maintainer has asked for substantially more
design work; it is explicitly not finished". That work is now scoped
and in progress: variables, `.shg` scripts, per-identity eq and volume,
and the queue timeline (pause as an entry, a default gap between
tracks, clips). **A `when` conditional engine was proposed and
explicitly rejected by the maintainer** ("writing conditionals is just
not particularly it"), which was the right call: a rules engine is a
programmer's idea of a feature and the timeline is something a listener
uses. See ROADMAP's shellsong sections and `docs/shellsong.md`.
