//! The cover panel during a timed silence.
//!
//! A silence has no cover and never will, so the biggest rect on the
//! full player was simply left blank for the whole gap. The one thing
//! anyone wants to know during a gap is how much longer it is, so that
//! number gets the space, drawn large.
//!
//! Custom-painted like everything else here (no stock widgets), and
//! monochrome: the countdown is bright, its caption is the dimmest
//! shade the ui uses. No pulse or sweep was added on purpose; a
//! countdown already changes every second, and that is the animation.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::widgets::Widget;

use crate::theme::{shade, NEAR_BLACK, NEAR_WHITE};
use crate::ui::progress::fmt_time;
use crate::ui::textfit::width;

/// What is left of the silence that is playing.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Gap {
    pub remaining_secs: f64,
}

/// 3x5 block digits, plus `:`. Hand-authored rather than pulled from
/// `logos::LOGOS`, which is figlet art for the word "WIM" and has no
/// digits in it.
///
/// `#` is an on-cell. Deliberately seven-segment-ish: at three columns
/// there is no room for anything subtler, and the shapes stay legible
/// when each cell is painted as a single block.
const GLYPH_W: usize = 3;
const GLYPH_H: usize = 5;

fn glyph(ch: char) -> Option<[&'static str; GLYPH_H]> {
    Some(match ch {
        '0' => ["###", "# #", "# #", "# #", "###"],
        // centred in its slot, not right-aligned like a seven-segment
        // display's. found by printing the panel: a leading 1 paints
        // one of three columns, so right-aligning it left a four-cell
        // notch and shoved the whole number visibly off-centre
        // against the caption below.
        '1' => [" # ", " # ", " # ", " # ", " # "],
        '2' => ["###", "  #", "###", "#  ", "###"],
        '3' => ["###", "  #", "###", "  #", "###"],
        '4' => ["# #", "# #", "###", "  #", "  #"],
        '5' => ["###", "#  ", "###", "  #", "###"],
        '6' => ["###", "#  ", "###", "# #", "###"],
        '7' => ["###", "  #", "  #", "  #", "  #"],
        '8' => ["###", "# #", "###", "# #", "###"],
        '9' => ["###", "# #", "###", "  #", "###"],
        ':' => ["   ", " # ", "   ", " # ", "   "],
        _ => return None,
    })
}

/// Cell size of one painted glyph "pixel", as (x, y).
///
/// x is twice y throughout, which is not a style choice: a terminal
/// cell is about twice as tall as it is wide, so a 3x5 glyph painted
/// one cell per pixel is physically 3 wide and 10 tall, a sliver.
/// Doubling x restores the 3:5 proportion a digit is supposed to have.
/// `(1, 1)` is the squeezed fallback for a rect too narrow for the
/// honest one, which is still far better than no countdown.
const SCALES: [(u16, u16); 3] = [(4, 2), (2, 1), (1, 1)];

/// Cells a string of `n` glyphs needs at `scale`: each glyph is
/// `GLYPH_W` pixels with one pixel of space after it, and the trailing
/// space is not drawn.
fn text_cells(n: usize, scale: (u16, u16)) -> (u16, u16) {
    let per = GLYPH_W as u16 + 1;
    let w = (per * n as u16).saturating_sub(1) * scale.0;
    (w, GLYPH_H as u16 * scale.1)
}

/// The largest scale whose digits fit, or `None` when even the
/// smallest does not and the caller should print plain text.
///
/// The caption is deliberately *not* part of this: it is dropped
/// independently when there is no row for it, because the number is
/// what was asked for and the word is only a label. Requiring both
/// would shrink the digits to keep a caption, which is backwards.
fn fit(n: usize, area: Rect) -> Option<(u16, u16)> {
    SCALES.into_iter().find(|scale| {
        let (w, h) = text_cells(n, *scale);
        w <= area.width && h <= area.height
    })
}

const CAPTION: &str = "remaining";

pub struct GapPanel {
    pub gap: Gap,
}

