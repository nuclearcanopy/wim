use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::widgets::Widget;

use crate::notify::{Notification, Severity};
use crate::theme::{shade, BRIGHT, MID, WHITE};
use crate::ui::textfit::truncate_with_ellipsis;

/// Bordered boxes stacked in the top-right corner, newest on top,
/// rendered last so they sit over everything else (sidebar, main
/// content, bottom bar) without disturbing their layout; each only
/// touches the cells inside its own box.
pub struct NotificationOverlay<'a> {
    pub items: &'a [Notification],
}

impl Widget for NotificationOverlay<'_> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        let mut y = area.y;
        for n in self.items.iter().rev() {
            if y + 3 > area.y + area.height {
                break;
            }

            let text = format!("[{}] {}", n.severity.icon(), n.message);
            let max_inner = (area.width as usize).saturating_sub(4);
            let text = truncate_with_ellipsis(&text, max_inner.max(1));
            let inner_width = (text.chars().count() as u16).max(4);
            let box_width = inner_width + 2;
            let start_x = area.x + area.width.saturating_sub(box_width);

            // brightness carries severity, not color (monochrome
            // rule): error is the most prominent, info the least.
            let color = shade(match n.severity {
                Severity::Info => MID,
                Severity::Alert => BRIGHT,
                Severity::Error => WHITE,
            });

            draw_box(buf, start_x, y, box_width, &text, &n.footer(), color);
            y += 4; // box height (3) + one row of gap before the next
        }
    }
}

fn draw_box(
    buf: &mut Buffer,
    x: u16,
    y: u16,
    width: u16,
    text: &str,
    footer: &str,
    color: ratatui::style::Color,
) {
    put(buf, x, y, '┌', color);
    put(buf, x + width - 1, y, '┐', color);
    for col in 1..width - 1 {
        put(buf, x + col, y, '─', color);
    }

    put(buf, x, y + 1, '│', color);
    put(buf, x + width - 1, y + 1, '│', color);
    for (i, ch) in text.chars().enumerate() {
        put(buf, x + 1 + i as u16, y + 1, ch, color);
    }

    // the countdown (or, for a download in progress, a live percentage)
    // lives in the bottom-right corner of the border itself, not a
    // separate line, so the box stays exactly 3 rows regardless of how
    // much time is left / how far along the download is.
    put(buf, x, y + 2, '└', color);
    put(buf, x + width - 1, y + 2, '┘', color);
    let footer_width = footer.chars().count() as u16;
    let footer_start = (width - 1).saturating_sub(footer_width);
    for col in 1..width - 1 {
        let in_footer = col >= footer_start && col - footer_start < footer_width;
        let ch = if in_footer {
            footer.chars().nth((col - footer_start) as usize).unwrap()
        } else {
            '─'
        };
        put(buf, x + col, y + 2, ch, color);
    }
}

fn put(buf: &mut Buffer, x: u16, y: u16, ch: char, color: ratatui::style::Color) {
    crate::ui::draw::put(buf, x, y, ch, color);
}
