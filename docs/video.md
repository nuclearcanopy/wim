# video

how fullscreen video playback works, and why it is built the way it is.
`src/video.rs`, `Mpv::set_video_output`/`set_video_geometry`,
`TerminalGuard::draw_unless_video`/`reclaim` and the handover block
right before the one `terminal.draw` in `main.rs` all point here.

the short version: **mpv draws the video, wim stops drawing.** no frame
ever crosses the ipc socket.

that last clause is about the *json ipc socket* and nothing wider. a
frame source for an ascii renderer does exist; four of them do, three
measured working on this machine, and one of them is **built**:
`src/framesource.rs`, a second headless mpv on a fifo. see "a frame
source does exist" for the comparison, "the frame source as built" for
the module, and `examples/frame_probe.rs` for the 150-line prototype
the module came out of.

## why `screenshot-raw` cannot come over json ipc

the previous design asked mpv for `screenshot-raw` on the ipc socket
and rasterized the result in wim. that one command on that one
transport is impossible, and the failure is not a tuning problem.

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

re-measured on 0.41.0 and it still holds exactly. the reply is

```
{"data":{"w":640,"h":480,"stride":2560,"format":"bgr0","data":},"request_id":0,"error":"success"}
```

and `json.loads` dies at column 63. the `format` argument the manual
documents is a *pixel* format, not an encoding, so it cannot help:
`bgr0` and `bgra` both produce the same truncated line, `rgba` and
`rgba64` answer `"error running command"` under `--vo=null`, and a
non-pixel-format name like `jpg` is an `"invalid parameter"`. there is
no base64 path and no size limit involved; mpv's json writer simply has
no encoding for an `MPV_FORMAT_BYTE_ARRAY`.

the old code called it from the main loop, so each attempt cost the full
10s reply timeout: the ui froze in 10-second blocks for as long as a
local video played. `mpv::FramePump` recognised an unparseable reply
as `PumpState::Unsupported`, stopped asking and said so once. **that
one path is a dead end; do not revive it.**

what does not follow is "so there is no frame source". the byte array
is unreachable *over json ipc*; it is reachable from every other client
surface mpv has, and mpv can also be made to write frames out without
being asked for them one at a time.

## a frame source does exist

all of the below was measured on this machine, mpv 0.41.0 / ffmpeg
8.1.2, against two h264 files generated with `mpv av://lavfi:testsrc2`
(640x480@25 and 1920x1080@30).

the thing that makes all of it cheap is the one the old measurements
left out: **`--vf=scale` makes mpv do the downscale**, and every frame
source below then carries cell-grid-sized frames rather than full
resolution ones. `screenshot-to-file` on the 1080p file is 396ms per
frame unscaled (which is where the recorded "530ms" came from) and
35ms with `--vf=scale=320:192` in front of it. the expensive part was
never the transport.

| source | works | measured |
| --- | --- | --- |
| `screenshot-raw` over json ipc | **no** | unparseable reply, every flag and format |
| `--vo=image` on the audible instance | **no** | untimed: playback runs at 0.44x |
| second mpv, `--of=rawvideo` on a fifo | **yes** | 290ms to first frame, 38% of a core at 1080p |
| mpv lua script, `mp.command_native` | yes | hands over 76800 real bytes as a lua string |
| libmpv sw render api | yes | `mpv_render_context_create(sw) = 0`, frame on the 2nd render |
| `screenshot-to-file` + `vf=scale` | yes, barely | 35-41ms per frame, 24-29fps ceiling |

### `--vo=image` is out, and it is the non-obvious one

the tempting single-process answer is to flip the *audible* instance's
vo to `image` at runtime the way fullscreen flips it to `tct`, and read
the files back. `set_property vf scale=160:96` and `set_property vo
image` are both accepted mid-playback and it does write frames. it is
still unusable: `vo_image` is an **untimed** vo, so mpv stops pacing
playback against a clock. measured on the 480p file with the same
realtime-emulating null ao in both cases:

| vo | time-pos advance |
| --- | --- |
| `null` | 1.00x realtime |
| `image` | 0.44x realtime |