impl Widget for GapPanel {
    fn render(self, area: Rect, buf: &mut Buffer) {
        if area.width == 0 || area.height == 0 {
            return;
        }
        // the same `m:ss` the scrubber's readouts use, so the big
        // number and the small one are never in different formats.
        let text = fmt_time(Some(self.gap.remaining_secs.max(0.0)));
        let glyphs: Vec<[&str; GLYPH_H]> = text.chars().filter_map(glyph).collect();

        let Some(scale) = fit(glyphs.len(), area).filter(|_| !glyphs.is_empty()) else {
            // no room for line art: the plain readout, centred, which
            // is still the answer to the only question being asked.
            draw_centered(
                buf,
                area,
                area.y + area.height / 2,
                &text,
                shade(NEAR_WHITE),
            );
            return;
        };

        let (w, h) = text_cells(glyphs.len(), scale);
        // the caption rides under the digits only when there is a row
        // to spare for it plus one to breathe; the block is centred as
        // a whole either way, so it never sits lopsided.
        let caption = area.height >= h + 2 && width(CAPTION) as u16 <= area.width;
        let block_h = h + if caption { 2 } else { 0 };
        let x0 = area.x + (area.width - w) / 2;
        let y0 = area.y + (area.height - block_h) / 2;

        let on = shade(NEAR_WHITE);
        for (i, g) in glyphs.iter().enumerate() {
            let gx = x0 + i as u16 * (GLYPH_W as u16 + 1) * scale.0;
            for (row, line) in g.iter().enumerate() {
                for (col, ch) in line.chars().enumerate() {
                    if ch != '#' {
                        continue;
                    }
                    // one glyph pixel is a scale.0 x scale.1 block of
                    // cells, so the shape scales without resampling.
                    for dy in 0..scale.1 {
                        for dx in 0..scale.0 {
                            let (x, y) = (
                                gx + col as u16 * scale.0 + dx,
                                y0 + row as u16 * scale.1 + dy,
                            );
                            if x < area.right() && y < area.bottom() {
                                buf[(x, y)].set_char('█').set_fg(on);
                            }
                        }
                    }
                }
            }
        }

        if caption {
            draw_centered(buf, area, y0 + h + 1, CAPTION, shade(NEAR_BLACK));
        }
    }
}

