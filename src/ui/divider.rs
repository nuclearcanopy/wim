use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::widgets::Widget;

use crate::theme::{shade, NEAR_BLACK};

/// A single-line unicode horizontal rule (`─`), not a stock bordered
/// widget; a deliberate, minimal line-drawing accent rather than
/// boxing every region in. Deliberately much dimmer than any text
/// (NEAR_BLACK, not DIM): a divider that sits at the same brightness
/// as the content it separates reads as visual noise, not structure.
pub struct HorizontalRule;

impl Widget for HorizontalRule {
    fn render(self, area: Rect, buf: &mut Buffer) {
        if area.height == 0 {
            return;
        }
        for x in area.x..area.x + area.width {
            if let Some(cell) = buf.cell_mut((x, area.y)) {
                cell.set_char('─');
                cell.set_fg(shade(NEAR_BLACK));
            }
        }
    }
}
