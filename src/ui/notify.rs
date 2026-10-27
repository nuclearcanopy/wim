use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::widgets::Widget;

use crate::notify::{Notification, Severity};
use crate::theme::{shade, BRIGHT, MID, WHITE};
use crate::ui::textfit::truncate_with_ellipsis;

/// Stacked in the top-right corner, newest on top, rendered last so it
/// sits over everything else (sidebar, main content, bottom bar) without
/// disturbing their layout; it only touches the cells it actually draws.
pub struct NotificationOverlay<'a> {
    pub items: &'a [Notification],
}

impl Widget for NotificationOverlay<'_> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        for (row, n) in self.items.iter().rev().enumerate() {
            let y = area.y + row as u16;
            if y >= area.y + area.height {
                break;
            }
            let text = format!("[{}] {}", n.severity.icon(), n.message);
            let max_width = (area.width as usize).saturating_sub(2);
            let text = truncate_with_ellipsis(&text, max_width);
            let text_width = text.chars().count() as u16;
            let start_x = area.x + area.width.saturating_sub(text_width + 1);

            // brightness carries severity, not color (monochrome rule):
            // error is the most prominent, info the least.
            let level = match n.severity {
                Severity::Info => MID,
                Severity::Alert => BRIGHT,
                Severity::Error => WHITE,
            };
            let color = shade(level);
            for (col, ch) in text.chars().enumerate() {
                let x = start_x + col as u16;
                if x >= area.x + area.width {
                    break;
                }
                if let Some(cell) = buf.cell_mut((x, y)) {
                    cell.set_char(ch);
                    cell.set_fg(color);
                }
            }
        }
    }
}
