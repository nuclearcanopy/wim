use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::widgets::Widget;

use crate::theme::{shade, DIM, NEAR_BLACK};

/// Small bordered box in the bottom-right corner hinting at the key
/// that opens the full keybind reference, instead of cramming every
/// keybind permanently into the bottom bar's text. Its top-left
/// corner is a `┬` junction into the horizontal rule above it (see
/// `main.rs`), not a floating box with no connection to anything.
pub struct HelpBox<'a> {
    pub label: &'a str,
}

impl Widget for HelpBox<'_> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        if area.width < 3 {
            return;
        }
        let color = shade(NEAR_BLACK);
        let w = area.width;
        let h = area.height;

        if h < 2 {
            // not enough room for any border at all; just the text,
            // better than nothing.
            draw_text(buf, area.x, area.y, w, self.label, shade(DIM));
            return;
        }

        put(buf, area.x, area.y, '┌', color);
        put(buf, area.x + w - 1, area.y, '┐', color);
        put(buf, area.x, area.y + h - 1, '└', color);
        put(buf, area.x + w - 1, area.y + h - 1, '┘', color);
        for x in 1..w - 1 {
            put(buf, area.x + x, area.y, '─', color);
            put(buf, area.x + x, area.y + h - 1, '─', color);
        }
        for y in 1..h - 1 {
            put(buf, area.x, area.y + y, '│', color);
            put(buf, area.x + w - 1, area.y + y, '│', color);
        }

        if h >= 3 {
            let text_y = area.y + h / 2;
            draw_text(
                buf,
                area.x + 1,
                text_y,
                w.saturating_sub(2),
                self.label,
                shade(DIM),
            );
        }
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
    let text_width = text.chars().count() as u16;
    let start_x = x + width.saturating_sub(text_width) / 2;
    for (i, ch) in text.chars().enumerate() {
        let cell_x = start_x + i as u16;
        if cell_x >= x + width {
            break;
        }
        if let Some(cell) = buf.cell_mut((cell_x, y)) {
            cell.set_char(ch);
            cell.set_fg(color);
        }
    }
}

fn put(buf: &mut Buffer, x: u16, y: u16, ch: char, color: ratatui::style::Color) {
    if let Some(cell) = buf.cell_mut((x, y)) {
        cell.set_char(ch);
        cell.set_fg(color);
    }
}
