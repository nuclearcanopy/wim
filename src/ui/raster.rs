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

/// Samples per cell on each axis. The 2x2 block glyphs carry two
/// across and two down, so a sample is half a cell wide and half a
/// cell tall; `place` needs both numbers to keep the picture's aspect
/// right, and getting the horizontal one wrong is what squashed the
/// letterbox bars when this went from half-blocks to quadrants.
const SAMPLES_X: u32 = 2;
const SAMPLES_Y: u32 = 2;

/// The sixteen 2x2 block glyphs, indexed by a mask of which quadrants
/// take the foreground shade: bit 0 upper-left, 1 upper-right, 2
/// lower-left, 3 lower-right.
///
/// All of these live in Unicode's Block Elements (U+2580..U+259F), the
/// same block as the half-block this used to draw with, so a font that
/// could render the old output can render this.
const QUADRANTS: [char; 16] = [
    '\u{2588}', // 0000 all background, drawn as a solid in the bg shade
    '\u{2598}', // 0001 upper left
    '\u{259d}', // 0010 upper right
    HALF_BLOCK, // 0011 upper half, the glyph this used to draw with
    '\u{2596}', // 0100 lower left
    '\u{258c}', // 0101 left half
    '\u{259e}', // 0110 upper right + lower left
    '\u{259b}', // 0111 all but lower right
    '\u{2597}', // 1000 lower right
    '\u{259a}', // 1001 upper left + lower right
    '\u{2590}', // 1010 right half
    '\u{259c}', // 1011 all but lower left
    '\u{2584}', // 1100 lower half
    '\u{2599}', // 1101 all but upper right
    '\u{259f}', // 1110 all but upper left
    '\u{2588}', // 1111 full block
];

/// Sharpen strength at `SHARPEN_REF_SPAN` samples across, scaled
/// inversely with resolution by `sharpen_amount`.
const SHARPEN_REF: f32 = 0.7;
/// Grid span the reference strength is tuned for.
const SHARPEN_REF_SPAN: f32 = 160.0;
/// Floor and ceiling on the scaled amount. The ceiling is the load
/// bearing one: past roughly this, an unsharp mask stops adding
/// definition and starts drawing a bright rim along the dark side of
/// every edge, which reads as a worse picture, not a sharper one.
const SHARPEN_MIN: f32 = 0.5;
const SHARPEN_MAX: f32 = 2.2;

/// How hard to sharpen a grid this size.
///
/// Lower resolution gets more, which is the right instinct: each
/// output sample straddles more of the image, so the box average that
/// produced it cut more detail and the result reads softer. It is
/// inverse-proportional rather than exponential, and clamped at both
/// ends, because sharpening cannot recover detail that was never
/// sampled; past `SHARPEN_MAX` it only adds halos.
fn sharpen_amount(grid_w: u32, grid_h: u32) -> f32 {
    let span = grid_w.max(grid_h).max(1) as f32;
    (SHARPEN_REF * (SHARPEN_REF_SPAN / span)).clamp(SHARPEN_MIN, SHARPEN_MAX)
}

/// Whether a rasterization is monochrome or carries the source's own
/// colour. `Mono` is the project default (every colour through
/// `theme::shade`); `Color` exists for the album-art `ascii-color`
/// mode, the one sanctioned exception to the monochrome rule.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Tone {
    #[default]
    Mono,
    Color,
}

/// Fit plus tone: everything about *how* a picture is rasterized that
/// the caller chooses. One struct rather than two parameters so
/// `Memo::render` stays inside clippy's argument limit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Style {
    pub fit: Fit,
    pub tone: Tone,
}

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
    /// The pre-sharpen grid, so the pass reads neighbours that have
    /// not already been rewritten. Reused, like every other buffer
    /// here, so a frame still allocates nothing.
    flat: Vec<u8>,
    /// Rgb per half-cell, parallel to `luma`, and empty unless the
    /// last `sample` asked for `Tone::Color`; a mono rasterization
    /// pays nothing for it. `luma` is kept either way: it is what
    /// picks the glyph and drives the sharpen pass.
    rgb: Vec<[u8; 3]>,
    tone: Tone,
}

impl Raster {
    pub fn new() -> Self {
        Self::default()
    }

