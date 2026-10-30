//! Image to half-block cells: the one rasterizer behind album art and
//! anything else that paints a picture in a cell grid.
//!
//! The upper half block (U+2580) carries two luma samples per cell,
//! foreground for the top sub-row and background for the bottom, so a
//! `w x h` cell rect samples a `w x 2h` grid: twice the vertical
//! resolution a full block gives for the same cells.
//!
//! Nothing here assumes the source is a static cover, and it is built
//! to stand up at video frame rates over a big grid: the source is
//! read as one contiguous slice rather than through `get_pixel`, the
//! luma grid and the tap table are kept and resized in place rather
//! than allocated per call, and the work per output sample is bounded
//! instead of averaging the full-resolution source per cell. See
//! docs/raster.md.

use std::cell::RefCell;
use std::sync::OnceLock;

use image::DynamicImage;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Color;

use crate::theme::shade;

/// U+2580, the glyph the whole module exists for.
pub const HALF_BLOCK: char = '▀';

/// Source samples per axis per output sample. Caps the cost at 16
/// reads per half-cell however large the source is; at or below this
/// the box average over the sample's source span is exact.
const MAX_TAPS: u32 = 4;

/// How the image is mapped onto the cell rect.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Fit {
    /// Fill the rect exactly, distorting the image if the rect's
    /// physical shape disagrees with it. The grid is the image, with
    /// no bars; what album art did before `Contain` existed.
    Stretch,
    /// Keep the image's aspect ratio against the rect's *physical*
    /// shape (a cell is `cell_aspect` times taller than it is wide),
    /// centring it and leaving the remainder black. What a video pane
    /// wants, and what a cover that is not exactly square wants.
    Contain,
}

/// A contiguous read of a source image's samples. Rasterizing is then
/// slice indexing rather than one `DynamicImage::get_pixel` per pixel
/// (an enum match, a bounds check and an `Rgba` conversion each),
/// which is what made the old sampler cost milliseconds per frame.
struct Pixels<'a> {
    data: &'a [u8],
    w: u32,
    h: u32,
    stride: usize,
    step: usize,
    /// One meaningful channel: the byte *is* the luma, unweighted.
    gray: bool,
}