0.44x is the user's audio playing at half speed. /tmp here is tmpfs, so
that is not disk latency, it is mpv no longer waiting for anything.
`vo=image` can only ever be a *second* instance's vo, and as a second
instance's vo it is strictly worse than rawvideo (a png encode and a
file per frame instead of neither).

### the chosen source: a second mpv writing rawvideo to a fifo

**this is built: `src/framesource.rs`.** the sketch below is the
prototype's version of the command; the shipped filter chain differs in
one way that matters, see "the filter chain is not the prototype's"
below.

```
mpv --no-config --no-terminal
    --vf=scale=W:H:force_original_aspect_ratio=decrease,pad=W:H:-1:-1:color=black,format=rgb24
    --audio=no --sub=no --start=<time-pos>
    --of=rawvideo --ovc=rawvideo --ovcopts=pix_fmt=rgb24
    --o=<fifo> <file>
```

`examples/frame_probe.rs` is this, in 150 lines, and it prints a real
frame:

```
source      /tmp/wimframe/big.mp4
frame       320x192 rgb24, 184320 bytes (w*h*3 = 184320)
first frame 144 ms after spawning mpv
px(0,0)         (0, 0, 0)
px(w/4,h/2)     (0, 216, 0)
px(w/2,h/2)     (16, 30, 244)
px(w-1,h-1)     (0, 0, 0)
mean rgb        (116.9, 122.0, 118.7)
non-black px    58239 of 61440
```

what that output is worth checking against: the byte count is exactly
`w * h * 3` with no stride to interpret, the pixel values are
testsrc2's own colour bars (green at a quarter across, blue at the
centre), and 58239/61440 is 94.8% non-black, which is 16:9 letterboxed
into a 5:3 grid to within the antialiased edge rows. the `pad` is what
makes every frame the same size whatever the source aspect is, so the
reader never renegotiates.

three properties make this the recommendation rather than merely a
thing that works.

**the fifo is the clock.** mpv blocks in `write(2)` once the pipe
fills, so the rate wim reads at *is* the rate mpv decodes at. measured:
after a 1.0s stall only 5 frames were waiting, not the 25 a free-running
decoder would have produced. pause is "stop reading" and costs nothing;
there is no buffer to bound and no racing ahead. this is also exactly
what the frame-budget roadmap item asks for, a frame rate wim picks
rather than one it is handed.

**sync is read scheduling, not seeking.** the frame instance has no
clock of its own, so it never drifts on its own either. frame *n* is
media time `start + n / container-fps`, and `container-fps` is already
readable over the existing ipc socket (30.083565 on the 1080p file).
wim knows the audible instance's `time-pos` because it already observes
it, so "which frame should be on screen" is arithmetic: read and discard
to catch up, stop reading to wait. only a user seek needs a restart,
and a restart with `--start=` costs 178-545ms to first frame on the
1080p file.

**there is no second video decode if the audible instance stops doing
one.** while ascii mode is on, the picture comes from the frame
instance, so the audible instance can be put on `vid=no` through the
`set_vid_for_item` machinery that already exists. one video decode, one
audio decode, two processes.

the costs, stated plainly:

- **one extra process.** 134MB RSS for the 1080p file, 74MB for the
  480p one.
- **cpu.** 38% of one core to decode 1080p h264 and scale it to 320x192
  at 24fps; 36% to 160x96 (so it is the decode, not the scale); 10% for
  the 480p file. the decode-and-scale ceiling if read as fast as
  possible is 76fps at 1080p and 297fps at 480p.
- **~180-550ms of black on a seek**, because the frame instance is
  restarted rather than seeked. encode mode has no ipc socket to seek
  over.
- **no new nix dependency.** mpv is already a runtime `PATH` dep via
  the flake's `wrapProgram`, nothing is linked, and `src/mpv.rs` does
  not change: mpv stays a subprocess.

## the frame source as built

`src/framesource.rs`, plus `tests/framesource_alloc.rs` for the one
measurement that needs its own allocator. three types and no more:

- **`Grid`** is a cell rect plus the terminal's cell aspect, and it
  owns all the geometry: `pixels()` is the frame size handed to mpv,
  `frame_len()` is exactly how many bytes a frame is, and `vf()` is the
  filter chain. nothing else computes a size.
