# raster

`src/ui/raster.rs` turns any image into cells. album art uses it, and
the video pane (`src/framesource.rs`) uses the same code, which is why
nothing in it knows what a cover is.

## the technique

one cell carries four luma samples, not one. the glyph comes from the
sixteen 2x2 quadrant blocks (U+2580..U+259F, `QUADRANTS`), picked by
which of the cell's four samples land on the bright side of their own
midpoint; the foreground shade is the average of those and the
background the average of the rest. so a `w x h` cell rect samples a
`2w x 2h` grid and gets twice the resolution on *both* axes that a
full block would give for the same cells.

this started as a half-block rasterizer (`▀`, U+2580, two samples per
cell) and `HALF_BLOCK` and the `0b0011` entry of the table are still
that glyph; the quadrant set is a superset, so a font that could render
the old output can render this.

## tone

under `Tone::Mono` every colour is `theme::shade(level)`. the module
memoizes `shade` over its 256 possible inputs (`shades()`) rather than
calling it twice per cell, so a 400x200 grid does not pay 160000 float
conversions; the palette is still entirely `shade`'s.

under `Tone::Color` (the album-art `ascii-color` mode, the one
sanctioned exception to the monochrome rule) the fill loop accumulates
three channels into a parallel rgb grid and `paint` emits each cell's
two groups as rgb instead. the glyph is unchanged: it still comes from
the luma grid, and the colours are grouped by the same mask, so colour
costs none of the 2x horizontal resolution and the glyph and the two
colours always describe one split. luma in colour mode is derived from
the averaged rgb rather than accumulated beside it; `rgb_luma` is linear
in the channels, so it is the same number the mono path computes and the
two grids cannot disagree.

the sharpen pass stays luma-only in colour mode too: luma is what picks
the glyph, so sharpening it steepens the structure, while leaving rgb
the plain box average keeps hues off the unsharp mask's halos.

a mono rasterization allocates no rgb grid at all, and a grey source
asked for in colour fills it with the grey rather than walking the
three-channel path for three times the reads.

## fit

- `Fit::Stretch` fills the rect, distorting the image if the rect's
  physical shape disagrees with it.
- `Fit::Contain` keeps the image's aspect against the rect's *physical*
  shape (a cell is `cell_aspect` times taller than wide, see
  `main::cell_aspect`), centres it, and leaves the rest at luma 0 (and
  rgb `[0, 0, 0]`), i.e. a black bar.

`Fit` and `Tone` travel together as a `Style`, so `Memo::render` stays
inside clippy's argument limit and the memo invalidates on either.

album art uses `Contain`, where it is usually a no-op: `browse::layout`
and `ui::player::layout` already size the art rect square against the
same `cell_aspect`, so a square cover fills it. it only shows up for a
cover that is not square, which used to be stretched to square
instead.

the video pane uses `Stretch`, and that is not a distortion: mpv is
asked for a frame exactly `2cols x 2rows` pixels with its own letterbox
already baked in (`framesource::Grid`), so stretching is a 1:1 copy and
the rasterizer resamples nothing.

## the unsharp pass

each output sample is a box average over its whole source span, which
lands softer than the picture is; `Raster::sharpen` is a four-neighbour
unsharp mask that puts the definition back. the strength scales
inversely with the grid's span (`sharpen_amount`), because a coarser
grid cut more detail, and is clamped at both ends: past `SHARPEN_MAX`
it stops adding definition and starts drawing a bright rim along the
dark side of every edge. uniform input is a fixed point, so flat areas
of a cover stay flat however hard the dial is turned; that is what
keeps the result pixelated rather than noisy.

a 1:1 source has no box average to compensate for, and there the pass
is pure cost: it is ~70% of what such a resample costs (measured
below), because it walks the whole grid twice while the copy walks it
once. `Sharpen::Off` turns it off, and it is a caller's argument, never
inferred from the geometry, because off is a visibly different picture.
`Sharpen` is part of `Memo`'s key (as `Memo::set_sharpen`, since
`Memo::render` is already at clippy's argument limit), so flipping it
resamples rather than handing back the other setting's grid.

## why it is fast

the old sampler cost ~7 ms for a 640px cover into a 40x20 cell panel
and ~36 ms for a 1080p frame into 400x200 cells, every frame, because
it averaged the full-resolution source span per cell through
`DynamicImage::get_pixel` (an enum match, a bounds check and an `Rgba`
conversion per pixel). five things fixed that:

