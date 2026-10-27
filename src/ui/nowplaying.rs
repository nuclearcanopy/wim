use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::widgets::Widget;

use crate::theme::{shade, DIM, NEAR_WHITE};
use crate::ui::textfit::truncate_with_ellipsis;

/// Main-content placeholder for whatever's currently playing. Minimal
/// for now; this is where an artist profile / album view takes over
/// once those exist (see ROADMAP.md) instead of just a centered label.
pub struct NowPlayingView<'a> {
    pub label: &'a str,
}

impl Widget for NowPlayingView<'_> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        draw_centered(buf, area, area.y + area.height / 2, "now playing", shade(DIM));
        if area.height > 0 {
            draw_centered(buf, area, area.y + area.height / 2 + 1, self.label, shade(NEAR_WHITE));
        }
    }
}

fn draw_centered(buf: &mut Buffer, area: Rect, y: u16, text: &str, color: ratatui::style::Color) {
    if y >= area.y + area.height {
        return;
    }
    let text = truncate_with_ellipsis(text, area.width as usize);
    let text_width = text.chars().count() as u16;
    let start_x = area.x + area.width.saturating_sub(text_width) / 2;
    for (i, ch) in text.chars().enumerate() {
        let x = start_x + i as u16;
        if x >= area.x + area.width {
            break;
        }
        if let Some(cell) = buf.cell_mut((x, y)) {
            cell.set_char(ch);
            cell.set_fg(color);
        }
    }
}