- **`Schedule`** is the arithmetic, and it is pure: `step(now)` answers
  `Hold`, `Advance(n)` or `Restart(at)` for a media time, with no io
  anywhere near it. every sync decision is testable without spawning
  anything, which is most of the test file.
- **`FrameSource`** owns the child, the fifo, the temp dir and the read
  loop. `pull()` takes whatever has arrived, `advance_to(now)` is the
  one call a host needs per ui frame, `restart(at)` is a seek,
  `resize(grid)` follows a pane, and `Drop` takes all of it down.

`paint()` goes through `ui::raster`, the album-art rasterizer, with
**no change to it at all**. that was not the expected answer: `raster`
takes a `&DynamicImage` and the frames are raw bytes. the trick is that
`FrameSource` keeps its two buffers *as* `DynamicImage::ImageRgb8` and
reads the fifo straight into one of them, so handing a frame to the
rasterizer is a borrow, not a conversion. `raster` is a 2x2 quadrant
rasterizer (its own module doc and docs/raster.md still say
half-blocks; they are stale), so `Grid` asks mpv for `2cols x 2rows`
pixels and paints with `Fit::Stretch`, which at that size is a 1:1 copy
with nothing left to resample. a half-block renderer is the same code
with `cols x 2rows`: each cell's two horizontal samples then come from
one source column, `quantize` can only pick the top/bottom split, and
`QUADRANTS` degenerates to `▀`/`▄`/`█`. no second rasterizer.

### the filter chain is not the prototype's

the prototype letterboxed for **square pixels**, and a terminal cell is
not square. the shipped chain is two scales:

```
scale=2cols:H:force_original_aspect_ratio=decrease,
pad=2cols:H:-1:-1:color=black,
scale=2cols:2rows,format=rgb24          where H = round(2rows * cell_aspect)
```

fit the picture inside the sample grid's *physical* shape, pad to that,
then squash by exactly the cell aspect to land on the sample grid.
measured, a 16:9 source into a 160x48 cell grid (320x96 samples):

| chain | lit sample rows | lit sample columns |
| --- | --- | --- |
| two scales (shipped) | 90 of 96 | 320 of 320 |
| one scale (prototype) | 96 of 96 | 170 of 320 |

the prototype's version pillarboxes a widescreen source and stretches
it 2x vertically. the 94.8%-non-black figure recorded above for the
probe is consistent with that: it is correct only if 320x192 is
displayed as square pixels, which in a terminal it never is. the second
scale costs nothing measurable (it is 320x192 to 320x96, against a
1080p decode). `pad` still does the job it did: every frame is the same
size whatever the source aspect is, so the reader never renegotiates.

### three things the prototype could not have found

**the fifo gotcha was real and is handled**: `Ok(0)` on a fifo with no
writer yet is a clean eof, not `WouldBlock`, so zero means "wait" and
only `try_wait` on the child says it is over. a mutation that reads
zero as eof is caught.

**a media time exactly on a frame boundary used to stall forever.**
this is the bug worth knowing about, because it is invisible until it
is total. `step` turns a media time into a frame index by flooring
`(now - start) * fps`, a host drives `now` from `time-pos`, which lands
on exact frame boundaries, and `7.0 / 25.0 * 25.0` is
`6.999999999999999`. floored, that names the frame already on screen,
so `step` says hold, so nothing is read, so `time-pos` never moves
either: a permanent freeze, at frame 29 for 25fps. `FRAME_EPS`, a
millionth of a frame on the floor, is the fix; the resolution of the
whole scheme is one frame, so it costs nothing.

**`--hr-seek` is a pin, not a fix.** `--start=5.37` on a 300-frame
1080p h264 file: mpv's *default* lands on frame 161 (t=5.3667, exact),
`--hr-seek=no` lands on frame **0**, a 5.4 second error, because the
only keyframe before it is the first one. so the default is already
right and `--hr-seek=yes` only stops that changing. a wrong `start` is
a permanent sync offset rather than a glitch, which is why it is named
rather than left implicit. note the corollary for the arithmetic: mpv
lands on `floor(start * fps)`, so `Schedule::new` snaps `start` onto
the frame grid, or every frame time is wrong by up to a frame for the
life of the instance. and `--start=0` is still a *seek*, which drops
the first frame, so `--start` is omitted when it would be zero.

