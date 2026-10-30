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

one detail worth stating, because getting it wrong looks exactly like a
stale cache: a `property-change` for a property that has *become
unavailable* (af cleared, playback stopped) carries no `data` key at
all, not a `data: null`. requiring the key dropped precisely the event
that says "this reading is gone", so the last one stood forever. the
reader coerces a missing `data` to `Value::Null`, which every folder
already reads as "no value".

## nothing on the render path waits for mpv

`get_property` is a blocking request/reply round trip; measured at
~1.5ms on a warm local socket. at display refresh rate that is the
frame budget, so the rule is: the render path reads state, it never
asks for it.

- everything the render path reads is `observe_property`'d and folded
  into `mpv::Playback` by `apply`: position, duration, pause, and the
  level meter's `astats` RMS tap. one pipelined round trip at startup,
  none per frame.
- the level tap's property path is `af-metadata/<label>/...` where
  `<label>` must be **mpv's own filter label**
  (`af=@wimlevel:lavfi=[...]`), not a label written inside the lavfi
  graph (`lavfi=[astats@wimlevel=...]`). the second spelling resolves
  nothing at all, which is how the meter spent a release dead while
  still paying a round trip per frame to learn nothing.
- mpv pushes that tap ~20/s while audio flows and simply stops pushing
  when it doesn't (pause), so a reading is only believed for
  `LEVEL_FRESH_FOR` (300ms) past when it landed; past that the meter
  falls back to its idle breathe. that horizon is the whole staleness
  story, since stop and af-clear both push an explicit "no value".
- per-frame *writes* (the crossfade and shellsong volume ramps) use
  `command_nowait`: the reply is the only cost, the next frame's value
  supersedes this one, and mpv applies one connection's commands in
  order so the ramp's last write still wins.
- several properties that have to be set together (`observe_playback`,
  `set_subtitle_style`) go through `command_batch`, which writes them
  all and then collects every reply: one round trip's latency for the
  batch instead of one each. the subtitle menu fires its seven on every
  keypress.

## video: mpv draws it, wim stops drawing

fullscreen video ('v') is one of mpv's own terminal video outputs
(`tct` half-blocks, or the `kitty` graphics protocol) writing straight
to this terminal, while wim skips `terminal.draw` entirely for as long
as it is up. `vo` is settable live over the existing ipc connection, so
there is no relaunch and no position to preserve.

the rest of the reasoning; why frames cannot be pulled out of mpv at
all, why mpv's stdout had to be reconnected, why the vo's geometry has
to be pushed from wim, why `auto` cannot guess kitty, and what still
blocks video in a sub-rect of the ui; is in `docs/video.md`. read that
before touching `src/video.rs`, `Mpv::set_video_output` or
`TerminalGuard::reclaim`/`draw_unless_video`.

## video frames: a pump, not a poll

the frame pump below is the *old* approach and its source does not
work; see `docs/video.md`. it is still wired up, and still reports
`Unsupported` rather than hanging, but nothing draws from it.

`mpv::FramePump` owns a second ipc connection and a thread. it captures
on an interval, decodes, and keeps exactly the newest frame; the render
path calls `latest()`, which is a short lock and an `Arc` clone. a
decode slower than the interval therefore drops frames instead of
building a backlog, and the ipc round trip, the json parse of a
multi-megabyte payload, and the bgr0 to rgb conversion are all off the
render thread. `Capture { interval, max_edge }` is the cost knob:
frame rate and optional downscale are parameters, not constants.

the second connection is deliberate. a frame reply is megabytes of
json; sharing the control connection would put that parse in front of
the `time-pos` events the render path reads.

**the frame source itself does not currently work.** mpv's own manual
says `screenshot-raw` "can be used only through the client API or from
a script using `mp.command_native`", and that is literal: over json ipc
mpv answers it with

```
{"data":{"w":640,"h":480,"stride":2560,"format":"bgr0","data":},...}
```

