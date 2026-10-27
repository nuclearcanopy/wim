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
