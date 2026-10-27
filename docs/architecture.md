# architecture

technical design decisions and why they were made. read this before
touching the areas it covers; the one-line comments in code point back
here rather than repeating the reasoning inline.

## mpv via json ipc, not libmpv

`src/mpv.rs` spawns `mpv --idle --input-ipc-server=<socket>` and talks
newline-delimited json over a unix socket, instead of linking libmpv via
FFI (the `libmpv`/`libmpv2` crates).

why: no pkg-config/C headers needed at build time, which keeps the nix
packaging story simple (just `mpv` as a runtime dep, nothing to find at
build time). it's also exactly how most external mpv controllers
(mpv-mpris, various mpv remotes) are built; a well-trodden path, not a
workaround.

the reader thread demuxes two kinds of messages on the same socket:
request replies (matched by `request_id`) and async events
(`property-change`, `end-file`, ...), dispatched to different channels so
callers awaiting a specific command's reply never see event noise.

## strictly monochrome ui

every color anywhere in the ui is `Color::Rgb(v, v, v)` via
`src/theme.rs`'s `shade()`/`lerp_shade()` helpers. no hues, ever,
including album art in `blocks` mode (see below). this is an explicit,
non-negotiable aesthetic choice, not a limitation of the tooling; state
(errors, selection, emphasis) is conveyed via brightness, weight, and
modifiers instead of color.

## no stock ratatui widgets

`Block`/`Paragraph`/`List` with default borders produce the generic
"boxes everywhere" tui look. ratatui itself doesn't force that; it's
just a cell buffer (`Buffer`) you can paint however you want, with full
rgb/truecolor per cell and no forced borders. every widget in `src/ui/`
implements `Widget::render` by writing cells directly
(`buf.cell_mut((x, y))`), never composing the stock widgets. see
`src/ui/banner.rs` for the reference pattern: a custom glyph grid with an
animated brightness sweep, built on ratatui's buffer/terminal/input
plumbing (resize handling, double-buffering/diffing, crossterm input)
without touching its widget library.

## album art: real images vs. pixelated blocks

`ratatui-image` supports multiple rendering backends (sixel, kitty
graphics protocol, iterm2 protocol, and a `Halfblocks` unicode fallback)
with terminal auto-detection. the `album_art` config option
(`auto | blocks | sixel | kitty | iterm2 | off`) exposes this directly:

- `auto`/`sixel`/`kitty`/`iterm2`: real raster album art, true color;
  this is actual image data, not ui chrome, so the monochrome rule
  doesn't apply to it.
- `blocks`: forces the halfblock protocol even on terminals that support
  real graphics, *and* desaturates the art to grayscale first, so users
  who want the "pure tui" look get it consistently rather than a
  colorful image poking a hole in an otherwise monochrome interface.

## search: two signals, not one

`src/search.rs` merges two independent matching passes into one ranked
list, because they catch different failure modes:

1. **nucleo-matcher subsequence fuzzy score**; fast, handles
   substrings/skips/case well. catches typos where the letters are still
   in the right relative order (e.g. a dropped letter).
2. **damerau-levenshtein edit-distance fallback** (`strsim` crate),
   computed per word in the haystack; catches genuine transposition
   typos where the letters are *not* in subsequence order (e.g. two
   adjacent letters swapped), which a pure subsequence matcher cannot
   match at all no matter how fuzzy its scoring is.

an item matches if *either* signal fires. the budget for the
edit-distance fallback (`edit_distance_budget()`) scales with query
length; deliberately generous, because a missed result is worse than
an occasional over-eager match. see `docs/features.md` for the product
reasoning (why typo tolerance isn't optional here).

the corpus is a flat `Vec<SearchItem>` spanning local music, local
video, and every navidrome entity type (artist/album/song/playlist):
one index, one ranked list, not per-source silos rendered next to each
other.

## subsonic: hand-rolled client

`src/subsonic.rs` talks to navidrome's subsonic-compatible api directly
over `ureq`, rather than using an existing subsonic crate; the
available ones are unmaintained or incomplete, and the actual surface
needed (ping, search3, playlists, stream urls) is small. token auth
follows the spec exactly: `t = md5(password + salt)` with a fresh random
`s` per request, so the plaintext password itself never goes out on the
wire (independent of tls).

## config: file-first, nix-declarative on top

`src/config.rs` always reads a single toml file
(`$XDG_CONFIG_HOME/wim/config.toml`). the planned home-manager module
(see roadmap) doesn't introduce a second config path; it just *writes
that same file* declaratively from nix options, so hand-edited and
nix-managed config are indistinguishable to the app. credentials use a
`password_file` option (a path, compatible with agenix) rather than an
inline nix option value, since nix store paths are world-readable and a
plaintext password embedded in a derivation would leak to every local
user.

## app state is decoupled from rendering

`src/app.rs`'s `App` struct and its `handle_key()` state machine have no
dependency on an actual terminal; `crossterm::event::KeyCode` is just a
plain enum, constructible in tests without a tty. this matters
practically: raw-mode rendering can't be exercised headlessly (no real
tty in this build/test/loop context), but all the *logic*; selection
movement and clamping, the `gg`/`G` vim-style pending-key handling,
query editing, what "enter" does; is fully unit-tested without one.
