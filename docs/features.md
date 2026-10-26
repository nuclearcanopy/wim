# features

living feature spec with the "why," not just the "what." when adding a
feature, the standing bar (see `feedback_proactive_feature_quality` —
applies to every feature, not just the ones explicitly requested): is
this an incredibly loved feature in top-tier apps? is it modern,
efficient, and does it make the app better for everyone? if yes, build
that version, not the minimal one, even if not explicitly asked.

## search

**requirement:** one keyword should surface matching artists, albums,
songs, playlists, local tracks, and local video together — never a
dead end.

**why typo tolerance isn't optional:** a fuzzy matcher that only does
subsequence matching (does the query appear as a subsequence of the
target, in order, possibly with gaps) already catches a lot of typos —
dropped letters, extra letters, case differences. but it fundamentally
cannot catch a transposition (two adjacent letters swapped), because
the letters are no longer in the right relative order *at all*, not
just gappy. real people transpose letters constantly. "never have no
results" means handling both failure modes, not just the easy one —
see `docs/architecture.md`'s search section for how the two signals
(nucleo subsequence + damerau-levenshtein edit distance) are combined.

**status:** implemented (`src/search.rs`). not yet wired into the app
loop against a live navidrome server (needs real credentials to
exercise end to end — see `QUESTIONS.md`).

**candidate future improvements** (not yet built, listed so they don't
get lost): weight recently-played / frequently-played items higher in
ranking (most players do this); as-you-type debounce for remote
(navidrome) queries specifically, since local search is in-memory and
instant but hitting the server on every keystroke isn't a good citizen.

## playback

**eq:** 10-band graphic eq via mpv's audio filter chain is table stakes,
but raw sliders alone is the *minimal* version. every well-regarded
player (foobar2000, winamp-lineage players, most mobile players) also
ships **presets** (flat, bass boost, vocal boost, treble boost, rock,
classical, ...) so most users never have to touch individual bands.
build presets alongside the raw sliders from the start, not as a
later add-on.

**gapless playback:** albums meant to flow as one continuous piece
(live albums, concept albums, classical movements) must not have a gap
or click between tracks. this needs the *next* track's output queued/
preloaded before the current one ends, not loaded reactively on
`end-file`. mpv supports this; the queue/autoplay logic needs to
account for it explicitly rather than treating track transitions as
"stop, then load the next file."

**crossfade:** standard, loved, and user-expect-able in modern players
(spotify, apple music, foobar2000 all have it). should be a toggle +
duration setting, off by default so it doesn't surprise anyone who
just wants gapless.

**smart shuffle:** naive full-random shuffle feels bad in practice —
it noticeably repeats recently-played tracks and clusters by chance.
a shuffle that avoids recent repeats (e.g. weighted against a recency
window, or a shuffled-bag algorithm) is what every major player
actually ships under the name "shuffle," even though "true random" is
what the word implies. implement the version that feels good, not the
literal naive one.

**status:** mpv control layer exists (`src/mpv.rs`), methods for
play/pause/seek/volume/speed/loop-file are wired but nothing calls them
yet — the app event loop (`src/app.rs` + the `main.rs` integration) is
the next piece. eq/gapless/crossfade/smart-shuffle are not yet built;
tracked here and in `ROADMAP.md` so they don't get forgotten once the
literal spec items are done.

## queue

a persistent play queue (not just "play one thing at a time") is
expected baseline behavior in any serious media player — add to queue,
play next, reorder, see what's coming up. without it the app would feel
primitive regardless of how good search or eq are. not yet built.

## now-playing visualizer

given the strictly-monochrome, custom-painted, omarchy-homepage-inspired
aesthetic already established (see `docs/architecture.md`), a live audio
level meter (think `cava`, the hugely popular terminal audio visualizer)
rendered as animated unicode blocks fits the project's own identity
directly — modern, efficient (mpv already exposes audio level data), and
genuinely loved in the terminal-tool space this app lives in. candidate
for the now-playing view once the core player loop exists.

## persistence

resuming where you left off (queue, position in the current track,
volume) across restarts is standard in podcast/audiobook apps and most
modern music players. not yet built; will piggyback on the config file
once the app loop exists to actually have state worth persisting.

## album art

real images (sixel/kitty/iterm2, auto-detected) or a desaturated
pixelated-block mode for users who want to keep the interface purely
monochrome. see `docs/architecture.md` for the technical mapping onto
`ratatui-image`'s protocols. not yet wired in — needs the app loop and
a real now-playing view to render into first.

## subtitles (video)

auto-fetch from opensubtitles on local video playback, with full
manual control: delay, language, size, outline, background opacity/
blur/radius, position. color stays grayscale per the monochrome rule
even here — this is ui chrome (subtitle rendering), not image content,
so the same rule that applies to the rest of the interface applies to
it. not yet built; needs a real opensubtitles api key to live-test
(see `QUESTIONS.md`).
