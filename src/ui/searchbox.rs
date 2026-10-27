use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::widgets::Widget;

use crate::theme::{shade, DIM, NEAR_BLACK, NEAR_WHITE};
use crate::ui::textfit::tail_fit;

/// A hand-drawn box (┌─┐│└┘, not a stock `Block`) around the search
/// input: "search..." as a dim placeholder when empty, replaced by the
/// live query once typing starts.
pub struct SearchBox<'a> {
    pub query: &'a str,
    pub editing: bool,
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
        put(buf, area.x + w - 1, area.y, '┐', border_color);
        put(buf, area.x, area.y + h - 1, '└', border_color);
        put(buf, area.x + w - 1, area.y + h - 1, '┘', border_color);
        for x in 1..w - 1 {
            put(buf, area.x + x, area.y, '─', border_color);
            put(buf, area.x + x, area.y + h - 1, '─', border_color);
        }
        for y in 1..h - 1 {
            put(buf, area.x, area.y + y, '│', border_color);
            put(buf, area.x + w - 1, area.y + y, '│', border_color);
        }

        let inner_width = w.saturating_sub(2) as usize;
        let (text, color) = if self.query.is_empty() {
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
    }
}

fn put(buf: &mut Buffer, x: u16, y: u16, ch: char, color: ratatui::style::Color) {
    if let Some(cell) = buf.cell_mut((x, y)) {
        cell.set_char(ch);
        cell.set_fg(color);
    }
}
