use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::widgets::Widget;

use crate::ui::raster::{self, Fit, Style, Tone};

/// Block-glyph rasterization of an album's cover art, the "pure-TUI
/// purity" modes: `tone` picks grey (`ascii-bw`) or the source's own
/// rgb (`ascii-color`). The rasterization itself lives in
/// `ui::raster`, which knows nothing about covers; this is just the
/// widget wrapper that hands it a rect.
///
/// Deliberately resamples on every render call rather than caching a
/// fixed-size grid: scale the terminal and the next frame resamples at
/// the new resolution for free, no explicit resize handling needed.
/// That is affordable because `raster` keeps its buffers and bounds
/// the work per output sample; see docs/raster.md for the numbers.
pub struct AsciiArt<'a> {
    pub image: Option<&'a image::DynamicImage>,
    /// The terminal's real cell height:width ratio (see
    /// `main::cell_aspect`); together with the rect's own shape it
    /// decides where a non-square image lands, so the art keeps its
    /// proportions instead of being stretched to whatever the rect is.
    pub cell_aspect: f32,
    /// `Tone::Mono` everywhere but the album-art `ascii-color` mode;
    /// the video pane stays monochrome with the rest of the ui.
    pub tone: Tone,
}

impl Widget for AsciiArt<'_> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        let Some(img) = self.image else {
            return;
        };
        // one kept rasterizer per render thread, so a steady frame
        // rate allocates nothing.
        let style = Style {
            fit: Fit::Contain,
            tone: self.tone,
        };
        raster::with_scratch(|r| r.render(img, area, self.cell_aspect, style, buf));
    }
}

/// `AsciiArt` for a picture that does not change between frames:
/// identical rasterization, but skipped outright when neither the
/// picture nor the rect's size has moved since the last frame (see
/// `raster::Memo`). Painting still happens every frame.
///
/// `key` names the *image*, which is the whole invalidation story:
/// album art passes `coverart::art_key`, the same name the
/// `ratatui-image` protocol cache is already rebuilt on, so a cover
/// can only ever be reused for the album it was fetched for.
pub struct CachedArt<'a> {
    pub image: Option<&'a image::DynamicImage>,
    pub cell_aspect: f32,
    pub key: &'a str,
    pub tone: Tone,
}

impl Widget for CachedArt<'_> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        let Some(img) = self.image else {
            return;
        };
        // the memo keys on the style too, so flipping bw/colour
        // resamples rather than repainting the other tone's grid.
        let style = Style {
            fit: Fit::Contain,
            tone: self.tone,
        };
        raster::with_memo(|m| {
            m.render(self.key, img, area, self.cell_aspect, style, buf);
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::testbuf::{assert_outside_untouched, buffer, char_at};
    use image::{Rgb, RgbImage};

    fn cover(w: u32, h: u32, v: u8) -> image::DynamicImage {
        image::DynamicImage::ImageRgb8(RgbImage::from_pixel(w, h, Rgb([v, v, v])))
    }

    #[test]
    fn no_image_draws_nothing() {
        let mut buf = buffer(6, 3);
        AsciiArt {
            image: None,
            cell_aspect: 2.0,
            tone: Tone::Mono,
        }
        .render(Rect::new(0, 0, 6, 3), &mut buf);
        assert_eq!(char_at(&buf, 0, 0), '.');
    }

    #[test]
    fn a_square_cover_fills_a_square_rect_with_half_blocks() {
        let mut buf = buffer(10, 6);
        let rect = Rect::new(1, 1, 8, 4);
        AsciiArt {
            image: Some(&cover(300, 300, 160)),
            cell_aspect: 2.0,
            tone: Tone::Mono,
        }
        .render(rect, &mut buf);
        // a flat cover gives every cell four equal samples, so each
        // draws a solid block rather than a half block with two equal
        // shades.
        for y in rect.y..rect.y + rect.height {
            for x in rect.x..rect.x + rect.width {
                assert_eq!(char_at(&buf, x, y), '\u{2588}');
            }
        }
        assert_outside_untouched(&buf, rect);
    }
}
