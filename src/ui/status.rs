use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::widgets::Widget;

use crate::theme::{shade, DIM, NEAR_WHITE};

/// One-line status bar: the live search query while editing, or a
/// now-playing/idle message otherwise. Custom-painted, no stock widget.
pub struct StatusBar<'a> {
    pub editing_query: bool,
    pub query: &'a str,
    pub message: &'a str,
}

impl Widget for StatusBar<'_> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        if area.height == 0 {
            return;
        }
        let text = if self.editing_query {
            format!("/{}", self.query)
        } else {
            self.message.to_string()
        };
        let color = if self.editing_query { shade(NEAR_WHITE) } else { shade(DIM) };

        for (col, ch) in text.chars().enumerate() {
            let x = area.x + col as u16;
            if x >= area.x + area.width {
                break;
            }
            if let Some(cell) = buf.cell_mut((x, area.y)) {
                cell.set_char(ch);
                cell.set_fg(color);
            }
        }
    }
}