    /// Samples `img` into the half-cell grid for an `area`-shaped cell
    /// rect. `area`'s position is ignored here (only `paint` needs
    /// it), so the same sampling is reusable wherever it lands.
    pub fn sample(&mut self, img: &DynamicImage, area: Rect, cell_aspect: f32, style: Style) {
        self.cols = area.width;
        self.rows = area.height;
        self.tone = style.tone;
        // four samples per cell: two across, two down. The 2x2 block
        // glyphs carry twice the horizontal detail the half-block did,
        // which is the only thing that actually makes a cover look
        // sharper; a filter cannot add detail that was never sampled.
        let samples = area.width as usize * area.height as usize * 4;
        self.luma.clear();
        self.luma.resize(samples, 0);
        self.rgb.clear();
        if style.tone == Tone::Color {
            // whatever `place` leaves over stays [0, 0, 0], the same
            // black bar luma zero is in the mono grid.
            self.rgb.resize(samples, [0; 3]);
        }
        if samples == 0 {
            return;
        }
        match Pixels::of(img) {
            Some(px) => {
                self.fill(&px, cell_aspect, style);
                self.sharpen();
            }
            None => {
                // not 8 bits per channel (a 16-bit or float png).
                // converted whole once here, which does allocate,
                // rather than reading the source through `get_pixel`
                // per sample. covers are normalized to rgb8 at decode
                // (`coverart::decode_cover`) and mpv hands over rgb8,
                // so this is the exotic path, never the hot one.
                if style.tone == Tone::Color {
                    let rgb = img.to_rgb8();
                    let (w, h) = rgb.dimensions();
                    if let Some(px) = Pixels::new(rgb.as_raw(), w, h, 3, false) {
                        self.fill(&px, cell_aspect, style);
                        self.sharpen();
                    }
                } else {
                    let lum = img.to_luma8();
                    let (w, h) = lum.dimensions();
                    if let Some(px) = Pixels::new(lum.as_raw(), w, h, 1, true) {
                        self.fill(&px, cell_aspect, style);
                        self.sharpen();
                    }
                }
            }
        }
    }

    /// Unsharp mask over the sampled grid, in place.
    ///
    /// A four-neighbour cross rather than a full 3x3: at this
    /// resolution the diagonals change nothing anyone can see, for
    /// twice the reads. Edge samples clamp to themselves, so the
    /// border neither wraps nor darkens.
    ///
    /// Uniform input is a fixed point: every neighbour equals the
    /// centre, so the correction is zero and flat areas stay flat.
    /// That is what keeps this "pixelated" rather than "noisy".
    ///
    /// Luma only, in colour mode too: luma is what picks the glyph, so
    /// sharpening it steepens the structure, while leaving `rgb` the
    /// plain box average keeps hues off the unsharp mask's halos.
    fn sharpen(&mut self) {
        let amount = sharpen_amount(self.cols as u32 * 2, self.rows as u32 * 2);
        self.sharpen_with(amount);
    }

    fn sharpen_with(&mut self, amount: f32) {
        if amount <= 0.0 {
            return;
        }
        let w = self.cols as usize * 2;
        let h = self.rows as usize * 2;
        // a 1-wide or 1-tall grid has no neighbourhood worth the name.
        if w < 3 || h < 3 || self.luma.len() < w * h {
            return;
        }
        let Self { luma, flat, .. } = self;
        flat.clear();
        flat.extend_from_slice(luma);
        for y in 0..h {
            let ym = y.saturating_sub(1) * w;
            let yp = (y + 1).min(h - 1) * w;
            let row = y * w;
            for x in 0..w {
                let xm = x.saturating_sub(1);
                let xp = (x + 1).min(w - 1);
                let around = (flat[row + xm] as f32
                    + flat[row + xp] as f32
                    + flat[ym + x] as f32
                    + flat[yp + x] as f32)
                    * 0.25;
                let c = flat[row + x] as f32;
                luma[row + x] = (c + amount * (c - around)).clamp(0.0, 255.0) as u8;
            }
        }
    }

