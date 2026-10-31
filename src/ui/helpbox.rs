use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::widgets::Widget;

use crate::theme::{shade, BRIGHT, DIM, NEAR_BLACK};
use crate::ui::textfit::{truncate_with_ellipsis, width as text_width};

/// Flush column in the bottom-right corner hinting at the key that
/// opens the full keybind reference, instead of cramming every
/// keybind permanently into the bottom bar's text. Its own inset
/// ┌─┐/└─┘ border used to eat the first and last row of an already
/// tight area, leaving it looking like a small disconnected box
/// floating inside the bar; just a flush `│` left edge (matching the
/// `┬` junction in the horizontal rule above it, see `main.rs`) and a
/// centered label filling the entire given height reads as part of
/// the bar instead.
pub struct HelpBox<'a> {
    pub label: &'a str,
}

/// Columns this box needs to show `label` without clipping: the label
/// itself, the flush `│` edge, and a cell of padding either side. The
/// caller used to carve a hardcoded 10, which was fine for "? help"
/// and cut "space help" down to "space hel" the moment the help key
/// was rebound to something longer than one character.
pub fn width_for(label: &str) -> u16 {
    crate::ui::textfit::width(label) as u16 + 3
}

impl Widget for HelpBox<'_> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        if area.width == 0 || area.height == 0 {
            return;
        }
        let color = shade(NEAR_BLACK);
        for y in area.y..area.y + area.height {
            put(buf, area.x, y, '│', color);
        }
        // top-biased centre: at an even height `height / 2` put the
        // label on the bottom row, so a two-row bar read as a label
        // hugging the frame edge with the blank row above it.
        let text_y = area.y + (area.height - 1) / 2;
        draw_label(
            buf,
            area.x + 1,
            text_y,
            area.width.saturating_sub(1),
            self.label,
        );
    }
}

/// The key bright and the word dim: the key is the actionable half,
/// and the controls bar immediately left of this already spells active
/// as BRIGHT and inert as DIM. Ellipsised rather than hard-cut, since
/// a clipped "? h" reads as a label of its own rather than a cut one.
fn draw_label(buf: &mut Buffer, x: u16, y: u16, width: u16, label: &str) {
    let avail = width as usize;
    if avail == 0 {
        return;
    }
    let fitted = truncate_with_ellipsis(label, avail);
    let pad = avail.saturating_sub(text_width(&fitted)) / 2;
    // split at the *last* space: a key rebound to "space" is one key,
    // not a key and a word.
    let (key, rest) = match fitted.rfind(' ') {
        Some(i) => fitted.split_at(i),
        None => (fitted.as_str(), ""),
    };
    let budget = avail - pad;
    let after = crate::ui::draw::at(buf, x + pad as u16, y, key, shade(BRIGHT), budget);
    crate::ui::draw::at(
        buf,
        after,
        y,
        rest,
        shade(DIM),
        budget.saturating_sub(text_width(key)),
    );
}