impl<'a> Pixels<'a> {
    /// `None` for the variants that are not 8 bits per channel
    /// (16-bit and float images); `Raster::sample` normalizes those.
    fn of(img: &'a DynamicImage) -> Option<Pixels<'a>> {
        let (data, step, gray) = match img {
            DynamicImage::ImageLuma8(b) => (b.as_raw().as_slice(), 1, true),
            DynamicImage::ImageLumaA8(b) => (b.as_raw().as_slice(), 2, true),
            DynamicImage::ImageRgb8(b) => (b.as_raw().as_slice(), 3, false),
            DynamicImage::ImageRgba8(b) => (b.as_raw().as_slice(), 4, false),
            _ => return None,
        };
        Pixels::new(data, img.width(), img.height(), step, gray)
    }

    fn new(data: &'a [u8], w: u32, h: u32, step: usize, gray: bool) -> Option<Pixels<'a>> {
        if w == 0 || h == 0 {
            return None;
        }
        let stride = w as usize * step;
        // a buffer shorter than its own dimensions claim would make
        // every index below a guess; refuse it instead.
        if data.len() < stride.checked_mul(h as usize)? {
            return None;
        }
        Some(Pixels {
            data,
            w,
            h,
            stride,
            step,
            gray,
        })
    }
}

/// Luma `0..=255` of one rgb pixel. Integer weights summing to 256, so
/// the divide is a shift and a neutral pixel maps exactly to itself;
/// the drift from the exact 0.2126/0.7152/0.0722 coefficients is under
/// half a level, invisible at 256 shades.
#[inline]
fn rgb_luma(p: &[u8]) -> u32 {
    (54 * p[0] as u32 + 183 * p[1] as u32 + 19 * p[2] as u32) >> 8
}

/// A reusable half-block rasterizer. Owns its scratch buffers, so
/// rasterizing the same grid again (the next video frame, the next
/// render of a cover) allocates nothing.
#[derive(Debug, Default)]
pub struct Raster {
    /// Luma per half-cell, row-major over sub-rows, `cols * rows * 2`
    /// long. Sub-row `2r` is cell row `r`'s foreground, `2r + 1` its
    /// background.
    luma: Vec<u8>,
    cols: u16,
    rows: u16,
    /// Source byte offset-within-row per (output column, tap), built
    /// once per `sample` so the inner loop is a read and an add.
    xs: Vec<u32>,
    /// One output row's running sums, so the source is walked row by
    /// row instead of re-entered per output sample.
    acc: Vec<u32>,
}

impl Raster {
    pub fn new() -> Self {
        Self::default()
    }

    /// Samples `img` into the half-cell grid for an `area`-shaped cell
    /// rect. `area`'s position is ignored here (only `paint` needs
    /// it), so the same sampling is reusable wherever it lands.
    pub fn sample(&mut self, img: &DynamicImage, area: Rect, cell_aspect: f32, fit: Fit) {
        self.cols = area.width;
        self.rows = area.height;
        let halves = area.width as usize * area.height as usize * 2;
        self.luma.clear();
        self.luma.resize(halves, 0);
        if halves == 0 {
            return;
        }
        match Pixels::of(img) {
            Some(px) => self.fill(&px, cell_aspect, fit),
            None => {
                // not 8 bits per channel (a 16-bit or float png).
                // converted whole once here, which does allocate,
                // rather than reading the source through `get_pixel`
                // per sample. covers are normalized to rgb8 at decode
                // (`coverart::decode_cover`) and mpv hands over rgb8,
                // so this is the exotic path, never the hot one.
                let lum = img.to_luma8();
                let (w, h) = lum.dimensions();
                if let Some(px) = Pixels::new(lum.as_raw(), w, h, 1, true) {
                    self.fill(&px, cell_aspect, fit);
                }
            }
        }
    }

    fn fill(&mut self, px: &Pixels, cell_aspect: f32, fit: Fit) {
        // field-wise borrows: the inner loop reads `xs` while writing
        // `acc`, which it cannot do through `self`.
        let Self {
            luma,
            cols,
            rows,
            xs,
            acc,
        } = self;
        let grid_w = *cols as u32;
        let grid_h = *rows as u32 * 2;
        let (ow, oh, off_x, off_y) = place(px.w, px.h, grid_w, grid_h, cell_aspect, fit);
        if ow == 0 || oh == 0 {
            return;
        }

        // an exact box average while an output sample covers few
        // source pixels, a bounded even subsample past that.
        let taps_x = px.w.div_ceil(ow).clamp(1, MAX_TAPS);
        let taps_y = px.h.div_ceil(oh).clamp(1, MAX_TAPS);

        xs.clear();
        xs.reserve(ow as usize * taps_x as usize);
        for ox in 0..ow {
            let (s0, s1) = span(ox, ow, px.w);
            for t in 0..taps_x {
                let x = (s0 + (s1 - s0) * t / taps_x).min(px.w - 1);
                xs.push(x * px.step as u32);
            }
        }

        acc.clear();
        acc.resize(ow as usize, 0);
        let count = taps_x * taps_y;
        let taps_x = taps_x as usize;

        for oy in 0..oh {
            let (s0, s1) = span(oy, oh, px.h);
            acc.fill(0);
            for t in 0..taps_y {
                let y = (s0 + (s1 - s0) * t / taps_y).min(px.h - 1);
                // the row as one slice and the taps as fixed-size
                // chunks, so the whole inner loop carries one bounds
                // check per tap instead of one per channel, and the
                // gray/colour branch is hoisted out of it entirely.
                let row = &px.data[y as usize * px.stride..][..px.stride];
                let taps = xs.chunks_exact(taps_x);
                if px.gray {
                    for (slot, taps) in acc.iter_mut().zip(taps) {
                        for &dx in taps {
                            *slot += row[dx as usize] as u32;
                        }
                    }
                } else {
                    for (slot, taps) in acc.iter_mut().zip(taps) {
                        for &dx in taps {
                            *slot += rgb_luma(&row[dx as usize..dx as usize + 3]);
                        }
                    }
                }
            }
            let base = (off_y + oy) as usize * grid_w as usize + off_x as usize;
            for (ox, &sum) in acc.iter().enumerate() {
                luma[base + ox] = (sum / count) as u8;
            }
        }
    }

    /// Paints the last `sample` into `buf` at `area`, clipped to the
    /// buffer. Every colour is a `theme::shade` level; nothing here
    /// can introduce a hue.
    pub fn paint(&self, area: Rect, buf: &mut Buffer) {
        let vis = area.intersection(buf.area);
        if vis.is_empty() || self.cols == 0 || self.rows == 0 {
            return;
        }
        let shades = shades();
        let grid_w = self.cols as usize;
        let buf_w = buf.area.width as usize;
        let first_col = (vis.x - area.x) as usize;
        for y in vis.y..vis.y + vis.height {
            let cell_row = (y - area.y) as usize;
            if cell_row >= self.rows as usize {
                break;
            }
            let top = cell_row * 2 * grid_w;
            let bottom = top + grid_w;
            let start = (y - buf.area.y) as usize * buf_w + (vis.x - buf.area.x) as usize;
            let row = &mut buf.content[start..start + vis.width as usize];
            for (i, cell) in row.iter_mut().enumerate() {
                let col = first_col + i;
                if col >= grid_w {
                    break;
                }
                cell.set_char(HALF_BLOCK);
                cell.set_fg(shades[self.luma[top + col] as usize]);
                cell.set_bg(shades[self.luma[bottom + col] as usize]);
            }
        }
    }

    /// `sample` then `paint`: the whole job, one call per frame.
    pub fn render(
        &mut self,
        img: &DynamicImage,
        area: Rect,
        cell_aspect: f32,
        fit: Fit,
        buf: &mut Buffer,
    ) {
        self.sample(img, area, cell_aspect, fit);
        self.paint(area, buf);
    }
}

/// Reading the grid back without a `Buffer`: what the tests assert
/// against, and the API an ascii video renderer wants if it ever needs
/// the samples rather than the cells. Nothing on the render path calls
/// these yet, hence the blanket allow (same documented gap as
/// `cache::play_count`).
#[allow(dead_code)]
impl Raster {
    /// Cell columns the last `sample` was sized for.
    pub fn cols(&self) -> u16 {
        self.cols
    }

    /// Cell rows the last `sample` was sized for; the luma grid is
    /// twice this tall.
    pub fn rows(&self) -> u16 {
        self.rows
    }

    /// The whole output: `cols() * rows() * 2` luma levels, row-major
    /// over sub-rows.
    pub fn luma(&self) -> &[u8] {
        &self.luma
    }

    /// One half-cell's luma; `sub_row` runs `0..rows() * 2`, and
    /// anything outside the grid reads as 0 rather than panicking.
    pub fn at(&self, col: u16, sub_row: u16) -> u8 {
        if col >= self.cols || sub_row >= self.rows.saturating_mul(2) {
            return 0;
        }
        self.luma[sub_row as usize * self.cols as usize + col as usize]
    }
}

/// The half-open source span `[s0, s1)` that output sample `i` of `n`
/// covers along a `len`-long axis, never empty. 64-bit throughout: a
/// 65535-wide source times its own index overflows 32 bits, and the
/// old sampler's `clamp(s0 + 1, len)` could be handed `min > max` and
/// panic outright.
fn span(i: u32, n: u32, len: u32) -> (u32, u32) {
    let s0 = (i as u64 * len as u64 / n as u64) as u32;
    let s1 = (((i as u64 + 1) * len as u64 / n as u64) as u32).min(len);
    (s0, s1.max(s0 + 1).min(len))
}

/// Where the image lands in the half-cell grid: drawn size in grid
/// units plus the offset that centres it. Whatever is left over stays
/// at luma zero, i.e. a black bar, the one part of the grid that is
/// not the image.
fn place(
    iw: u32,
    ih: u32,
    grid_w: u32,
    grid_h: u32,
    cell_aspect: f32,
    fit: Fit,
) -> (u32, u32, u32, u32) {
    if fit == Fit::Stretch || iw == 0 || ih == 0 || grid_w == 0 || grid_h == 0 {
        return (grid_w, grid_h, 0, 0);
    }
    // the rect's physical shape measured in cell widths: `grid_w`
    // across, and `grid_h` half-cells down at `cell_aspect / 2` cell
    // widths each.
    let aspect = if cell_aspect.is_finite() {
        cell_aspect.clamp(0.25, 8.0)
    } else {
        2.0
    };
    let phys_w = grid_w as f32;
    let phys_h = grid_h as f32 * aspect / 2.0;
    let want = iw as f32 / ih as f32;
    let (draw_w, draw_h) = if phys_w / phys_h > want {
        (phys_h * want, phys_h)
    } else {
        (phys_w, phys_w / want)
    };
    let ow = (draw_w.round() as u32).clamp(1, grid_w);
    let oh = ((draw_h * 2.0 / aspect).round() as u32).clamp(1, grid_h);
    (ow, oh, (grid_w - ow) / 2, (grid_h - oh) / 2)
}

/// `theme::shade` memoized over the 256 luma levels. Every colour the
/// module can produce still comes out of `shade`; this only stops a
/// 400x200 grid paying for the float conversion 160000 times twice.
fn shades() -> &'static [Color; 256] {
    static SHADES: OnceLock<[Color; 256]> = OnceLock::new();
    SHADES.get_or_init(|| std::array::from_fn(|i| shade(i as f32 / 255.0)))
}

/// Everything a sampling depends on. Two renders agreeing on all of
/// it produce byte-identical grids, which is the entire basis for
/// `Memo` skipping the second one.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Sampled {
    /// Names the *image*. Not the track, not the rect: see `Memo`.
    key: String,
    /// The rect's size only. The same grid moved elsewhere on screen
    /// is the same rasterization, so a pane sliding sideways is not a
    /// resample.
    width: u16,
    height: u16,
    /// Bitwise, since this is an identity check and not a measurement.
    aspect: u32,
    fit: Fit,
}