i.e. a `data` field with nothing after it, because the value is an
`MPV_FORMAT_BYTE_ARRAY` and mpv's json writer has no encoding for one.
the line does not parse. measured on mpv 0.41.0 for both a real h264
file and a lavfi source, with and without `--vo=null`, for every
`flags` value.

the old code called this straight from the main loop, so the failure
cost the full 10s reply timeout per attempt: the whole ui froze in 10s
blocks for as long as a local video was playing. the pump recognises an
unparseable reply as `PumpState::Unsupported`, stops asking, and says
so once; a blank pane is bad, a frozen app is worse.

what is left for a working source, none of it free: `screenshot-to-file`
plus a read-back measures 200ms (480p) to 530ms (1080p) per frame on
mpv's side, so ~2-5fps at best; an mpv lua script using
`mp.command_native` and shipping frames out itself; or `--vo=image`
writing every frame to a directory. the pump's public surface
(`latest()` -> newest `Frame`) does not change under any of them.

## strictly monochrome ui

every color anywhere in the ui is `Color::Rgb(v, v, v)` via
`src/theme.rs`'s `shade()`/`lerp_shade()` helpers. no hues, ever. this is
an explicit, non-negotiable aesthetic choice, not a limitation of the
tooling; state (errors, selection, emphasis) is conveyed via brightness,
weight, and modifiers instead of color.

album art is the one sanctioned exception, authorised by the maintainer
directly: the `ascii-color` and `full-color` modes show the cover in its
own color (see below). it is image data, not ui chrome. nothing else on
screen may carry a hue, and `ascii-bw`/`full-bw` stay grey.

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

## album art: four modes

two independent choices: who draws the picture, and whether it has color.
`config.album_art` (and `i`, which cycles) names the result rather than
the protocol:

- `ascii-bw`, `ascii-color`: wim's own rasterizer, `src/ui/raster.rs`.
  the sixteen 2x2 block glyphs, two samples per cell on each axis, so it
  carries twice the horizontal detail a half-block renderer does. needs
  nothing from the terminal. `ascii-bw` goes through `theme::shade`;
  `ascii-color` emits each cell's two groups as the cover's own rgb
  (`raster::Tone`), grouped by the same glyph mask so the colors and the
  glyph describe one split.
- `full-bw`, `full-color`: a real inline graphics protocol
  (sixel/kitty/iterm2) through `ratatui-image`, overlaid by main.rs on
  the rect the widget left empty. `full-bw` greyscales the image before
  the protocol encodes it, so the protocol's resize/encode cache holds
  the already-greyscaled picture; the mode is therefore part of the
  protocol cache key alongside the cover.
- `auto`: the best available, i.e. `full-color` where a real protocol
  exists and `ascii-color` where none does.
- `off`: no panel at all, the track list gets the full width.

the capability test is the *protocol*, never whether a `Picker` exists.
`Picker::from_query_stdio` falls back to `ProtocolType::Halfblocks` when
the terminal answers nothing at all, so `picker.is_some()` is true
everywhere; believing it meant alacritty rendered ratatui-image's
coloured half-blocks and called it real art, at half the rasterizer's
horizontal resolution. `main::has_real_protocol` accepts only
sixel/kitty/iterm2, a `full-*` mode on a terminal without one falls back
to the matching ascii mode and says so once, and `i` steps straight past
the full modes there rather than cycling into a blank pane.

pre-rename config values still parse as aliases: `blocks` is `ascii-bw`,
and `sixel`/`kitty`/`iterm2` are `full-color`.

## startup: nothing blocks the first frame

the app has to be on screen immediately, so the only work between
process start and the first `terminal.draw` is work that cannot be
deferred: reading config, spawning mpv, setting up the terminal. every
other startup cost either runs on a background thread and streams its
results into the main loop, or waits until the first frame is up.

