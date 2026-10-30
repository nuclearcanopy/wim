# raster

`src/ui/raster.rs` turns any image into cells. album art uses it; an
ascii video pane is meant to use the same code, which is why nothing in
it knows what a cover is.

## the technique

one cell carries two luma samples, not one. the glyph is the upper half
block `▀` (U+2580): the foreground paints the top half of the cell, the
background the bottom. so a `w x h` cell rect samples a `w x 2h` grid
and gets twice the vertical resolution a full block would give for the
same cells.

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

album art and the video pane both use `Contain`. for album art this is
usually a no-op: `browse::layout` and `ui::player::layout` already size
the art rect square against the same `cell_aspect`, so a square cover
fills it. it only shows up for a cover that is not square, which used
to be stretched to square instead.

`Stretch` is kept because it is what the pre-extraction sampler did, so
the old look is one argument away.

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
- the inner loop takes the source row as one slice and the taps as
  `chunks_exact`, so it carries one bounds check per tap rather than
  one per channel, and the gray/colour branch is hoisted out of it
  (that alone was 1.9x on the 400x200 case).
- the taps per output sample are capped at `MAX_TAPS` (4) per axis. at
  or below that the box average over the sample's source span is exact;
  past it the taps are spread evenly across the span. the work is
  therefore `O(output)` with a bounded constant, not `O(source)`.
- the luma grid, the tap table and the row accumulator live in a
  `Raster` that callers keep (`with_scratch` holds the render thread's
  one), so a steady frame rate allocates nothing. a resize is the only
  event that reallocates.

measured on this machine, 8 cores at load average ~11 from concurrent
builds, so these are pessimistic and noisy to roughly ±30%
(`cargo test --release -- --ignored --nocapture --test-threads=1
raster_cost legacy_raster_cost`; the bench reports the fastest of five
batches, not the mean, for exactly that reason):

| case | before | after | after, memoized |
| --- | --- | --- | --- |
| 640px cover -> 40x20 cells | 7.0 ms | 0.12 ms | 0.015 ms |
| 1500px cover -> 40x20 cells | 37.3 ms | 0.12 ms | 0.015 ms |
| 640px cover -> 400x200 cells | 11.3 ms | 4.2 ms | 1.7 ms |
| 1080p frame -> 400x200 cells | 35.9 ms | 7.1 ms | 1.6 ms |

the "before" column at 40x20 cells scales with the *source*, which is
why a 1500px cover cost 5x what a 640px one did for the same panel;
"after" is bounded by the output, so it does not move.

the 400x200 figures are the extreme (the font zoomed way out). most of
what is left there is painting 80000 cells, which is also what the
"memoized" column still pays; the terminal write behind it dwarfs
both.

## the per-frame cache

`Raster::sample` is the expensive half and `Raster::paint` is the half
that cannot be skipped (ratatui rebuilds the buffer every frame).
`raster::Memo` skips the sampling when nothing it depends on changed.

the memo key is `(key, rect width, rect height, cell_aspect bits,
fit)`. `key` has to name the **image**, not the track and not the pane:
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
- nothing else. an entry lives indefinitely while all of the above hold
  steady, which is sound because while they hold steady there is
  nothing new to compute.

the one thing the memo cannot see is a *new image under an existing
key at the same size*; `a_new_cover_under_the_same_key_is_the_one_thing_the_memo_cannot_see`
pins that down as the caller's contract rather than leaving it implicit.

## decoded cover memory

`coverart::decode_cover` bounds a decoded cover to 640px on its long
edge and normalizes it to rgb8. 640 is comfortably above both consumers
(the half-block grid samples at most `cols x 2 rows`, so 400x400 even
at a 400x200 cell grid; the `ratatui-image` path draws into an art
panel a few hundred pixels across) while a full-resolution cover is far
past it. the 24-entry in-memory bound therefore holds a predictable
amount:

| cover | decoded as fetched | 24 of them | bounded to 640px | 24 of them |
| --- | --- | --- | --- | --- |
| 600px | 1 MB | 24 MB | 1 MB | 24 MB |
| 1000px | 2 MB | 68 MB | 1 MB | 28 MB |
| 1500px | 6 MB | 154 MB | 1 MB | 28 MB |
| 3000px | 25 MB | 617 MB | 1 MB | 28 MB |

the downscale is not free: on a 1500px cover it adds ~34 ms to a ~6 ms
decode (`thumbnail`; `resize(Triangle)` was ~95 ms and CatmullRom ~144
ms for detail the half-block grid flattens anyway). that cost is paid
once per cover, on a fetch thread, which is also why `Cache::tick`
prefetches the next queued track: the alternative is paying it at the
moment the song starts.

(`cargo test --release -- --ignored --nocapture cover_memory`.)
