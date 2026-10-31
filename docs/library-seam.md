# the library seam

why this crate has a `src/lib.rs`, where the line between the player
core and a front-end is drawn, and what a front-end is allowed to
refuse. read this before moving anything across that line.

## why there is a library at all

until shellswim there was one binary, so every module was a private
module of it and `main()` was free to keep the whole world in locals.
a second front-end cannot share a line of that: a bin crate exports
nothing. so `src/lib.rs` declares every module and both binaries are
`use wim::...` consumers of it.

the split is deliberately *not* "move the easy half". the test of
whether the line is in the right place is whether an op applies with
no pane state in the room at all, which is what
`core::tests::ops_apply_with_no_ui_state_at_all` asserts and what
shellswim is the real-world proof of.

## what the core knows

`core::CoreCtx` is `ActionCtx` plus the shellsong-only fields, minus
everything about panes. it holds: config, the mpv handle, the session
(`app::App`: corpus, query, results, and the playback modes), the play
queue, the cache index, favorites and the remote starred set,
notifications, scrobble state, the download and subtitle channels, the
volume/crossfade/fade/sleep ramp state, the library scan receiver, the
remote search sender, the search settle window, the saved-queue list,
the lyrics client, and the console.

## what the core does not know

these never appear in `CoreCtx`, in either front-end's build of it:

- `Focus`, and anything that moves it
- `browse_view`, `browse_loader`, `real_art_protocol`
- `player_minimized`
- `flash` (the bottom bar's per-field highlight)
- the six `*_menu_open: bool` / `*_selected: usize` pairs
- `lyrics_lines` / `lyrics_track` / `lyrics_scroll`
- `suggestions` / `suggestions_selected`
- `queue_view_open` / `queue_selected`
- `bottombar_selected`

## how the core asks for what it cannot do

`core::Surface`, a trait each front-end implements, passed alongside
`CoreCtx` as `&mut dyn Surface`. two disjoint borrows: the surface
holds the front-end's pane locals, `CoreCtx` holds the core's. every
method has a default implementation that **declines** with a one-line
reason, so:

- adding a `Surface` method cannot silently break a front-end
- a front-end opts in to exactly what it can honour
- declining is a normal, legible outcome, not a panic and not a lie

this was chosen over the other two candidates:

- **a request queue** (`Vec<FrontendRequest>` drained after the op).
  rejected because wim's behaviour has to be bit-identical and several
  of these effects are ordered *inside* an op: `view(starred)`
  invalidates the art protocol, then builds the view, then moves
  focus, and `go(browse row)` reads the browse view before starting a
  session from it. a queue drained afterwards reorders all of that.
- **a generic `Host` trait with the core as an associated type.**
  rejected because the core is not polymorphic; there is exactly one
  player. the polymorphism is on the presentation side only, which is
  where the trait belongs.

## what each front-end honours

| surface request | wim | shellswim |
| --- | --- | --- |
| `selection()` | the highlighted row, browse view first | top of the current result set |
| `open_screen(now)` | the full player pane | one scrollback line |
| `open_screen(lyrics)` | the lyrics pane | the lyrics, as scrollback |
| `open_screen(eq/settings/keybinds/starred/suggestions)` | the menu | declines, naming wim |
| `move_selection()` | pane-aware, see `wim`'s impl | declines; there is no highlight |
| `go(browse row)` | plays from the open browse view | declines |
| `open_browse()` | opens the track listing | declines |
| `bar_field()` | the bottom bar's selected field | `None`; nothing is focusable |
| `flash()` | lights the field for 600ms | no-op |
| `minimize_player()` | toggles the mini player | no-op |
| `queue_cursor()` | the queue view's cursor, reclamped | `None` |

the five `ViewTarget`s that `ViewTarget::is_screen()` says are *not*
screens (`queue`, `index`, `history`, `playlists`, `help`) print into
the console and so are handled in the core, identically in both
binaries. that partition already existed in the language; the seam
just started honouring it.

## `dry()` describes every op in both binaries

`dry(...)` never reaches the op table: `Lowering::lower` turns the
whole call into `Node::Note` lines and the host only ever sees
`Step::Note`. so `dry(view(eq))` in shellswim still describes exactly
what wim would do, including the ops shellswim would decline to run.
that is the intended reading: `dry` answers "what does this mean",
not "what would happen here".

## what is still joined that should not be

`app::App` holds the corpus, the query and the result list (core: the
interpreter resolves names against them, in both binaries) alongside
`selected`, `editing_query` and `pending` plus the key handling
(wim's). the core carries the whole struct and simply never touches
the second group. splitting `App` would touch its sixteen tests and
several hundred call sites in `main.rs` for no behaviour change, so it
was left; `CoreCtx`'s field list is the contract, not `App`'s.