    fn fill(&mut self, px: &Pixels, cell_aspect: f32, style: Style) {
        // field-wise borrows: the inner loop reads `xs` while writing
        // `acc`, which it cannot do through `self`.
        let Self {
            luma,
            cols,
            rows,
            xs,
            acc,
            rgb,
            ..
        } = self;
        // a grey source has nothing to carry: three equal channels is
        // the same answer as one, for three times the reads.
        let colour = style.tone == Tone::Color && !px.gray;
        let grid_w = *cols as u32 * SAMPLES_X;
        let grid_h = *rows as u32 * SAMPLES_Y;
        let (ow, oh, off_x, off_y) = place(px.w, px.h, grid_w, grid_h, cell_aspect, style.fit);
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
        // three slots per output sample in colour, one in mono.
        acc.resize(ow as usize * if colour { 3 } else { 1 }, 0);
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
                if colour {
                    for (slot, taps) in acc.as_chunks_mut::<3>().0.iter_mut().zip(taps) {
                        for &dx in taps {
                            let p = &row[dx as usize..dx as usize + 3];
                            slot[0] += p[0] as u32;
                            slot[1] += p[1] as u32;
                            slot[2] += p[2] as u32;
                        }
                    }
                } else if px.gray {
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
            if colour {
                // luma derived from the averaged rgb rather than
                // accumulated beside it: `rgb_luma` is linear in the
                // channels, so this is the same number the mono path
                // computes, and the two grids cannot disagree.
                for (ox, c) in acc.as_chunks::<3>().0.iter().enumerate() {
                    let p = [
                        (c[0] / count) as u8,
                        (c[1] / count) as u8,
                        (c[2] / count) as u8,
                    ];
                    rgb[base + ox] = p;
                    luma[base + ox] = rgb_luma(&p) as u8;
                }
            } else {
                for (ox, &sum) in acc.iter().enumerate() {
                    let v = (sum / count) as u8;
                    luma[base + ox] = v;
                    // a grey source asked for in colour: the grid
                    // still has to be filled, with the grey itself.
                    if let Some(slot) = rgb.get_mut(base + ox) {
                        *slot = [v; 3];
                    }
                }
            }
        }
    }

    /// Paints the last `sample` into `buf` at `area`, clipped to the
    /// buffer. After a `Tone::Mono` sample every colour is a
    /// `theme::shade` level and nothing here can introduce a hue;
    /// after a `Tone::Color` one each cell's two shades are the
    /// source's own rgb instead.
    pub fn paint(&self, area: Rect, buf: &mut Buffer) {
        let vis = area.intersection(buf.area);
        if vis.is_empty() || self.cols == 0 || self.rows == 0 {
            return;
        }
        // the grids are filled together, so a short one means the last
        // sample was mono; fall back to shades rather than indexing it.
        let colour = self.tone == Tone::Color && self.rgb.len() == self.luma.len();
        let shades = shades();
        let grid_w = self.cols as usize * 2;
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
                if col >= self.cols as usize {
                    break;
                }
                // the cell's four samples, clockwise from upper left.
                let gx = col * 2;
                let q = [
                    self.luma[top + gx],
                    self.luma[top + gx + 1],
                    self.luma[bottom + gx],
                    self.luma[bottom + gx + 1],
                ];
                let (mask, fg, bg) = quantize(q);
                cell.set_char(QUADRANTS[mask]);
                if colour {
                    // the same split `quantize` made, applied to the
                    // cell's own rgb: the glyph and the two colours
                    // are then describing one grouping, not two.
                    let (f, b) = group_rgb(
                        mask,
                        [
                            self.rgb[top + gx],
                            self.rgb[top + gx + 1],
                            self.rgb[bottom + gx],
                            self.rgb[bottom + gx + 1],
                        ],
                    );
                    cell.set_fg(Color::Rgb(f[0], f[1], f[2]));
                    cell.set_bg(Color::Rgb(b[0], b[1], b[2]));
                } else {
                    cell.set_fg(shades[fg as usize]);
                    cell.set_bg(shades[bg as usize]);
                }
            }
        }
    }

    /// `sample` then `paint`: the whole job, one call per frame.
    pub fn render(
        &mut self,
        img: &DynamicImage,
        area: Rect,
        cell_aspect: f32,
        style: Style,
        buf: &mut Buffer,
    ) {
        self.sample(img, area, cell_aspect, style);
        self.paint(area, buf);
    }
}

/// One cell's four samples reduced to a `QUADRANTS` mask plus two
/// levels. The mask, not the glyph, so `paint` can group the cell's
/// rgb the same way when it needs colour.
///
/// A terminal cell can hold exactly two colours, so four samples have
/// to be split in two groups and each group averaged. The split is at
/// the midpoint of the cell's own range rather than a fixed level, so
/// it adapts to local contrast: a cell spanning 90..110 splits at 100
/// and keeps the detail a global threshold would flatten away.
///
/// A uniform cell (every sample equal) lands wholly in the foreground
/// and draws a full block, which is the crispest thing available and
/// avoids inventing an edge inside flat colour.
fn quantize(q: [u8; 4]) -> (usize, u8, u8) {
    let lo = q.iter().copied().min().unwrap_or(0);
    let hi = q.iter().copied().max().unwrap_or(0);
    let mid = (lo as u16 + hi as u16).div_ceil(2) as u8;

    let mut mask = 0usize;
    let mut fg_sum = 0u32;
    let mut fg_n = 0u32;
    let mut bg_sum = 0u32;
    let mut bg_n = 0u32;
    for (i, &v) in q.iter().enumerate() {
        if v >= mid {
            mask |= 1 << i;
            fg_sum += v as u32;
            fg_n += 1;
        } else {
            bg_sum += v as u32;
            bg_n += 1;
        }
    }
    // `mid` is inside the range, so at least one sample is always in
    // the foreground; the background can be empty when the cell is
    // flat, and then both shades are that one level.
    let fg = fg_sum.checked_div(fg_n).map_or(lo, |v| v as u8);
    let bg = bg_sum.checked_div(bg_n).map_or(fg, |v| v as u8);
    (mask, fg, bg)
}

