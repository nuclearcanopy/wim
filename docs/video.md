# video

how fullscreen video playback works, and why it is built the way it is.
`src/video.rs`, `Mpv::set_video_output`/`set_video_geometry`,
`TerminalGuard::draw_unless_video`/`reclaim` and the handover block
right before the one `terminal.draw` in `main.rs` all point here.

the short version: **mpv draws the video, wim stops drawing.** no frame
ever crosses the ipc socket.

## why frames cannot be pulled out of mpv

the previous design asked mpv for `screenshot-raw` and rasterized the
result in wim. that is not possible over json ipc, and the failure is
not a tuning problem.

mpv's own manual says `screenshot-raw` "can be used only through the
client API or from a script using `mp.command_native`", and that is
literal. over json ipc mpv answers it with

```
{"data":{"w":640,"h":480,"stride":2560,"format":"bgr0","data":},...}
```

a `data` field with nothing after it, because the value is an
`MPV_FORMAT_BYTE_ARRAY` and mpv's json writer has no encoding for one.
the line does not parse as json at all. measured on mpv 0.41.0 for a
real h264 file and for a lavfi source, with and without `--vo=null`, for
every `flags` value.

the old code called it from the main loop, so each attempt cost the full
10s reply timeout: the ui froze in 10-second blocks for as long as a
local video played. `mpv::FramePump` now recognises an unparseable reply
as `PumpState::Unsupported`, stops asking and says so once. **that path
is a dead end; do not revive it.** the measured alternatives are all
bad: `screenshot-to-file` plus a read-back is 200ms (480p) to 530ms
(1080p) per frame, i.e. 2-5fps at best, and the others (an mpv lua
script shipping frames out, or `--vo=image` writing every frame to a
directory) are a second moving part each.

## what mpv can do instead

mpv has video outputs that draw into a terminal directly. on this build
(0.41.0) there are two, and no sixel vo:

- **`tct`**, true-color terminals. `--vo-tct-algo` defaults to
  `half-blocks`, so a cell is two vertically stacked pixels of 24-bit
  colour. works in **every** terminal that does truecolor, alacritty
  included.
- **`kitty`**, the kitty graphics protocol. real pixels, and needs a
  terminal that decodes `ESC _G`.

mpv drawing its own output is the whole design. it also means the video
is mpv's pixels, not wim's, so the monochrome rule in
`docs/architecture.md` does not apply to it; do not try to make mpv
grayscale.

### `vo` is settable at runtime

`set_property vo tct` over the existing ipc connection works
mid-playback. verified against a real mpv: with a file already playing
under `--vo=null`, flipping to `tct` and back left `time-pos` advancing
monotonically (2.0 -> 4.0 -> 5.0) and `pause` false throughout. so there
is **no relaunch** and nothing to seek back to; the ipc layer's `seek`
and `time-pos` are not involved at all.

one guard rail: `set_property vo bogusvo` replies `"error":"success"`
and then simply never draws. mpv validates nothing here, so the value
handed to it is always a `video::Vo` variant, never a string from
config.

### mpv's stdout had to be reconnected

`Mpv::spawn` used to set all three stdio handles to `Stdio::null()`.
with stdout closed, a drawing vo's escapes go nowhere, so this had to
become `Stdio::inherit()` for stdout.

that is safe for the common case: with `--vo=null`, mpv writes **zero
bytes** to an inherited stdout across a load, a seek and a volume change
(measured by fencing a pty capture). `--no-terminal` already implies
`--really-quiet`, so there is no status line to land on top of the ui.
stdin stays `null` so mpv can never compete with the tui for
keystrokes, and stderr stays `null` so a loader warning cannot either.

### the geometry has to be pushed from wim

leaving `--vo-tct-width`/`height` at mpv's default of 0 ("ask the tty")
does not work under `--no-terminal`: mpv never initializes its terminal
layer, never queries the tty, and silently falls back to **80x25**.
measured on a 100x30 pty: auto-sizing produced a 66x25 grid centred in
an 80-column field, where the same mpv with `--terminal` correctly
produced 80x30. so wim pushes `vo-tct-width`/`vo-tct-height` and
`vo-kitty-cols`/`vo-kitty-rows` from the size ratatui already knows.

mpv reads those **once, when the vo initializes**. changing them live is
accepted and ignored (verified: a 60x20 vo stayed 60x20 after a request
for 40x14). a terminal resize therefore has to bounce `vo` through
`null` and back, which does pick the new size up. wim is not drawing in
between, so the flicker belongs to nobody.

mpv letterboxes and centres inside whatever grid it is given, so wim
does no aspect maths.

### `auto` cannot be optimistic about kitty

mpv's kitty vo probes nothing. pointed at a plain pty that cannot decode
the protocol it emitted 2327 `ESC _G` sequences anyway, with no error
and no fallback; the user would see a screenful of mojibake.

