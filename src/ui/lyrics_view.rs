use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::widgets::Widget;

use crate::theme::{shade, DIM};

/// Simple scrollable plain-text view, no stock `Paragraph`/wrapping
/// widget — lines are pre-split by the caller and indexed directly.
pub struct LyricsView<'a> {
    pub lines: &'a [&'a str],
    pub scroll: usize,
}

impl Widget for LyricsView<'_> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        if area.height == 0 {
            return;
        }
        for row in 0..area.height as usize {
            let Some(line) = self.lines.get(self.scroll + row) else {
                break;
            };
            let y = area.y + row as u16;
            for (col, ch) in line.chars().enumerate() {
                let x = area.x + col as u16;
                if x >= area.x + area.width {
                    break;
                }
                if let Some(cell) = buf.cell_mut((x, y)) {
                    cell.set_char(ch);
                    cell.set_fg(shade(DIM));
                }
            }
        }
    }
}