/// The two colours a cell's four rgb samples reduce to, split by the
/// mask `quantize` already picked from their luma. Averaging per group
/// rather than taking one sample's colour keeps a cell straddling an
/// edge from flipping hue with the glyph.
fn group_rgb(mask: usize, q: [[u8; 3]; 4]) -> ([u8; 3], [u8; 3]) {
    let mut fg = [0u32; 3];
    let mut bg = [0u32; 3];
    let mut fg_n = 0u32;
    let mut bg_n = 0u32;
    for (i, p) in q.iter().enumerate() {
        let (sum, n) = if mask & (1 << i) != 0 {
            (&mut fg, &mut fg_n)
        } else {
            (&mut bg, &mut bg_n)
        };
        for (s, &v) in sum.iter_mut().zip(p.iter()) {
            *s += v as u32;
        }
        *n += 1;
    }
    let avg = |s: [u32; 3], n: u32| std::array::from_fn(|i| (s[i] / n.max(1)) as u8);
    // `quantize` always leaves at least one sample in the foreground;
    // an empty background (a flat cell) takes the foreground's colour.
    let f = avg(fg, fg_n);
    (f, if bg_n == 0 { f } else { avg(bg, bg_n) })
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

    /// The whole output: `cols() * 2 * rows() * 2` luma levels,
    /// row-major over sub-rows; two samples per cell on each axis.
    pub fn luma(&self) -> &[u8] {
        &self.luma
    }

    /// One sample's luma. `col` runs `0..cols() * 2` and `sub_row`
    /// `0..rows() * 2`, both twice the cell count; anything outside
    /// the grid reads as 0 rather than panicking.
    pub fn at(&self, col: u16, sub_row: u16) -> u8 {
        let grid_w = self.cols.saturating_mul(2);
        if col >= grid_w || sub_row >= self.rows.saturating_mul(2) {
            return 0;
        }
        self.luma[sub_row as usize * grid_w as usize + col as usize]
    }

    /// One sample's rgb, `[0, 0, 0]` outside the grid and after a
    /// `Tone::Mono` sample (which keeps no rgb grid at all).
    pub fn rgb_at(&self, col: u16, sub_row: u16) -> [u8; 3] {
        let grid_w = self.cols.saturating_mul(2);
        if col >= grid_w || sub_row >= self.rows.saturating_mul(2) {
            return [0; 3];
        }
        self.rgb
            .get(sub_row as usize * grid_w as usize + col as usize)
            .copied()
            .unwrap_or([0; 3])
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
    // the rect's physical shape measured in cell widths: each sample
    // is `1 / SAMPLES_X` of a cell across and `cell_aspect /
    // SAMPLES_Y` of a cell width down.
    let aspect = if cell_aspect.is_finite() {
        cell_aspect.clamp(0.25, 8.0)
    } else {
        2.0
    };
    let phys_w = grid_w as f32 / SAMPLES_X as f32;
    let phys_h = grid_h as f32 * aspect / SAMPLES_Y as f32;
    let want = iw as f32 / ih as f32;
    let (draw_w, draw_h) = if phys_w / phys_h > want {
        (phys_h * want, phys_h)
    } else {
        (phys_w, phys_w / want)
    };
    // back from cell widths into grid samples on each axis.
    let ow = ((draw_w * SAMPLES_X as f32).round() as u32).clamp(1, grid_w);
    let oh = ((draw_h * SAMPLES_Y as f32 / aspect).round() as u32).clamp(1, grid_h);
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
    style: Style,
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
/// aspect or the style. Worst-case staleness is unbounded in *time* and
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
    /// `style` differ from the last call, then paints. Returns whether
    /// it had to resample, which is what the tests assert on.
    pub fn render(
        &mut self,
        key: &str,
        img: &DynamicImage,
        area: Rect,
        cell_aspect: f32,
        style: Style,
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
                && l.style == style
        });
        if resampled {
            self.raster.sample(img, area, cell_aspect, style);
            self.last = Some(Sampled {
                key: key.to_string(),
                width: area.width,
                height: area.height,
                aspect,
                style,
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
    /// A monochrome style, which is what nearly every test here wants.
    fn mono(fit: Fit) -> Style {
        Style {
            fit,
            tone: Tone::Mono,
        }
    }

    fn colour(fit: Fit) -> Style {
        Style {
            fit,
            tone: Tone::Color,
        }
    }

    /// A source with real hues in every channel, so a test that claims
    /// colour survived (or did not leak) is actually checking that.
    fn hued(w: u32, h: u32) -> DynamicImage {
        DynamicImage::ImageRgb8(RgbImage::from_fn(w, h, |x, y| {
            Rgb([
                (x * 255 / (w - 1).max(1)) as u8,
                (y * 255 / (h - 1).max(1)) as u8,
                200,
            ])
        }))
    }

    /// Any of the sixteen block glyphs. Tests about *placement* assert
    /// this rather than one specific glyph, since which one a cell
    /// gets depends on its own four samples.
    fn is_block(c: char) -> bool {
        QUADRANTS.contains(&c)
    }

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
        r.sample(
            &flat(8, 8, 128),
            Rect::new(0, 0, 10, 4),
            2.0,
            mono(Fit::Stretch),
        );
        assert_eq!((r.cols(), r.rows()), (10, 4));
        assert_eq!(
            levels(&r).len(),
            10 * 4 * 4,
            "four samples per cell: two across, two down"
        );
    }

    #[test]
    fn a_half_white_half_black_image_fills_one_cell_as_two_sub_rows() {
        // the whole reason for the half block: a single cell carries
        // both halves of the picture, not their average.
        let mut r = Raster::new();
        r.sample(&split(4, 4), Rect::new(0, 0, 1, 1), 2.0, mono(Fit::Stretch));
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
            mono(Fit::Stretch),
            &mut buf,
        );
        // a cell wholly inside one half of the picture has nothing to
        // split, so it draws a solid in that shade: crisper than a
        // half block carrying the same shade twice.
        for y in 0..2 {
            for x in 0..3 {
                assert_eq!(char_at(&buf, x, y), '\u{2588}');
            }
        }
        assert_eq!(buf.cell((0, 0)).unwrap().fg, shade(1.0));
        assert_eq!(buf.cell((0, 1)).unwrap().fg, shade(0.0));

        // one cell row instead of two, so a single cell straddles the
        // boundary and the half block earns its place: white over black.
        let mut buf = buffer(3, 1);
        let mut r = Raster::new();
        r.render(
            &split(4, 4),
            Rect::new(0, 0, 3, 1),
            2.0,
            mono(Fit::Stretch),
            &mut buf,
        );
        assert_eq!(char_at(&buf, 0, 0), HALF_BLOCK);
        assert_eq!(buf.cell((0, 0)).unwrap().fg, shade(1.0));
        assert_eq!(buf.cell((0, 0)).unwrap().bg, shade(0.0));
    }

    /// `Tone::Mono` is still strictly grey, from a hued source too:
    /// this is the invariant the whole ui rests on, and only the
    /// album-art `ascii-color` mode is allowed out of it.
    #[test]
    fn every_colour_mono_can_produce_is_a_grey() {
        for img in [ramp(64, 64), hued(64, 64)] {
            let mut buf = buffer(6, 3);
            let mut r = Raster::new();
            r.render(
                &img,
                Rect::new(0, 0, 6, 3),
                2.0,
                mono(Fit::Stretch),
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
    }

    /// `Tone::Color` paints the source's own rgb, as the same block
    /// glyphs: the quadrant set is what gives this twice the
    /// horizontal detail of a half-block renderer, so colour must not
    /// cost any of it.
    #[test]
    fn colour_keeps_the_quadrant_glyphs_and_paints_real_hues() {
        let mut buf = buffer(8, 4);
        let rect = Rect::new(0, 0, 8, 4);
        let mut r = Raster::new();
        r.render(&hued(64, 64), rect, 2.0, colour(Fit::Stretch), &mut buf);

        let mut tinted = 0;
        for cell in buf.content.iter() {
            assert!(
                is_block(cell.symbol().chars().next().unwrap()),
                "not one of the sixteen block glyphs: {:?}",
                cell.symbol()
            );
            for c in [cell.fg, cell.bg] {
                match c {
                    Color::Rgb(a, b, d) => tinted += u32::from(!(a == b && b == d)),
                    other => panic!("not an rgb colour: {other:?}"),
                }
            }
        }
        assert!(tinted > 0, "colour mode painted nothing but greys");
        // and the grid is still four samples per cell, i.e. the same
        // resolution the mono path has.
        assert_eq!(r.luma().len(), 8 * 4 * 4);
    }

    /// A cell wholly inside one flat colour draws that colour, not an
    /// average with something else: the plainest check that the rgb
    /// grid lines up with the luma grid it is indexed beside.
    #[test]
    fn a_flat_colour_cover_paints_that_exact_colour() {
        let flat_red = DynamicImage::ImageRgb8(RgbImage::from_pixel(32, 32, Rgb([200, 40, 60])));
        let mut buf = buffer(4, 2);
        let rect = Rect::new(0, 0, 4, 2);
        let mut r = Raster::new();
        r.render(&flat_red, rect, 2.0, colour(Fit::Stretch), &mut buf);
        for cell in buf.content.iter() {
            assert_eq!(cell.fg, Color::Rgb(200, 40, 60));
            assert_eq!(cell.bg, Color::Rgb(200, 40, 60));
        }
        assert_eq!(r.rgb_at(0, 0), [200, 40, 60]);
    }

    /// Flipping tone is a resample, not a repaint: the memo would
    /// otherwise hand back the other tone's grid under the same key.
    #[test]
    fn the_memo_invalidates_on_a_new_tone() {
        let mut buf = buffer(6, 3);
        let rect = Rect::new(0, 0, 6, 3);
        let img = hued(32, 32);
        let mut m = Memo::default();
        assert!(m.render("a:x", &img, rect, 2.0, mono(Fit::Contain), &mut buf));
        assert!(
            m.render("a:x", &img, rect, 2.0, colour(Fit::Contain), &mut buf),
            "bw -> colour must resample"
        );
        assert!(
            m.render("a:x", &img, rect, 2.0, mono(Fit::Contain), &mut buf),
            "colour -> bw must resample"
        );
    }

    /// Letterbox bars stay black in colour too, rather than picking up
    /// whatever the uninitialized rgb grid held.
    #[test]
    fn contain_bars_are_black_in_colour_mode() {
        let mut r = Raster::new();
        // a 4:1 source in a physically square grid: bars top and
        // bottom, same case `stretch_uses_the_whole_grid...` covers.
        r.sample(
            &hued(400, 100),
            Rect::new(0, 0, 20, 10),
            2.0,
            colour(Fit::Contain),
        );
        for sub in 0..7 {
            assert_eq!(r.rgb_at(10, sub), [0; 3], "top bar should be black");
        }
        for sub in 14..20 {
            assert_eq!(r.rgb_at(10, sub), [0; 3], "bottom bar should be black");
        }
    }

    /// A grey source asked for in colour: the rgb grid still has to be
    /// filled, with the grey, so `paint` does not read a short grid.
    #[test]
    fn a_grey_source_in_colour_mode_still_fills_the_rgb_grid() {
        let mut r = Raster::new();
        let grey =
            DynamicImage::ImageLuma8(image::GrayImage::from_pixel(32, 32, image::Luma([90])));
        r.sample(&grey, Rect::new(0, 0, 4, 2), 2.0, colour(Fit::Stretch));
        assert_eq!(r.rgb_at(1, 1), [90; 3]);
    }

    #[test]
    fn a_flat_image_samples_to_its_own_level_everywhere() {
        let mut r = Raster::new();
        r.sample(
            &flat(101, 37, 200),
            Rect::new(0, 0, 13, 7),
            2.0,
            mono(Fit::Stretch),
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
        r.sample(&wide, Rect::new(0, 0, 20, 10), 2.0, mono(Fit::Stretch));
        assert!(
            levels(&r).iter().any(|&v| v > 240),
            "stretch should reach the image's bright edge"
        );
        // the leftmost sample is the box average of the ramp's first
        // column of source pixels, so near-black rather than exactly 0.
        assert!(r.at(0, 0) < 10, "left edge read as {}", r.at(0, 0));

        r.sample(&wide, Rect::new(0, 0, 20, 10), 2.0, mono(Fit::Contain));
        // grid is 20x20 half-cells, physically square at aspect 2; a
        // 4:1 image draws 20 wide and 5 tall, centred.
        for sub in 0..7 {
            assert_eq!(r.at(10, sub), 0, "top bar should be black");
        }
        // the grid is 40 samples across now (two per cell), so the
        // ramp's bright end is column 39, not 19.
        assert!(
            r.at(39, 10) > 200,
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
            mono(Fit::Contain),
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
                mono(Fit::Stretch),
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
        assert!(m.render("a:x", &img, rect, 2.0, mono(Fit::Contain), &mut buf));
        for _ in 0..5 {
            assert!(
                !m.render("a:x", &img, rect, 2.0, mono(Fit::Contain), &mut buf),
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
        m.render("a:x", &img, rect, 2.0, mono(Fit::Contain), &mut first);
        let mut second = buffer(6, 3);
        assert!(!m.render("a:x", &img, rect, 2.0, mono(Fit::Contain), &mut second));
        assert_eq!(first, second, "a cached frame drew something different");
    }

    #[test]
    fn the_memo_invalidates_on_a_new_cover_a_resize_or_a_new_aspect() {
        let mut buf = buffer(12, 6);
        let img = flat(32, 32, 100);
        let rect = Rect::new(0, 0, 8, 4);
        let mut m = Memo::default();
        m.render("a:x", &img, rect, 2.0, mono(Fit::Contain), &mut buf);
        assert!(
            m.render("a:y", &img, rect, 2.0, mono(Fit::Contain), &mut buf),
            "a different album must not reuse the last one's grid"
        );
        assert!(
            m.render(
                "a:y",
                &img,
                Rect::new(0, 0, 12, 6),
                2.0,
                mono(Fit::Contain),
                &mut buf
            ),
            "a resize must resample"
        );
        let wider = Rect::new(0, 0, 12, 6);
        assert!(
            m.render("a:y", &img, wider, 1.8, mono(Fit::Contain), &mut buf),
            "a new cell aspect must resample"
        );
        assert!(
            m.render("a:y", &img, wider, 1.8, mono(Fit::Stretch), &mut buf),
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
            mono(Fit::Contain),
            &mut buf,
        );
        assert!(!m.render(
            "a:x",
            &img,
            Rect::new(9, 3, 8, 4),
            2.0,
            mono(Fit::Contain),
            &mut buf
        ));
        // flat image, so every cell is a solid; the point is that the
        // memo did not resample just because the rect moved.
        assert_eq!(char_at(&buf, 9, 3), '\u{2588}', "it moved with the rect");
    }

    #[test]
    fn a_new_cover_under_the_same_key_is_the_one_thing_the_memo_cannot_see() {
        // stated outright because it is the contract callers have to
        // honour: the key names the picture. two pictures under one
        // key, same size, and the first one keeps showing.
        let rect = Rect::new(0, 0, 4, 2);
        let mut buf = buffer(4, 2);
        let mut m = Memo::default();
        m.render(
            "same",
            &flat(8, 8, 10),
            rect,
            2.0,
            mono(Fit::Stretch),
            &mut buf,
        );
        assert!(!m.render(
            "same",
            &flat(8, 8, 250),
            rect,
            2.0,
            mono(Fit::Stretch),
            &mut buf
        ));
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
        r.render(&ramp(50, 50), rect, 2.0, mono(Fit::Contain), &mut buf);
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
            mono(Fit::Stretch),
            &mut buf,
        );
        // which glyph depends on the ramp inside that cell; the point
        // is that it was painted at all and nothing outside was.
        assert!(is_block(char_at(&buf, 3, 1)), "{}", char_at(&buf, 3, 1));
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
            r.render(&ramp(8, 8), rect, 2.0, mono(Fit::Contain), &mut buf);
        }
        // a 1x1 source and a 1x1 rect, the other end of the range. one
        // cell is physically 1 wide by 2 tall, so a square source
        // fills the top sub-row and the bottom is a bar; under
        // `Stretch` the same cell is the image twice over.
        r.render(
            &flat(1, 1, 77),
            Rect::new(0, 0, 1, 1),
            2.0,
            mono(Fit::Contain),
            &mut buf,
        );
        assert_eq!((r.at(0, 0), r.at(0, 1)), (77, 0));
        r.render(
            &flat(1, 1, 77),
            Rect::new(0, 0, 1, 1),
            2.0,
            mono(Fit::Stretch),
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
            mono(Fit::Contain),
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
            r.sample(&img, rect, 2.0, mono(Fit::Stretch));
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
            mono(Fit::Stretch),
        );
        r.sample(
            &flat(32, 32, 10),
            Rect::new(0, 0, 4, 2),
            2.0,
            mono(Fit::Stretch),
        );
        assert_eq!((r.cols(), r.rows()), (4, 2));
        assert_eq!(levels(&r).len(), 4 * 2 * 4);
        assert!(levels(&r).iter().all(|&v| v == 10));
        assert_eq!(r.at(9, 0), 0, "out of the new grid reads as nothing");
    }

    #[test]
    fn the_thread_local_scratch_is_reused_rather_than_reallocated() {
        let img = flat(64, 64, 44);
        let rect = Rect::new(0, 0, 20, 10);
        let first = with_scratch(|r| {
            r.sample(&img, rect, 2.0, mono(Fit::Stretch));
            r.luma().as_ptr()
        });
        let second = with_scratch(|r| {
            r.sample(&img, rect, 2.0, mono(Fit::Stretch));
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
            let each = best(5, 100, || {
                r.render(img, rect, 2.0, mono(Fit::Contain), &mut buf)
            });
            // sample alone, to split rasterization from painting.
            let sample = best(5, 100, || r.sample(img, rect, 2.0, mono(Fit::Contain)));
            // and what the same frame costs once the grid is
            // memoized: paint only, the part that cannot be skipped.
            let mut m = Memo::default();
            m.render("k", img, rect, 2.0, mono(Fit::Contain), &mut buf);
            let memoized = best(5, 100, || {
                assert!(!m.render("k", img, rect, 2.0, mono(Fit::Contain), &mut buf));
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

    /// A grid set up by hand, so the sharpen pass can be tested on
    /// known input rather than through a resample.
    fn grid_raster(cols: u16, rows: u16, luma: Vec<u8>) -> Raster {
        let mut r = Raster::new();
        r.cols = cols;
        r.rows = rows;
        assert_eq!(luma.len(), cols as usize * rows as usize * 4);
        r.luma = luma;
        r
    }

    /// The invariant that makes this "pixelated" and not "noisy":
    /// uniform input is a fixed point, so flat areas of a cover stay
    /// flat however hard the dial is turned.
    #[test]
    fn sharpening_leaves_a_flat_image_alone() {
        let mut r = grid_raster(4, 2, vec![128; 32]);
        r.sharpen_with(2.5);
        assert_eq!(r.luma, vec![128; 32], "a flat field must not gain texture");
    }

    /// The point of the pass: an edge comes out steeper than the box
    /// average left it.
    #[test]
    fn sharpening_steepens_an_edge() {
        // 4 cells across is 8 samples; left half dark, right bright,
        // so the edge sits between sample 3 and sample 4.
        let row = [60u8, 60, 60, 60, 200, 200, 200, 200];
        let flat: Vec<u8> = row.iter().cycle().copied().take(32).collect();
        let before = (flat[4] as i32 - flat[3] as i32).abs();

        let mut r = grid_raster(4, 2, flat);
        r.sharpen_with(1.0);
        let after = (r.luma[4] as i32 - r.luma[3] as i32).abs();

        assert!(
            after > before,
            "edge contrast should rise: {before} -> {after}"
        );
        assert!(r.luma[3] < 60, "the dark side should darken");
        assert!(r.luma[4] > 200, "the bright side should brighten");
    }

    #[test]
    fn a_zero_amount_is_exactly_the_box_downscale() {
        let flat: Vec<u8> = (0..32).map(|i| (i * 8) as u8).collect();
        let mut r = grid_raster(4, 2, flat.clone());
        r.sharpen_with(0.0);
        assert_eq!(r.luma, flat);
    }

    /// Extreme neighbours push the correction past the byte range;
    /// that has to clamp rather than wrap a dark sample to white.
    #[test]
    fn sharpening_clamps_instead_of_wrapping() {
        let mut luma = vec![0u8; 32];
        luma[10] = 255;
        let mut r = grid_raster(4, 2, luma);
        r.sharpen_with(3.0);
        // nothing to assert beyond "it is still a byte and did not
        // invert": a wrap would have turned the 0s next to the spike
        // into near-white.
        assert_eq!(r.luma[8], 0, "a dark sample must not wrap to bright");
        assert_eq!(r.luma[10], 255);
    }

    /// A one-column or one-row grid has no neighbourhood, and the
    /// index arithmetic would read out of the grid if it tried.
    #[test]
    fn a_degenerate_grid_is_left_alone_rather_than_panicking() {
        // one cell across is a 2-wide grid: no neighbourhood.
        let thin: Vec<u8> = vec![10, 200, 30, 40, 50, 60, 70, 80];
        let mut r = grid_raster(1, 2, thin.clone());
        r.sharpen_with(1.0);
        assert_eq!(r.luma, thin);

        // one cell down is a 2-tall grid: likewise.
        let short: Vec<u8> = (0..16).map(|i| (i * 10) as u8).collect();
        let mut r = grid_raster(4, 1, short.clone());
        r.sharpen_with(1.0);
        assert_eq!(r.luma, short);
    }
}