so `video_output = "auto"` resolves to kitty **only** when wim's own
graphics probe reported `ProtocolType::Kitty`, and to `tct` for
everything else: another protocol, halfblocks, or a probe that has not
answered yet. note that `Picker::from_query_stdio()` returning `Ok` does
not mean a real protocol exists; on `NoCap` it hands back a `Halfblocks`
picker with a made-up font size, so `picker.is_some()` is not a proxy
for "this terminal does graphics" and `protocol_type()` has to be asked.
see `video::drawing_vo`.

## the handover

while a drawing vo is up, `main.rs` does not call `terminal.draw` at
all. that is not an optimization: ratatui repaints from its own buffer,
so one draw would overwrite the video with whatever ui was on screen
before the handover. the gate is `TerminalGuard::draw_unless_video`, so
the single `draw` call in the app stays a single expression.

the loop keeps running at the display rate: every background channel is
still drained, mpv's property events are still folded in, and input is
still read, so the toggle key, quit and everything else keep working
with nothing on screen but the video.

`hits` (the mouse hit map) is reset before the draw and refilled *by*
the draw, so a skipped frame leaves it empty. that is the correct
answer rather than a bug: there are no wim widgets on screen to click.

### taking the screen back

the vos own the screen while they draw, and the escapes they emit are
**not** balanced around a single vo's lifetime. measured, per window:

| event | emits |
| --- | --- |
| `set vo tct` | `?1049h` (enter alt screen), `?2J`, `?25l`, `?1003h` |
| `set vo null` | `?25h`, `?1003l` |
| mpv process exit | `?1049l` (leave alt screen) |

so after a handover back, wim is still on the alternate screen, but the
cursor is visible again and wim's own mouse capture is off.
`TerminalGuard::reclaim` re-asserts both, re-enters the alternate screen
as insurance against a vo that stops behaving this way, and then calls
`Terminal::clear()`. the clear is not cosmetic: ratatui diffs the next
frame against what it last drew, and mpv overwrote all of it, so without
dropping that notion of the screen only the cells wim's own buffer
thinks changed would be repainted.

the asymmetry is also why the exit path is ordered backwards from the
usual one. mpv holds that `?1049l` until its process exits, however long
ago the vo went away, and `Mpv`'s drop normally runs *after* the terminal
guard's; left that way the escape lands on the user's shell, where it
restores a cursor position the shell never saved. so when a vo was up,
`main` turns it off and drops `Mpv` before dropping the guard.

### what takes the screen back, and when

`want_vo` is `Some` only when all of these hold: 'v' is on, the player
is not minimized, no overlay (onboarding, nav manager, help, settings,
keybinds, eq) is open, and the playing item is a video. an overlay
pulling the screen back matters: a menu nobody can see is a hang as far
as the user is concerned.

"is a video" is `now_playing_is_video`, a plain variant test for
`SearchItem::LocalVideo`, which is only ever produced for a file whose
extension is in `library::VIDEO_EXTENSIONS`. a music track, local or
remote, can therefore never acquire a drawing vo; audio-only playback
never leaves `--vo=null`.

## what is still missing for in-workspace video

fullscreen is the whole scope here. compositing video into a sub-rect of
the tui (the `main_content` pane, next to the sidebar and the bottom
bar) does not work yet and is a separate, harder problem. what stands in
the way:

1. **wim repaints every cell it owns, every frame.** sharing the screen
   means wim must never write the cells mpv is drawing into. ratatui has
   no concept of a hole in its buffer; it owns a rectangle of `Cell`s
   and diffs all of them. something has to teach the render path to skip
   a rect, and the diff to not "restore" it.
2. **the two vos place themselves differently.** `kitty` has
   `--vo-kitty-left`/`--vo-kitty-top` and `--vo-kitty-alt-screen=no`, so
   it can be told to draw a sized image at an offset without touching
   the rest of the screen: that is the plausible path. `tct` has
   `--vo-tct-width`/`height` and **no** left/top and no alt-screen
   switch, so it always paints from the top-left of the alternate screen
   it just entered. a sub-rect tct would need either an upstream mpv
   option or wim moving the cursor for it, which it cannot, since mpv
   does its own absolute positioning per row.
3. **neither vo can be told to stop clearing.** both emit `ESC[2J` at
   init, and `vo-kitty-config-clear` only governs image deletion.
4. **the geometry is init-only.** every pane resize becomes a vo bounce
   (see above), which currently flickers through the primary screen. a
   sub-rect that follows a resizable layout needs that to be cheap, so
   `vo-kitty-alt-screen=no` is a prerequisite rather than a nicety.

the realistic next step is kitty-only sub-rect video via
`alt-screen=no` + `left`/`top`, with tct staying fullscreen-only. that
also needs (1) solved regardless, and (1) is the real work.