/// A `Raster` plus what it was last sampled from, so a picture that
/// has not changed is not resampled every frame. Painting still
/// happens every frame (the buffer is rebuilt each one); only the
/// sampling, which is the expensive half, is skipped.
///
/// `key` has to name the *image*. That is the whole correctness
/// argument: the grid handed back belongs to whatever was last
/// sampled under that key, so two different pictures sharing a key
/// would show one in place of the other. Album art passes
/// `coverart::art_key`, the same name the `ratatui-image` protocol
/// cache is already invalidated by.
///
/// Invalidated by any change to the key, the target size, the cell
/// aspect or the fit. Worst-case staleness is unbounded in *time* and
/// zero in content: an entry lives until one of those changes, and
/// while none of them has changed there is nothing new to compute.
#[derive(Debug, Default)]
pub struct Memo {
    raster: Raster,
    /// `None` before the first sample, i.e. always a miss.
    last: Option<Sampled>,
}

impl Memo {
    /// Samples `img` only if `key`, the rect's size, `cell_aspect` or
    /// `fit` differ from the last call, then paints. Returns whether
    /// it had to resample, which is what the tests assert on.
    pub fn render(
        &mut self,
        key: &str,
        img: &DynamicImage,
        area: Rect,
        cell_aspect: f32,
        fit: Fit,
        buf: &mut Buffer,
    ) -> bool {
        let aspect = cell_aspect.to_bits();
        // compared field by field rather than against a built
        // `Sampled`, so a hit (the common case, every frame) does not
        // allocate a copy of the key just to throw it away.
        let resampled = !self.last.as_ref().is_some_and(|l| {
            l.key == key
                && l.width == area.width
                && l.height == area.height
                && l.aspect == aspect
                && l.fit == fit
        });
        if resampled {
            self.raster.sample(img, area, cell_aspect, fit);
            self.last = Some(Sampled {
                key: key.to_string(),
                width: area.width,
                height: area.height,
                aspect,
                fit,
            });
        }
        self.raster.paint(area, buf);
        resampled
    }
}