### teardown, including the paths `Drop` does not reach

`Drop` kills this source's own child by its own pid, reaps it, closes
the read end and removes the fifo and the temp dir. never a
pattern-matched kill; nothing it did not spawn.

`Drop` runs while unwinding, so a panic is covered, which is what
`src/term.rs`'s guard discipline needs: the terminal restore would
otherwise be outlived by a decoder writing into a dead pipe. for the
paths that skip `Drop` entirely (`abort`, `process::exit`) there is a
second line of defence that costs nothing: the kernel closes the read
end at exit anyway, and **a frame instance with no reader takes SIGPIPE
on its next write.** measured, exit status 141. so a leaked instance is
bounded by one pipe buffer of work, not by the length of the film.

### the numbers

mpv 0.41.0, real h264 generated with `--ovc=libx264 preset=veryfast
crf=28`, `cargo test --release -- --ignored --nocapture
bench_frame_source`. "decode" is `RUSAGE_CHILDREN`, i.e. mpv's own cpu,
which does not appear in this process at all; "read" and "raster" are
`CLOCK_PROCESS_CPUTIME_ID`, i.e. wim's.

ranges are two runs, one on an idle machine and one with the rest of
the repo compiling beside it; the cpu-time columns barely move between
them, which is the point of measuring cpu rather than wall clock.

| source | cell grid | ceiling | decode | read | raster | rss |
| --- | --- | --- | --- | --- | --- | --- |
| 640x480@25 | 80x24 | 568-804 fps | 2.9-3.1 ms/f | 24 µs/f | 228-233 µs/f | 70 MB |
| 640x480@25 | 160x48 | 349-633 fps | 3.8-3.9 ms/f | 56-61 µs/f | 0.56-0.93 ms/f | 71 MB |
| 640x480@25 | 320x96 | 314-451 fps | 5.5-5.6 ms/f | 162-190 µs/f | 2.1-3.7 ms/f | 73 MB |
| 1920x1080@30 | 80x24 | 230-284 fps | 11.9-12.4 ms/f | 43-47 µs/f | 126-241 µs/f | 115 MB |
| 1920x1080@30 | 160x48 | 170-268 fps | 13.1-13.4 ms/f | 75-76 µs/f | 0.56-0.96 ms/f | 117 MB |
| 1920x1080@30 | 320x96 | 137-189 fps | 15.7-16.1 ms/f | 182-186 µs/f | 3.4-3.9 ms/f | 120 MB |

read at 24fps on the worst row: mpv is 16ms x 24 = **38-39% of one
core**, which is exactly the figure recorded for 320x192 above; wim's
reads are **0.4%** and wim's rasterization is **8-9%**. so the decode
dominates by an order of magnitude and it is in another process; wim's
own share of a 320x96 cell grid at video rate is under a tenth of a
core, and the frame-budget item's fear of cooking a core turns out to
be about the decode, which `--vf=scale` already bounded.

the ceiling is 137-189fps on the 1080p file, i.e. 4.6-6.3x real time,
so catching up by read-and-discard is genuinely faster than playing,
which is what makes `Step::Advance` a real option rather than a stall.

per cell, rasterization is 2.1-3.9ms for 30720 cells (68-127ns),
0.56-0.96ms for 7680 (73-125ns) and 126-241µs for 1920 (66-125ns), so
it is linear in cells and the grid size is not hiding a cliff. note
`raster`'s unsharp pass is unconditional and not switchable, so it is
inside those numbers; for a 1:1 frame it is pure cost, and making it
optional is the one plausible win left on wim's side of the budget.

**allocations per frame: zero, counted.** `tests/framesource_alloc.rs`
wraps `System` in a counting `GlobalAlloc` and reports 0 allocations
and 0 bytes over 50 frames at both 40x20 and 160x48, read and painted.
it is a separate test binary because a `#[global_allocator]` is
per-binary and in the lib's test binary the counters would be a race
with whatever ran in parallel. the three suspects the roadmap names are
all structurally absent: mpv downscales once to the cell grid, the read
goes into a pre-sized buffer, completing a frame swaps two pointers,
and `raster`'s scratch is thread-local and kept. the only allocating
operations on the whole surface are `spawn` and `resize`.

