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

        for row in 0..area.height {
            let sy0 = (row as u32 * ih) / area.height as u32;
            let sy1 = (((row + 1) as u32 * ih) / area.height as u32).clamp(sy0 + 1, ih);
            for col in 0..area.width {
                let sx0 = (col as u32 * iw) / area.width as u32;
                let sx1 = (((col + 1) as u32 * iw) / area.width as u32).clamp(sx0 + 1, iw);

                let mut sum = 0u64;
                let mut count = 0u64;
                for y in sy0..sy1 {
                    for x in sx0..sx1 {
                        let p = img.get_pixel(x, y);
                        // standard luma weights; alpha isn't factored
                        // in since cover art is opaque in practice.
                        let luma = 0.2126 * p[0] as f64 + 0.7152 * p[1] as f64 + 0.0722 * p[2] as f64;
                        sum += luma as u64;
                        count += 1;
                    }
                }
                let level = if count > 0 { (sum as f32 / count as f32) / 255.0 } else { 0.0 };

                let x = area.x + col;
                let y = area.y + row;
                if let Some(cell) = buf.cell_mut((x, y)) {
                    cell.set_char('█');
                    cell.set_fg(shade(level));
                }
            }
        }
    }
}