- the source is read as one contiguous `&[u8]` with a known stride and
  step (`Pixels`), so a sample is a slice index. 8-bit luma/lumaA/rgb/
  rgba are read in place; 16-bit and float images are normalized once,
  and covers are normalized to rgb8 at decode so that path is never
  hot.
- luma is integer: weights 54/183/19 summing to 256, so the divide is a
  shift and a neutral pixel maps exactly to itself.
- the inner loop takes the source row as one slice and the taps as a
  chunk of the offset table, so it carries one bounds check per tap
  rather than one per channel, and the gray/colour branch is hoisted
  out of it (that alone was 1.9x on the 400x200 case).
- the taps per output sample are capped at `MAX_TAPS` (4) per axis. at
  or below that the box average over the sample's source span is exact;
  past it the taps are spread evenly across the span. the work is
  therefore `O(output)` with a bounded constant, not `O(source)`.
- the horizontal tap count is a const generic (`fill_taps::<TX>`, four
  instantiations, picked once per resample), so the innermost loop has
  a compile-time trip count and unrolls instead of branching per tap.
- one output sample is summed in registers and written once. the
  earlier shape walked a `Vec` row accumulator, read-modify-writing a
  slot per tap, and that dependency through memory was most of a tap's
  cost; there is no accumulator in `Raster` any more.
