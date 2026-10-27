use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::widgets::Widget;

use crate::theme::{shade, DIM, NEAR_BLACK, NEAR_WHITE};
use crate::ui::textfit::tail_fit;

/// A hand-drawn box around the search input: "search..." as a dim
/// placeholder when empty, replaced by the live query once typing
/// starts.
pub struct SearchBox<'a> {
    pub query: &'a str,
    pub editing: bool,
    /// Blink phase for the terminal-style cursor block; only matters
    /// while `editing`. The caller toggles this on a timer so the
    /// widget itself stays a pure render of whatever state it's given.
    pub cursor_visible: bool,
    /// `false` only for the sidebar instance, whose right edge is
    /// closed by the vertical divider sitting one column past `area`
    /// (see `ui::junctions`); a second parallel line right next to
    /// that divider read as a box bolted onto the layout. Anything
    /// else has nothing to its right, so it closes its own edge
    /// rather than trailing off into the terminal.
    pub right_border: bool,
}

impl Widget for SearchBox<'_> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        if area.width < 3 || area.height < 3 {
            return;
        }
        let border_color = shade(NEAR_BLACK);
        let w = area.width;
        let h = area.height;

        put(buf, area.x, area.y, '┌', border_color);
        put(buf, area.x, area.y + h - 1, '└', border_color);
        for x in 1..w {
            put(buf, area.x + x, area.y, '─', border_color);
            put(buf, area.x + x, area.y + h - 1, '─', border_color);
        }
        for y in 1..h - 1 {
            put(buf, area.x, area.y + y, '│', border_color);
        }
        if self.right_border {
            put(buf, area.x + w - 1, area.y, '┐', border_color);
            put(buf, area.x + w - 1, area.y + h - 1, '┘', border_color);
            for y in 1..h - 1 {
                put(buf, area.x + w - 1, area.y + y, '│', border_color);
            }
        }

        // the left border column always costs one, the right one only
        // when it's actually drawn.
        let inner_width = w.saturating_sub(if self.right_border { 2 } else { 1 }) as usize;
        // the placeholder only makes sense when there's nothing typed
        // *and* nothing being typed; once editing starts on an empty
        // query, the blinking cursor alone is the "you're in here" signal,
        // not placeholder text fighting it for the same cell.
        let (text, color) = if self.query.is_empty() && !self.editing {
            ("search...".to_string(), shade(DIM))
        } else {
            (tail_fit(self.query, inner_width), shade(NEAR_WHITE))
        };
        for (col, ch) in text.chars().enumerate() {
            if col >= inner_width {
                break;
            }
            put(buf, area.x + 1 + col as u16, area.y + 1, ch, color);
        }

        if self.editing && self.cursor_visible {
            let cursor_col = text.chars().count().min(inner_width.saturating_sub(1));
            put(
                buf,
                area.x + 1 + cursor_col as u16,
                area.y + 1,
                '█',
                shade(NEAR_WHITE),
            );
        }
    }
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

    fn render(query: &str, right_border: bool) -> String {
        let mut buf = buffer(10, 3);
        SearchBox {
            query,
            editing: false,
            cursor_visible: false,
            right_border,
        }
        .render(Rect::new(0, 0, 10, 3), &mut buf);
        grid(&buf)
    }

    /// The sidebar instance: open on the right on purpose, the
    /// vertical divider one column over closes it.
    #[test]
    fn open_right_edge_leaves_the_border_unterminated() {
        assert_eq!(
            render("", false),
            ["┌─────────", "│search...", "└─────────"].join("\n")
        );
    }

    /// Standalone instance (the artist profile's song filter): nothing
    /// is to its right, so it closes its own corners.
    #[test]
    fn closed_right_edge_draws_its_own_corners() {
        assert_eq!(
            render("", true),
            ["┌────────┐", "│search..│", "└────────┘"].join("\n")
        );
    }

    #[test]
    fn closed_right_edge_keeps_the_query_inside_the_box() {
        assert_eq!(
            render("abcdefghij", true),
            ["┌────────┐", "│cdefghij│", "└────────┘"].join("\n")
        );
    }
}