thread_local! {
    /// The render path's one unkeyed `Raster`, for a source that
    /// changes every frame anyway (a video pane). ratatui draws on
    /// the thread that owns the terminal, so a thread-local is
    /// exactly as shared as the draw call is; keeping it is what makes
    /// a steady frame rate allocation-free, with a resize the only
    /// event that reallocates.
    static SCRATCH: RefCell<Raster> = RefCell::new(Raster::new());
    /// The render path's one `Memo`, for static pictures. One slot is
    /// enough because the art panes are mutually exclusive branches
    /// of the workspace: at most one of them draws per frame.
    static MEMO: RefCell<Memo> = RefCell::new(Memo::default());
}

/// Runs `f` with the calling thread's kept `Raster`. Not reentrant:
/// `f` must not call back into this (nothing in a render path does).
pub fn with_scratch<R>(f: impl FnOnce(&mut Raster) -> R) -> R {
    SCRATCH.with(|r| f(&mut r.borrow_mut()))
}

/// Runs `f` with the calling thread's kept `Memo`. Same reentrancy
/// rule as `with_scratch`.
pub fn with_memo<R>(f: impl FnOnce(&mut Memo) -> R) -> R {
    MEMO.with(|m| f(&mut m.borrow_mut()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::testbuf::{assert_outside_untouched, buffer, char_at};
    use image::{Rgb, RgbImage};

    /// Half white on top, half black underneath, `h` rows tall.
    fn split(w: u32, h: u32) -> DynamicImage {
        let mut img = RgbImage::new(w, h);
        for y in 0..h {
            let v = if y < h / 2 { 255 } else { 0 };
            for x in 0..w {
                img.put_pixel(x, y, Rgb([v, v, v]));
            }
        }
        DynamicImage::ImageRgb8(img)
    }

    fn flat(w: u32, h: u32, v: u8) -> DynamicImage {
        DynamicImage::ImageRgb8(RgbImage::from_pixel(w, h, Rgb([v, v, v])))
    }

    /// A left-to-right ramp, for checking the horizontal mapping.
    fn ramp(w: u32, h: u32) -> DynamicImage {
        let mut img = RgbImage::new(w, h);
        for y in 0..h {
            for x in 0..w {
                let v = (x * 255 / (w - 1).max(1)) as u8;
                img.put_pixel(x, y, Rgb([v, v, v]));
            }
        }
        DynamicImage::ImageRgb8(img)
    }

    fn levels(r: &Raster) -> Vec<u8> {
        r.luma().to_vec()
    }

    #[test]
    fn the_grid_is_twice_as_tall_as_the_cell_rect() {
        let mut r = Raster::new();
        r.sample(&flat(8, 8, 128), Rect::new(0, 0, 10, 4), 2.0, Fit::Stretch);
        assert_eq!((r.cols(), r.rows()), (10, 4));
        assert_eq!(
            levels(&r).len(),
            10 * 4 * 2,
            "one luma per half-cell, not per cell"
        );
    }

    #[test]
    fn a_half_white_half_black_image_fills_one_cell_as_two_sub_rows() {
        // the whole reason for the half block: a single cell carries
        // both halves of the picture, not their average.
        let mut r = Raster::new();
        r.sample(&split(4, 4), Rect::new(0, 0, 1, 1), 2.0, Fit::Stretch);
        assert_eq!(r.at(0, 0), 255, "top sub-row is the white half");
        assert_eq!(r.at(0, 1), 0, "bottom sub-row is the black half");
    }

    #[test]
    fn the_glyph_painted_is_the_upper_half_block_with_fg_top_bg_bottom() {
        let mut buf = buffer(3, 2);
        let mut r = Raster::new();
        r.render(
            &split(4, 4),
            Rect::new(0, 0, 3, 2),
            2.0,
            Fit::Stretch,
            &mut buf,
        );
        for y in 0..2 {
            for x in 0..3 {
                assert_eq!(char_at(&buf, x, y), HALF_BLOCK);
            }
        }
        // top cell row is entirely in the white half, bottom entirely
        // in the black one.
        assert_eq!(buf.cell((0, 0)).unwrap().fg, shade(1.0));
        assert_eq!(buf.cell((0, 1)).unwrap().bg, shade(0.0));
    }

    #[test]
    fn every_colour_it_can_produce_is_a_grey() {
        let mut buf = buffer(6, 3);
        let mut r = Raster::new();
        r.render(
            &ramp(64, 64),
            Rect::new(0, 0, 6, 3),
            2.0,
            Fit::Stretch,
            &mut buf,
        );
        for cell in buf.content.iter() {
            for c in [cell.fg, cell.bg] {
                match c {
                    Color::Rgb(a, b, d) => {
                        assert!(a == b && b == d, "hue leaked into the palette: {c:?}")
                    }
                    other => panic!("not a shade: {other:?}"),
                }
            }
        }
    }

    #[test]
    fn a_flat_image_samples_to_its_own_level_everywhere() {
        let mut r = Raster::new();
        r.sample(
            &flat(101, 37, 200),
            Rect::new(0, 0, 13, 7),
            2.0,
            Fit::Stretch,
        );
        for (i, &v) in levels(&r).iter().enumerate() {
            assert_eq!(v, 200, "half-cell {i} drifted off a flat source");
        }
    }

    #[test]
    fn stretch_uses_the_whole_grid_and_contain_centres_with_bars() {
        // a 4:1 image in a square-ish rect: stretch distorts it to
        // fill, contain keeps the shape and leaves bars.
        let wide = ramp(400, 100);
        let mut r = Raster::new();
        r.sample(&wide, Rect::new(0, 0, 20, 10), 2.0, Fit::Stretch);
        assert!(
            levels(&r).iter().any(|&v| v > 240),
            "stretch should reach the image's bright edge"
        );
        // the leftmost sample is the box average of the ramp's first
        // column of source pixels, so near-black rather than exactly 0.
        assert!(r.at(0, 0) < 10, "left edge read as {}", r.at(0, 0));

        r.sample(&wide, Rect::new(0, 0, 20, 10), 2.0, Fit::Contain);
        // grid is 20x20 half-cells, physically square at aspect 2; a
        // 4:1 image draws 20 wide and 5 tall, centred.
        for sub in 0..7 {
            assert_eq!(r.at(10, sub), 0, "top bar should be black");
        }
        assert!(
            r.at(19, 10) > 200,
            "the image itself should sit in the middle band"
        );
        for sub in 14..20 {
            assert_eq!(r.at(10, sub), 0, "bottom bar should be black");
        }
    }

    #[test]
    fn contain_leaves_a_square_image_in_a_square_rect_full_bleed() {
        // the album-art case: `browse::layout` already sized the rect
        // square, so letterboxing must not shrink a square cover.
        let mut r = Raster::new();
        r.sample(
            &flat(600, 600, 120),
            Rect::new(0, 0, 40, 20),
            2.0,
            Fit::Contain,
        );
        for (i, &v) in levels(&r).iter().enumerate() {
            assert_eq!(v, 120, "half-cell {i} got a bar it should not have");
        }
    }

    #[test]
    fn a_cell_aspect_other_than_two_neither_panics_nor_crops() {
        // regression: the old sampler sized its grid `height *
        // cell_aspect` tall but read `height * 2` sub-rows out of it,
        // so anything under 2.0 indexed past the source (and tripped
        // `clamp(min > max)`) and anything over 2.0 silently dropped
        // the bottom of the image.
        for aspect in [1.0f32, 1.4, 1.8, 2.0, 2.4, 3.0] {
            let mut r = Raster::new();
            r.sample(
                &split(64, 64),
                Rect::new(0, 0, 24, 12),
                aspect,
                Fit::Stretch,
            );
            let last = r.at(0, 23);
            assert_eq!(last, 0, "aspect {aspect} cropped the black half away");
            assert_eq!(r.at(0, 0), 255, "aspect {aspect} lost the white half");
        }
    }

    #[test]
    fn the_memo_resamples_once_and_then_stops() {
        let mut buf = buffer(8, 4);
        let rect = Rect::new(0, 0, 8, 4);
        let img = ramp(200, 200);
        let mut m = Memo::default();
        assert!(m.render("a:x", &img, rect, 2.0, Fit::Contain, &mut buf));
        for _ in 0..5 {
            assert!(
                !m.render("a:x", &img, rect, 2.0, Fit::Contain, &mut buf),
                "an unchanged cover in an unchanged rect was resampled"
            );
        }
    }

    #[test]
    fn the_memo_still_paints_on_a_hit() {
        // the sampling is what is skipped, never the drawing: the
        // buffer is rebuilt every frame, so a cached frame that drew
        // nothing would be a blank pane.
        let rect = Rect::new(0, 0, 6, 3);
        let img = flat(32, 32, 180);
        let mut m = Memo::default();
        let mut first = buffer(6, 3);
        m.render("a:x", &img, rect, 2.0, Fit::Contain, &mut first);
        let mut second = buffer(6, 3);
        assert!(!m.render("a:x", &img, rect, 2.0, Fit::Contain, &mut second));
        assert_eq!(first, second, "a cached frame drew something different");
    }

    #[test]
    fn the_memo_invalidates_on_a_new_cover_a_resize_or_a_new_aspect() {
        let mut buf = buffer(12, 6);
        let img = flat(32, 32, 100);
        let rect = Rect::new(0, 0, 8, 4);
        let mut m = Memo::default();
        m.render("a:x", &img, rect, 2.0, Fit::Contain, &mut buf);
        assert!(
            m.render("a:y", &img, rect, 2.0, Fit::Contain, &mut buf),
            "a different album must not reuse the last one's grid"
        );
        assert!(
            m.render(
                "a:y",
                &img,
                Rect::new(0, 0, 12, 6),
                2.0,
                Fit::Contain,
                &mut buf
            ),
            "a resize must resample"
        );
        let wider = Rect::new(0, 0, 12, 6);
        assert!(
            m.render("a:y", &img, wider, 1.8, Fit::Contain, &mut buf),
            "a new cell aspect must resample"
        );
        assert!(
            m.render("a:y", &img, wider, 1.8, Fit::Stretch, &mut buf),
            "a new fit must resample"
        );
    }

    #[test]
    fn the_memo_ignores_where_the_rect_sits_on_screen() {
        // the same grid moved sideways is the same rasterization; a
        // pane sliding across should not pay to resample.
        let mut buf = buffer(20, 8);
        let img = flat(32, 32, 100);
        let mut m = Memo::default();
        m.render(
            "a:x",
            &img,
            Rect::new(0, 0, 8, 4),
            2.0,
            Fit::Contain,
            &mut buf,
        );
        assert!(!m.render(
            "a:x",
            &img,
            Rect::new(9, 3, 8, 4),
            2.0,
            Fit::Contain,
            &mut buf
        ));
        assert_eq!(char_at(&buf, 9, 3), HALF_BLOCK, "it moved with the rect");
    }

    #[test]
    fn a_new_cover_under_the_same_key_is_the_one_thing_the_memo_cannot_see() {
        // stated outright because it is the contract callers have to
        // honour: the key names the picture. two pictures under one
        // key, same size, and the first one keeps showing.
        let rect = Rect::new(0, 0, 4, 2);
        let mut buf = buffer(4, 2);
        let mut m = Memo::default();
        m.render("same", &flat(8, 8, 10), rect, 2.0, Fit::Stretch, &mut buf);
        assert!(!m.render("same", &flat(8, 8, 250), rect, 2.0, Fit::Stretch, &mut buf));
        assert_eq!(
            buf.cell((0, 0)).unwrap().fg,
            shade(10.0 / 255.0),
            "documented: the key is the whole invalidation story"
        );
    }

    #[test]
    fn it_never_writes_outside_its_rect() {
        let mut buf = buffer(12, 8);
        let rect = Rect::new(3, 2, 5, 4);
        let mut r = Raster::new();
        r.render(&ramp(50, 50), rect, 2.0, Fit::Contain, &mut buf);
        assert_outside_untouched(&buf, rect);
    }

    #[test]
    fn it_clips_to_a_buffer_smaller_than_the_rect_asked_for() {
        let mut buf = buffer(4, 2);
        let mut r = Raster::new();
        // a rect running off both edges: paint what fits, panic on
        // nothing.
        r.render(
            &ramp(50, 50),
            Rect::new(2, 1, 40, 20),
            2.0,
            Fit::Stretch,
            &mut buf,
        );
        assert_eq!(char_at(&buf, 3, 1), HALF_BLOCK);
        assert_eq!(char_at(&buf, 0, 0), '.', "outside the rect, untouched");
    }

    #[test]
    fn degenerate_sizes_draw_nothing_and_do_not_panic() {
        let mut buf = buffer(4, 4);
        let mut r = Raster::new();
        for rect in [
            Rect::new(0, 0, 0, 0),
            Rect::new(0, 0, 4, 0),
            Rect::new(0, 0, 0, 4),
            Rect::new(9, 9, 4, 4),
        ] {
            r.render(&ramp(8, 8), rect, 2.0, Fit::Contain, &mut buf);
        }
        // a 1x1 source and a 1x1 rect, the other end of the range. one
        // cell is physically 1 wide by 2 tall, so a square source
        // fills the top sub-row and the bottom is a bar; under
        // `Stretch` the same cell is the image twice over.
        r.render(
            &flat(1, 1, 77),
            Rect::new(0, 0, 1, 1),
            2.0,
            Fit::Contain,
            &mut buf,
        );
        assert_eq!((r.at(0, 0), r.at(0, 1)), (77, 0));
        r.render(
            &flat(1, 1, 77),
            Rect::new(0, 0, 1, 1),
            2.0,
            Fit::Stretch,
            &mut buf,
        );
        assert_eq!((r.at(0, 0), r.at(0, 1)), (77, 77));
    }

    #[test]
    fn a_zero_sized_image_draws_nothing() {
        let mut r = Raster::new();
        r.sample(
            &DynamicImage::ImageRgb8(RgbImage::new(0, 0)),
            Rect::new(0, 0, 4, 2),
            2.0,
            Fit::Contain,
        );
        assert!(levels(&r).iter().all(|&v| v == 0));
    }

    #[test]
    fn rgba_luma_and_lumaa_sources_all_read_the_same_grey() {
        let rect = Rect::new(0, 0, 4, 2);
        let mut out = Vec::new();
        for img in [
            flat(32, 32, 90),
            DynamicImage::ImageRgba8(image::RgbaImage::from_pixel(
                32,
                32,
                image::Rgba([90, 90, 90, 255]),
            )),
            DynamicImage::ImageLuma8(image::GrayImage::from_pixel(32, 32, image::Luma([90]))),
            DynamicImage::ImageLumaA8(image::GrayAlphaImage::from_pixel(
                32,
                32,
                image::LumaA([90, 255]),
            )),
            // the exotic path: 16 bits per channel, normalized once.
            DynamicImage::ImageLuma16(image::ImageBuffer::from_pixel(
                32,
                32,
                image::Luma([90u16 * 257]),
            )),
        ] {
            let mut r = Raster::new();
            r.sample(&img, rect, 2.0, Fit::Stretch);
            out.push(r.at(1, 1));
        }
        for v in &out {
            assert!(
                v.abs_diff(90) <= 1,
                "a flat grey read back as {v:?} across variants: {out:?}"
            );
        }
    }

    #[test]
    fn resampling_the_same_raster_at_a_new_size_forgets_the_old_one() {
        // what a resize does: the grid has to be the new rect's, with
        // no leftovers from the old one readable through `at`.
        let mut r = Raster::new();
        r.sample(
            &flat(32, 32, 255),
            Rect::new(0, 0, 30, 10),
            2.0,
            Fit::Stretch,
        );
        r.sample(&flat(32, 32, 10), Rect::new(0, 0, 4, 2), 2.0, Fit::Stretch);
        assert_eq!((r.cols(), r.rows()), (4, 2));
        assert_eq!(levels(&r).len(), 16);
        assert!(levels(&r).iter().all(|&v| v == 10));
        assert_eq!(r.at(9, 0), 0, "out of the new grid reads as nothing");
    }

    #[test]
    fn the_thread_local_scratch_is_reused_rather_than_reallocated() {
        let img = flat(64, 64, 44);
        let rect = Rect::new(0, 0, 20, 10);
        let first = with_scratch(|r| {
            r.sample(&img, rect, 2.0, Fit::Stretch);
            r.luma().as_ptr()
        });
        let second = with_scratch(|r| {
            r.sample(&img, rect, 2.0, Fit::Stretch);
            r.luma().as_ptr()
        });
        assert_eq!(first, second, "the same grid should not reallocate");
    }

    /// Fastest per-iteration time over `batches` batches of `n`. The
    /// minimum, not the mean: this machine runs other builds, and a
    /// mean measures their scheduling as much as this code.
    #[cfg(test)]
    fn best(batches: u32, n: u32, mut f: impl FnMut()) -> std::time::Duration {
        use std::time::Instant;
        (0..batches)
            .map(|_| {
                let t = Instant::now();
                for _ in 0..n {
                    f();
                }
                t.elapsed() / n
            })
            .min()
            .unwrap_or_default()
    }

    /// Numbers for the report, not a pass/fail gate; `cargo test
    /// --release -- --ignored --nocapture raster_cost` prints them.
    #[test]
    #[ignore = "timing, run explicitly"]
    fn raster_cost() {
        let cover = ramp(640, 640);
        let frame = ramp(1920, 1080);
        let cases: [(&str, &DynamicImage, Rect); 4] = [
            (
                "cover 640px -> 40x20 cells",
                &cover,
                Rect::new(0, 0, 40, 20),
            ),
            (
                "cover 640px -> 120x60 cells",
                &cover,
                Rect::new(0, 0, 120, 60),
            ),
            (
                "cover 640px -> 400x200 cells",
                &cover,
                Rect::new(0, 0, 400, 200),
            ),
            (
                "1080p frame -> 400x200 cells",
                &frame,
                Rect::new(0, 0, 400, 200),
            ),
        ];
        let mut buf = Buffer::empty(Rect::new(0, 0, 400, 200));
        for (name, img, rect) in cases {
            let mut r = Raster::new();
            let each = best(5, 100, || r.render(img, rect, 2.0, Fit::Contain, &mut buf));
            // sample alone, to split rasterization from painting.
            let sample = best(5, 100, || r.sample(img, rect, 2.0, Fit::Contain));
            // and what the same frame costs once the grid is
            // memoized: paint only, the part that cannot be skipped.
            let mut m = Memo::default();
            m.render("k", img, rect, 2.0, Fit::Contain, &mut buf);
            let memoized = best(5, 100, || {
                assert!(!m.render("k", img, rect, 2.0, Fit::Contain, &mut buf));
            });
            println!("{name}: render {each:?} (sample {sample:?}, memoized {memoized:?})");
        }
    }

    /// The cost the old sampler had, for the same cases, so the report
    /// is a measured before/after rather than an assertion.
    #[test]
    #[ignore = "timing, run explicitly"]
    fn legacy_raster_cost() {
        use image::GenericImageView;

        /// Verbatim copy of the pre-`raster` sampler, kept here only
        /// so the before number is measured rather than remembered.
        fn sample_luma(
            img: &DynamicImage,
            iw: u32,
            ih: u32,
            out_w: u32,
            out_h: u32,
            col: u32,
            row: u32,
        ) -> f32 {
            let sx0 = (col * iw) / out_w;
            let sx1 = (((col + 1) * iw) / out_w).clamp(sx0 + 1, iw);
            let sy0 = (row * ih) / out_h;
            let sy1 = (((row + 1) * ih) / out_h).clamp(sy0 + 1, ih);
            let mut sum = 0u64;
            let mut count = 0u64;
            for y in sy0..sy1 {
                for x in sx0..sx1 {
                    let p = img.get_pixel(x, y);
                    let luma = 0.2126 * p[0] as f64 + 0.7152 * p[1] as f64 + 0.0722 * p[2] as f64;
                    sum += luma as u64;
                    count += 1;
                }
            }
            if count > 0 {
                (sum as f32 / count as f32) / 255.0
            } else {
                0.0
            }
        }

        let cover = ramp(640, 640);
        let big = ramp(1500, 1500);
        let frame = ramp(1920, 1080);
        let cases: [(&str, &DynamicImage, Rect); 4] = [
            (
                "cover 640px -> 40x20 cells",
                &cover,
                Rect::new(0, 0, 40, 20),
            ),
            ("cover 1500px -> 40x20 cells", &big, Rect::new(0, 0, 40, 20)),
            (
                "cover 640px -> 400x200 cells",
                &cover,
                Rect::new(0, 0, 400, 200),
            ),
            (
                "1080p frame -> 400x200 cells",
                &frame,
                Rect::new(0, 0, 400, 200),
            ),
        ];
        for (name, img, rect) in cases {
            let (iw, ih) = img.dimensions();
            let out_w = rect.width as u32;
            let out_h = rect.height as u32 * 2;
            let each = best(3, 3, || {
                let mut sink = 0f32;
                for row in 0..rect.height {
                    for col in 0..rect.width {
                        sink += sample_luma(img, iw, ih, out_w, out_h, col as u32, row as u32 * 2);
                        sink +=
                            sample_luma(img, iw, ih, out_w, out_h, col as u32, row as u32 * 2 + 1);
                    }
                }
                assert!(sink >= 0.0);
            });
            println!("legacy {name}: {each:?}");
        }
    }
}