a dead end worth recording, because it looked like the cheap proof:
pointer identity **cannot** tell a swap from a copy. mutating
`mem::swap(&mut front, &mut back)` into `front = back.clone()` still
alternates between exactly two addresses, because the clone allocates
while the old buffer is live (so it cannot reuse that chunk) and the
next frame's clone gets the chunk just freed. the pointer test passes
on the mutant; only the counter catches it.

### what a seek costs

`bench_restart`, best and worst of six respawns:

| source | cell grid | best | worst | worst, machine busy |
| --- | --- | --- | --- | --- |
| 640x480@25 | 80x24 | 156 ms | 254 ms | 593 ms |
| 640x480@25 | 320x96 | 208 ms | 313 ms | 749 ms |
| 1920x1080@30 | 80x24 | 304 ms | 593 ms | 974 ms |
| 1920x1080@30 | 160x48 | 405 ms | 1.05 s | 1.07 s |
| 1920x1080@30 | 320x96 | 430 ms | 1.48 s | 1.86 s |

unlike the cpu columns this one is wall clock and has to be, since what
is being measured is how long the pane is black; it therefore moves a
lot with machine load, which the last column is.

worse at the top end than the 180-550ms recorded from the probe: a
respawn is a process start, a demuxer open, an exact seek and a first
decode, and at 1080p the tail is over a second. that is the design's
one real wart and it is why `Schedule::step` would rather read than
restart. the threshold is `MAX_CATCHUP = 1.0`s: closing a `g`-second
gap by read-and-discard costs about `g / 5.7` seconds at 1080p, so
break-even against a ~400ms respawn is around a second, and a catch-up
at least keeps the picture moving where a respawn blanks it.

a backwards seek smaller than the same limit **holds** rather than
respawning: the frame on screen is a little way into the future and the
clock walks back up to it, which costs nothing and beats a third of a
second of black.

### colour or monochrome: not decided here

`FrameSource::paint` takes a `raster::Tone`, so colour and greyscale
are the same call with a different argument and both work today
(tested, both ways). the default this ships with is `Tone::Mono`,
which is what `src/ui/albumart.rs` already says the video pane does:
"`Tone::Mono` everywhere but the album-art `ascii-color` mode; the
video pane stays monochrome with the rest of the ui".

that matches the existing rule rather than settling it. the rule in
docs/architecture.md is that album art is the *one* sanctioned
exception to the monochrome ui, authorised by the maintainer directly,
and the exemption video already has in this file is narrower than it
looks: it covers **mpv's** pixels, drawn by `tct`/`kitty`, on the
grounds that they are not wim's. an ascii renderer paints wim's own
cells, so it is not covered by that, and a second exception is the
maintainer's call. nothing needs to change to take it either way; if
colour is allowed it wants a config variant next to `AlbumArtMode`'s
`ascii-bw`/`ascii-color` pair, following `browse::ArtMode::tone()`.

### the two that also work, and why they are not it

**an mpv lua script** on the audible instance. this is the single
most surprising result: `mp.command_native({"screenshot-raw","video"})`
**does** hand the byte array over, as a plain lua string. with
`--vf=scale=160:96` it returned `w=160 h=120 stride=640 format=bgr0`
and a 76800-byte string, written out intact. so the manual's "client
API or `mp.command_native`" is literal, and lua is a real surface. it
is not the recommendation for one reason: lua's `io` is blocking and
the script runs on mpv's main thread, so a wim that stops reading the
fifo stalls the *audible* instance's playback. there is no non-blocking
write available from mpv's lua (no luaposix, no socket api). trading "a
slow reader slows a throwaway decoder" for "a slow reader stutters the
user's audio" is the wrong trade. note also `screenshot video` reapplies
aspect correction, so it answered 160x120 for a 160x96 filter chain,
and it carries a stride.

