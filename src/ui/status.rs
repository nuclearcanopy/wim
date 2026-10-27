use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::widgets::Widget;

use crate::theme::{shade, DIM, NEAR_WHITE};
use crate::ui::textfit::{tail_fit, truncate_with_ellipsis};

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
        let width = area.width as usize;
        // actively-typed text keeps its tail visible (the cursor end);
        // static messages truncate with an ellipsis instead; same
        // distinction as everywhere else text might overflow the
        // terminal width. see ui/textfit.rs.
        let text = if self.editing_query {
            tail_fit(&format!("/{}", self.query), width)
        } else {
            truncate_with_ellipsis(self.message, width)
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
