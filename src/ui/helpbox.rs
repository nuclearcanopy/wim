use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::widgets::Widget;

use crate::theme::{shade, DIM, NEAR_BLACK};

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
        let text_y = area.y + area.height / 2;
        draw_text(
            buf,
            area.x + 1,
            text_y,
            area.width.saturating_sub(1),
            self.label,
            shade(DIM),
        );
    }
}

fn draw_text(
    buf: &mut Buffer,
    x: u16,
    y: u16,
    width: u16,
    text: &str,
    color: ratatui::style::Color,
) {
    crate::ui::draw::centered(
        buf,
        Rect {
            x,
            y,
            width,
            height: 1,
        },
        y,
        text,
        color,
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

    #[test]
    fn a_label_wider_than_the_area_clips_instead_of_panicking() {
        let out = render("an absurdly long help label", 8);
        assert_eq!(out.lines().count(), 2);
    }
}