fn put(buf: &mut Buffer, x: u16, y: u16, ch: char, color: ratatui::style::Color) {
    if let Some(cell) = buf.cell_mut((x, y)) {
        cell.set_char(ch);
        cell.set_fg(color);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::testbuf::{buffer, grid};

    fn render(label: &str, w: u16) -> String {
        let area = Rect::new(0, 0, w, 2);
        let mut buf = buffer(w, 2);
        HelpBox { label }.render(area, &mut buf);
        grid(&buf)
    }

    #[test]
    fn width_grows_with_the_label() {
        assert_eq!(width_for("? help"), 9);
        assert_eq!(width_for("space help"), 13);
    }

    #[test]
    fn a_rebound_help_key_is_not_clipped() {
        // the old hardcoded 10 columns cut this to "space hel".
        let w = width_for("space help");
        let out = render("space help", w);
        assert!(out.contains("space help"), "clipped at {w} cols:\n{out}");
    }

    #[test]
    fn the_default_label_still_fits_its_own_width() {
        let out = render("? help", width_for("? help"));
        assert!(out.contains("? help"), "{out}");
    }

    #[test]
    fn the_flush_left_edge_is_drawn_on_every_row() {
        let out = render("? help", width_for("? help"));
        for line in out.lines() {
            assert!(line.starts_with('│'), "missing edge: {line:?}");
        }
    }

    /// A hard cut turned "? help" into "? h", which reads as a label
    /// in its own right rather than a clipped one; the `…` is what
    /// says the rest is there.
    #[test]
    fn a_label_too_wide_for_the_bar_says_so_with_an_ellipsis() {
        // from 2 (one cell of label past the `│`) up to the last width
        // too narrow to hold the whole thing.
        for w in 2..width_for("? help") - 2 {
            let out = render("? help", w);
            assert!(
                out.contains('…'),
                "clipped to {w} cols with no ellipsis:\n{out}"
            );
        }
    }

    /// `height / 2` landed on the bottom row at an even height, so a
    /// two-row bar had the label against its own bottom edge with the
    /// blank row above it.
    #[test]
    fn the_label_never_sits_below_the_midline() {
        for h in 1..=9u16 {
            let area = Rect::new(0, 0, width_for("? help"), h);
            let mut buf = buffer(area.width, h);
            HelpBox { label: "? help" }.render(area, &mut buf);
            let row = grid(&buf)
                .lines()
                .position(|l| l.contains('?'))
                .unwrap_or_else(|| panic!("no label at height {h}"));
            let above = row;
            let below = (h as usize - 1) - row;
            assert!(
                above <= below,
                "height {h}: label on row {row} leaves {above} blank above and {below} below"
            );
        }
    }

    /// Hierarchy, not decoration: the key is the half you act on, and
    /// one flat shade across both made the box read as a sentence
    /// rather than a binding.
    #[test]
    fn the_key_is_brighter_than_the_word() {
        for label in ["? help", "space help", "f5 help"] {
            let w = width_for(label);
            let area = Rect::new(0, 0, w, 3);
            let mut buf = buffer(w, 3);
            HelpBox { label }.render(area, &mut buf);
            // char positions, not byte offsets: the `│` edge is three
            // bytes and `str::find` would report the label two cells
            // right of where it was painted.
            let row: Vec<char> = grid(&buf).lines().nth(1).unwrap().chars().collect();
            let key_chars: Vec<char> = label.chars().collect();
            let start = row
                .windows(key_chars.len())
                .position(|w| w == key_chars.as_slice())
                .expect("the whole label fits its own width") as u16;
            let word = start + text_width(label.split(' ').next().unwrap()) as u16 + 1;
            let fg = |x: u16| buf.cell((x, 1)).unwrap().fg;
            assert_eq!(fg(start), shade(BRIGHT), "{label:?}: key not bright");
            assert_eq!(
                fg(word),
                shade(DIM),
                "{label:?}: the word is as bright as the key"
            );
        }
    }

    #[test]
    fn a_label_wider_than_the_area_clips_instead_of_panicking() {
        let out = render("an absurdly long help label", 8);
        assert_eq!(out.lines().count(), 2);
    }
}

#[cfg(test)]
mod eyeball {
    use super::*;
    use crate::ui::testbuf::{buffer, grid};

    /// Prints the box at every height the bottom bar can be resized to
    /// plus the clipped cases, because this one is read, not measured:
    /// `cargo test --lib ui::helpbox::eyeball -- --nocapture`.
    /// Asserts the floor too, so it is a test and not just a printer.
    #[test]
    fn print_the_box() {
        for (label, w, h) in [
            ("? help", width_for("? help"), 3u16),
            ("? help", width_for("? help"), 1),
            ("? help", width_for("? help"), 2),
            ("? help", width_for("? help"), 4),
            ("? help", width_for("? help"), 9),
            ("space help", width_for("space help"), 3),
            ("? help", 4, 3),
            ("? help", 1, 3),
        ] {
            let area = Rect::new(0, 0, w, h);
            let mut buf = buffer(w, h);
            HelpBox { label }.render(area, &mut buf);
            let text = grid(&buf);
            println!("--- {label:?} at {w}x{h}\n{text}");
            assert!(
                text.chars().any(|c| c != '.' && c != '\n'),
                "{label:?} at {w}x{h} drew nothing at all"
            );
        }
    }
}