- **local library scan** (`library::spawn_scan`): a tag read
  (`Probe::read`) per music file, the biggest cost of all; runs on its
  own thread, reads tags across a pool sized to the machine, and sends
  batches of tracks over a channel the main loop drains next to
  `sync_rx`. the channel disconnecting is how "scan finished" is
  reported. `library::TagIndex` caches `path -> (mtime, tags)` under the
  cache dir, so a warm start is a `stat` pass with no tag parsing at
  all.
- **navidrome** (`spawn_background_sync`): every configured server is
  connected by the sync thread, which now fetches first and waits
  `AUTO_SYNC_INTERVAL` afterwards. this used to be a synchronous
  ping + catalog + starred-songs fetch per server before the first
  frame, so a stalled server held the ui at a black screen for as long
  as it stalled.
- **restored queue**: handing mpv a saved queue costs one blocking ipc
  round-trip per track, so the appends happen on the first pass through
  the main loop, right after the first `draw`. the index map is filled
  by the same call that does each append (see "the queue is mpv's
  playlist" below), so the ui never disagrees with what's actually
  queued.
- **subtitle auto-fetch** (`fire_subtitle_fetch`): two network
  round-trips, on a thread that reports the written file back over a
  channel; the main thread calls `sub_add`, since the `Mpv` handle
  isn't `Send`.

an empty corpus therefore means "still loading" for the first moment of
a session, which the results pane says outright
(`main::loading_message`) instead of claiming an empty library.
`WIM_STARTUP_TRACE=1` (`src/trace.rs`) prints every milestone's elapsed
time to stderr for measuring any of this.

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

## the queue is mpv's playlist

there is one queue and mpv owns it. `src/playqueue.rs`'s `PlayQueue` is
not a second copy of it; it's a map from an mpv playlist index to the
`SearchItem` that index was built from, which mpv can't hold itself
(it has no title/artist/album art for a remote stream url). the
invariant is written at the top of that module: `entries` is mpv's
playlist, same items in the same order, and `pos` is the index mpv
reports it is playing.

so "what's playing" is *resolved*, never guessed: `playlist-pos` and
`idle-active` are `observe_property`'d (`Mpv::observe_playback`) and
every display, the scrobbler and the download trigger read whatever
the last reported index resolves to. the previous design inferred the
position by counting `end-file` events, which is what showed the wrong
track: `loadfile replace` emits an end-file for the track it *evicts*,
so starting anything new mid-playback counted a phantom advance and
left the whole app one track ahead of mpv for the rest of the session.
an absolute index can't drift that way; a notification that never
arrives only delays the next correction.

mpv's playlist and the index map are only ever written together, by
`main::load_playlist`, `main::enqueue_track` and
`main::resync_upcoming`. every play, queue, shuffle, back and loop-all
restart goes through one of those three. `loop all` is the one thing
mpv has no concept of, so `PlayQueue::session` keeps the list to
restart from; it is deliberately not index-mapped, since
`playlist-clear` drops the already-played prefix from mpv.

## app state is decoupled from rendering

`src/app.rs`'s `App` struct and its `handle_key()` state machine have no
dependency on an actual terminal; `crossterm::event::KeyCode` is just a
plain enum, constructible in tests without a tty. this matters
practically: raw-mode rendering can't be exercised headlessly (no real
tty in this build/test/loop context), but all the *logic*; selection
movement and clamping, the `gg`/`G` vim-style pending-key handling,
query editing, what "enter" does; is fully unit-tested without one.

## the mouse resolves against the frame it was aimed at

every rect is computed inside the draw closure, so the input handler
cannot locate anything. the draw pass records a `hit::HitMap` as it
goes and the next mouse event resolves against that one, which makes
resolution a pure function of a recorded map and therefore testable
without a terminal, same reason `App::handle_key` is. the mouse's
precedence is the keyboard's own tier order read from one list rather
than a second cascade. see `docs/input-precedence.md`.