**libmpv's sw render api**, which is the proper answer and does work.
verified with a 40-line C program against the dev shell's own libmpv:

```
mpv_render_context_create(sw) = 0 (success)
frame after 2 renders: 160x96 rgb0, 30904 nonzero bytes of 61440
  px(0,0)=(0,0,0) px(w/4,h/2)=(74,251,0) px(w/2,h/2)=(38,45,234)
```

mpv renders straight into a caller-supplied buffer at a
caller-supplied size, which is every property wanted at once: one
decoder, sync by construction, pull-based so wim picks the frame rate,
and no transport at all.

the cost is that it is not a feature, it is a rewrite. the render
context is only available to the process that *owns* the `mpv_handle`,
and wim does not own one: it spawns mpv and talks to it over a socket,
which is the premise of all 951 lines of `src/mpv.rs` and of the
flake's "mpv is a runtime subprocess dependency, not a linked library"
comment. taking this path means wim becomes the mpv host. it also needs
flake work the flake has not had: `pkg-config` is **not** in the dev
shell (`nix develop --command pkg-config --libs mpv` is
`exec: pkg-config: not found`) and mpv's dev output is not on the
include path, so the C program above had to be pointed at the store
paths by hand. and a new crate (`libmpv2`/`libmpv-sys`) has to go into
`Cargo.lock`, for four platforms. mpv's own `render.h` calls the sw
renderer "extremely simple (but slow)".

so: rawvideo on a fifo now, because it unblocks the ascii renderer
without touching the player layer, and libmpv later if wim ever stops
treating mpv as a subprocess for other reasons. whatever replaces
`FramePump` should keep a surface narrow enough that the source is a
swap.

**`screenshot-to-file` plus `vf=scale`** is the fallback, recorded
because the number in this file used to be wrong. with the scale filter
in front of it the per-frame round trip is 35-41ms median rather than
113ms (480p) or 396ms (1080p) unscaled, so 24-29fps rather than 2-9fps.
still not it: every frame is a blocking ipc round trip competing with
wim's own property traffic, plus a png encode, a file write and a file
read for data wim already had in raw form a step earlier.

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
everything else: another protocol, halfblocks, a probe that has not
answered yet, or one that was never run at all (`graphics_plausible`
skips it on alacritty, so alacritty lands on tct without asking).

note that `Picker::from_query_stdio()` returning `Ok` does not mean a
real protocol exists; on `NoCap` it hands back a `Halfblocks` picker
with a made-up font size, so `picker.is_some()` is not a proxy for "this
terminal does graphics" and `protocol_type()` has to be asked. see
`video::drawing_vo`.

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

1. ~~**wim repaints every cell it owns, every frame.**~~ **this was
   wrong, and it was the item called "the real work".** the claim was
   that ratatui's diff would "restore" the cells mpv drew into. it
   cannot: `Terminal::flush` is

   ```rust
   let previous_buffer = &self.buffers[1 - self.current];
   let current_buffer = &self.buffers[self.current];
   let updates = previous_buffer.diff(current_buffer);
   self.backend.draw(updates.into_iter())
   ```

   a diff of **previous buffer against current buffer**. ratatui has no
   knowledge of what is actually on the terminal, so it cannot restore
   anything it has not been told to draw. a rect left untouched in two
   consecutive frames holds identical cells in both buffers, the diff
   yields nothing for it, and whatever mpv painted there is never
   written over. no fork, no patch, nothing to teach ratatui.

   `video::Hole` is this, with the mechanism pinned by tests against
   ratatui's own `Buffer::diff` rather than argued: an untouched hole
   emits nothing on the next frame, the frame that *opens* the hole
   still clears what was there (which is correct, mpv draws after), and
   a hole hanging off the edge of the buffer clips instead of
   panicking.

   what is actually required turns out to be the unglamorous half:
   wim's render path must **not draw in the rect** (a layout change,
   not a ratatui change), and something must notice when that promise
   is broken so mpv can be asked to redraw. `Hole::painted` is that
   notice. the cases that break it are an overlay or toast landing in
   the hole, and the full repaint after a resize; neither can be
   repaired from wim's side, so the answer is always "ask mpv to draw
   again once the thing covering it has gone".
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
