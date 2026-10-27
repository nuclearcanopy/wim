use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::widgets::Widget;

use crate::theme::{shade, DIM, NEAR_BLACK, NEAR_WHITE};
use crate::ui::textfit::tail_fit;

/// A hand-drawn box around the search input: "search..." as a dim
/// placeholder when empty, replaced by the live query once typing
/// starts. Deliberately open on the right: the sidebar's own vertical
/// divider sits one column past `area`'s right edge already, so a
/// second parallel line right next to it just looked like a
/// disconnected box bolted onto the layout rather than part of it.
/// Closing this side is main.rs's job, via the vdivider's own
/// `junction_rows` at this box's top/bottom rows (┬ and ┤), the same
/// way every other junction this session got wired up.
pub struct SearchBox<'a> {
    pub query: &'a str,
    pub editing: bool,
    /// Blink phase for the terminal-style cursor block; only matters
    /// while `editing`. The caller toggles this on a timer so the
    /// widget itself stays a pure render of whatever state it's given.
    pub cursor_visible: bool,
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

        // one border column on the left only now; no right border to
        // reserve a second column for.
        let inner_width = w.saturating_sub(1) as usize;
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