/// Centred on display width within `area`, clipped to it. A local copy
/// rather than `player`'s private one; this widget is handed its own
/// rect and centres inside that, where `player` centres inside the
/// whole pane.
fn draw_centered(buf: &mut Buffer, area: Rect, y: u16, text: &str, color: ratatui::style::Color) {
    if y < area.y || y >= area.bottom() {
        return;
    }
    let w = width(text) as u16;
    if w > area.width {
        return;
    }
    // one cell per char: everything this draws is ascii (the digits'
    // plain-text fallback and the caption), so there is no wide glyph
    // to advance two for.
    for (x, ch) in (area.x + (area.width - w) / 2..).zip(text.chars()) {
        if x >= area.right() {
            break;
        }
        buf[(x, y)].set_char(ch).set_fg(color);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::testbuf::{assert_outside_untouched, buffer, grid};

    fn render(w: u16, h: u16, remaining: f64) -> Buffer {
        let mut buf = buffer(w, h);
        let area = Rect::new(0, 0, w, h);
        GapPanel {
            gap: Gap {
                remaining_secs: remaining,
            },
        }
        .render(area, &mut buf);
        buf
    }

    #[test]
    fn every_glyph_is_the_declared_size() {
        for ch in "0123456789:".chars() {
            let g = glyph(ch).unwrap_or_else(|| panic!("{ch} has no glyph"));
            for line in g {
                assert_eq!(
                    line.chars().count(),
                    GLYPH_W,
                    "{ch} has a row of the wrong width"
                );
            }
        }
    }

    /// Only `#` and space, or the painter would skip cells it was
    /// meant to fill and the digit would come out broken.
    #[test]
    fn glyphs_contain_nothing_but_on_and_off() {
        for ch in "0123456789:".chars() {
            for line in glyph(ch).unwrap() {
                assert!(
                    line.chars().all(|c| c == '#' || c == ' '),
                    "{ch} has a cell that is neither on nor off"
                );
            }
        }
    }

    #[test]
    fn the_countdown_is_drawn_large() {
        // 15:00 at the honest scale is 38x5; this rect has room.
        let buf = render(44, 12, 900.0);
        let painted = grid(&buf).chars().filter(|c| *c == '█').count();
        assert!(painted > 40, "a five-glyph countdown paints many cells");
    }

    /// The caption is what tells you the big number counts *down*; a
    /// bare number could be read as elapsed.
    #[test]
    fn the_caption_rides_under_the_digits_when_there_is_room() {
        let buf = render(44, 12, 900.0);
        let text = grid(&buf);
        assert!(text.contains("remaining"), "{text}");
    }

    /// And is dropped rather than shrinking the number, which is the
    /// part being asked for.
    #[test]
    fn a_short_rect_keeps_the_digits_and_drops_the_caption() {
        let buf = render(44, 5, 900.0);
        let text = grid(&buf);
        assert!(!text.contains("remaining"), "{text}");
        assert!(text.contains('█'), "the digits survive");
    }

    /// A rect too narrow for even the squeezed line art still answers
    /// the question, in plain text.
    #[test]
    fn a_tiny_rect_falls_back_to_the_plain_readout() {
        let buf = render(10, 3, 900.0);
        let text = grid(&buf);
        assert!(!text.contains('█'), "no line art at this size");
        assert!(text.contains("15:00"), "{text}");
    }

    #[test]
    fn nothing_is_painted_outside_the_rect() {
        let mut buf = buffer(40, 14);
        let area = Rect::new(4, 3, 30, 8);
        GapPanel {
            gap: Gap {
                remaining_secs: 61.0,
            },
        }
        .render(area, &mut buf);
        assert_outside_untouched(&buf, area);
    }

    /// Zero is a real state (the gap just ended) and must not render
    /// as a negative or blank.
    #[test]
    fn a_finished_gap_reads_zero() {
        let buf = render(44, 12, -0.4);
        let text = grid(&buf);
        assert!(!text.contains('-'), "never a negative countdown");
        assert!(text.contains('█'), "0:00 still draws");
    }

    #[test]
    fn the_honest_scale_is_twice_as_wide_as_tall_per_pixel() {
        // the reason the ladder exists: a 1:1 pixel makes a digit a
        // sliver, because a cell is about twice as tall as it is wide.
        for (x, y) in SCALES {
            assert!(x >= y, "a pixel is never taller than it is wide");
        }
        assert_eq!(SCALES[1], (2, 1), "the proportionally correct one");
    }

    #[test]
    fn a_wider_scale_is_chosen_when_it_fits() {
        let big = fit(5, Rect::new(0, 0, 200, 40)).expect("fits");
        let mid = fit(5, Rect::new(0, 0, 44, 12)).expect("fits");
        assert_eq!(big, (4, 2));
        assert_eq!(mid, (2, 1));
    }
}

#[cfg(test)]
mod eyeball {
    use super::*;
    use crate::ui::testbuf::{buffer, grid};

    /// Prints the panel at three sizes so the glyphs can actually be
    /// read rather than inferred from a cell count:
    /// `cargo test --lib eyeball -- --nocapture`. This is how the `1`
    /// glyph's alignment bug was found, which no assertion here would
    /// have caught.
    ///
    /// It asserts the floor too, so it is a test and not just a
    /// printer: every size has to draw *something*.
    #[test]
    fn print_the_panel() {
        for (w, h, secs) in [(44u16, 12u16, 900.0), (24, 9, 95.0), (12, 4, 900.0)] {
            let mut buf = buffer(w, h);
            GapPanel {
                gap: Gap {
                    remaining_secs: secs,
                },
            }
            .render(Rect::new(0, 0, w, h), &mut buf);
            let text = grid(&buf);
            println!("--- {w}x{h} @ {secs}s\n{text}");
            assert!(
                text.chars().any(|c| c != '.' && c != '\n'),
                "{w}x{h} drew nothing at all"
            );
        }
    }
}