- the luma grid, the tap table, the pre-sharpen copy and the rgb grid
  live in a `Raster` that callers keep (`with_scratch` holds the render
  thread's one), so a steady frame rate allocates nothing. a resize is
  the only event that reallocates.
- a cell's glyph is `Clone`d out of a `static` array of prebuilt
  `Cell`s (`QUADRANT_CELLS`) rather than written with
  `Cell::set_symbol`. that call rebuilds the cell's `CompactString`
  from a `&str` of runtime length, which is a 24-byte zero-init plus a
  `memcpy` call: 8.6 of `paint`'s 13.8 ns/cell. cloning an inline
  `CompactString` is a flat 24-byte copy, 1.4 ns/cell, and `put`
  carries over the fields a glyph write does not touch (the modifier,
  the underline colour, `skip`) so the painted cell is identical. the
  array has to be a `static`: a `const` of a type with a `Drop` impl
  is materialized at every use site, and that version was *slower*
  than `set_symbol` (24 ns/cell, since each call built all sixteen
  cells on the stack and dropped fifteen).

## numbers

all measured on this machine (8 cores, load average ~2.5 from the
other work on it), so they are machine-specific and noisy to roughly
±15%; the benches report the fastest of several batches, not the mean,
for exactly that reason, and anything compared against a previous
implementation is run with the batches *alternating* in one process,
because back-to-back runs of the same binary came out 1.6x apart while
other builds were running.

`cargo test --release --lib -- --ignored --nocapture --test-threads=1
raster_cost legacy_raster_cost`:

| case | before | after | after, memoized |
| --- | --- | --- | --- |
| 640px cover -> 40x20 cells | 2.8 ms | 0.098 ms | 0.006 ms |
| 640px cover -> 120x60 cells | | 0.76 ms | 0.054 ms |
| 640px cover -> 400x200 cells | 5.3 ms | 4.1 ms | 0.63 ms |
| 1080p frame -> 400x200 cells | 16.6 ms | 5.9 ms | 0.64 ms |

the "before" column is the pre-extraction sampler, kept verbatim in
`legacy_raster_cost`. it is not quite a like-for-like: it sampled the
half-block grid, i.e. half as many samples as the quadrant one, so the
"after" column is doing twice the sampling. it also scales with the
*source* rather than the output, which is why a 1500px cover costs 16.2
ms into the same 40x20 panel that a 640px one costs 2.8 ms in, while
"after" does not move at all between them.

the 400x200 figures are the extreme (the font zoomed way out). most of
what is left there is painting 80000 cells, which is also what the
"memoized" column still pays; the terminal write behind it dwarfs
both.

`paint` per cell, which is the floor a memo cannot skip
(`paint_cost`, `probe_paint_floor`):

| | per cell | of which |
| --- | --- | --- |
| mono | 7.8 ns | cell math 5.8, glyph+colours 1.9 |
| colour | 16.9 ns | the rgb grouping is the difference |

the glyph write was 8.9 of a mono cell's 13.8 ns before
`QUADRANT_CELLS`; it is 1.4 now, which is 1.8x on `paint` as a whole
and 1.4x in colour. the cell math (`quantize`: a midpoint, four
compares and two averages per cell) is now 73% of a mono cell and the
obvious next thing to look at.

`sample`, for a 640px cover into 70x35 cells (`probe_paint_floor`):
282 µs, of which the source walk is 220 and the unsharp pass 60. the
walk is 1.28x the same loop with a runtime tap count and 1.38x the
row-accumulator shape that preceded it. for the *video* shape, where
the source is already the grid's size, the same 70x35 resample is 88 µs
with the pass on and 27 µs with it off.

and the whole now-playing frame, which is what any of this is for
(`cargo test --release --lib -- --ignored --nocapture
--test-threads=1 ui::player::bench`):

| frame | total | art | text | transport |
| --- | --- | --- | --- | --- |
| 80x24 | 16.4 µs | 2.7 | 9.7 | 3.9 |
| 120x40 | 20.8 µs | 8.4 | 9.3 | 3.1 |
| 200x60 | 31.9 µs | 19.5 | 9.2 | 3.1 |

the art column is a memo hit, i.e. `paint` alone: at 200x60 it is 12000
cells and still the largest single piece of a frame.

## the per-frame cache

`Raster::sample` is the expensive half and `Raster::paint` is the half
that cannot be skipped (ratatui rebuilds the buffer every frame).
`raster::Memo` skips the sampling when nothing it depends on changed.

the memo key is `(key, rect width, rect height, cell_aspect bits,
style, sharpen)`, i.e. every input a sampling has. `key` has to name
the **image**, not the track and not the pane:
that is the entire correctness argument, since two different pictures
sharing a key would show one in place of the other. callers:

- the player passes `coverart::art_key(playing track)`, the same name
  the `ratatui-image` protocol cache is already rebuilt on.
- the browse view passes `BrowseView::art_key()`: kind, artist, title
  and year of the view it was opened on, which is strictly finer than
  `coverart::Key` (that one already treats one artist's two same-titled
  albums as one record).
- the video pane uses the unkeyed `with_scratch` path instead, because
  its source changes every frame anyway.

invalidation, in full:

- art changes (a different album, or a cover arriving where there was
  none): the key changes.
- resize, or the art panel being re-laid-out to a different size: the
  width/height change.
- the terminal reporting a new cell aspect: the aspect bits change.
- a new fit, a new tone, or the unsharp pass being turned on or off:
  the style or the sharpen setting changes.
- nothing else. an entry lives indefinitely while all of the above hold
  steady, which is sound because while they hold steady there is
  nothing new to compute.

the one thing the memo cannot see is a *new image under an existing
key at the same size*; `a_new_cover_under_the_same_key_is_the_one_thing_the_memo_cannot_see`
pins that down as the caller's contract rather than leaving it implicit.

## decoded cover memory

`coverart::decode_cover` bounds a decoded cover to 640px on its long
edge and normalizes it to rgb8. 640 covers both consumers at any
sensible size (the quadrant grid samples `2 cols x 2 rows`, so 2x the
cell grid on each axis; the `ratatui-image` path draws into an art
panel a few hundred pixels across) while a full-resolution cover is far
past it. a square art panel is at most half the terminal's height
across, so 640 is short of the grid only past ~160 cell rows of art,
where the picture is already a wall of blocks. the 24-entry in-memory
bound therefore holds a predictable amount:

| cover | decoded as fetched | 24 of them | bounded to 640px | 24 of them |
| --- | --- | --- | --- | --- |
| 600px | 1 MB | 24 MB | 1 MB | 24 MB |
| 1000px | 2 MB | 68 MB | 1 MB | 28 MB |
| 1500px | 6 MB | 154 MB | 1 MB | 28 MB |
| 3000px | 25 MB | 617 MB | 1 MB | 28 MB |

the downscale is not free: on a 1500px cover it adds ~34 ms to a ~6 ms
decode (`thumbnail`; `resize(Triangle)` was ~95 ms and CatmullRom ~144
ms for detail the sample grid flattens anyway). that cost is paid
once per cover, on a fetch thread, which is also why `Cache::tick`
prefetches the next queued track: the alternative is paying it at the
moment the song starts.

(`cargo test --release -- --ignored --nocapture cover_memory`.)
