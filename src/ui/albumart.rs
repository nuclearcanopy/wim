use image::GenericImageView;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::widgets::Widget;

use crate::theme::shade;

/// Grayscale ascii-block rasterization of an album's cover art, the
/// "pure-TUI purity" default mode. Deliberately resamples straight
/// from the source image at whatever `area` actually is on every
/// render call rather than caching a fixed-size grid: scale the
/// terminal and the next frame resamples at the new resolution for
/// free, no explicit resize handling needed.
pub struct AsciiArt<'a> {
    pub image: Option<&'a image::DynamicImage>,
    /// The terminal's real cell height:width ratio (see
    /// `main::cell_aspect`); must match whatever `browse::layout` used
    /// to size the rect this renders into, or the two disagreeing
    /// assumptions are exactly what makes the art look non-square.
    pub cell_aspect: f32,
}

impl Widget for AsciiArt<'_> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        let Some(img) = self.image else {
            return;
        };
        if area.width == 0 || area.height == 0 {
            return;
        }
        let (iw, ih) = img.dimensions();
        if iw == 0 || ih == 0 {
            return;
        }

        // terminal cells are taller than wide by `cell_aspect`;
        // sampling one cell as a single square pixel leaves the image
        // looking vertically squashed. sample two vertical sub-rows
        // per cell instead and render them as an upper half block
        // (U+2580): foreground paints the top sub-row, background the
        // bottom, doubling the effective vertical resolution so each
        // pair of sub-rows covers the same physical height as one
        // sub-row's width, same ratio `browse::layout` sized this
        // rect with.
        let out_w = area.width as u32;
        let out_h = (area.height as f32 * self.cell_aspect).round() as u32;

        for row in 0..area.height {
            for col in 0..area.width {
                let top = sample_luma(img, iw, ih, out_w, out_h, col as u32, row as u32 * 2);
                let bottom = sample_luma(img, iw, ih, out_w, out_h, col as u32, row as u32 * 2 + 1);

                let x = area.x + col;
                let y = area.y + row;
                if let Some(cell) = buf.cell_mut((x, y)) {
                    cell.set_char('▀');
                    cell.set_fg(shade(top));
                    cell.set_bg(shade(bottom));
                }
            }
        }
    }
}

/// Average luma (0..1) of the source-image region corresponding to
/// one cell of an `out_w` x `out_h` virtual sampling grid.
fn sample_luma(
    img: &image::DynamicImage,
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
            // standard luma weights; alpha isn't factored in since
            // cover art is opaque in practice.
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
