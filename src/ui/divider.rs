use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::widgets::Widget;

use crate::theme::{shade, DIM, NEAR_BLACK};

/// A single-line unicode horizontal rule (`─`), not a stock bordered
/// widget; a deliberate, minimal line-drawing accent rather than
/// boxing every region in. Deliberately much dimmer than any text
/// (NEAR_BLACK, not DIM) by default: a divider that sits at the same
/// brightness as the content it separates reads as visual noise, not
/// structure. `highlighted` brightens it to DIM, used as the resize-mode
/// selection cue for whichever panel this divider borders.
pub struct HorizontalRule {
    pub highlighted: bool,
}

impl Widget for HorizontalRule {
    fn render(self, area: Rect, buf: &mut Buffer) {
        if area.height == 0 {
            return;
        }
        let color = shade(if self.highlighted { DIM } else { NEAR_BLACK });
        for x in area.x..area.x + area.width {
            if let Some(cell) = buf.cell_mut((x, area.y)) {
                cell.set_char('─');
                cell.set_fg(color);
            }
        }
    }
}

/// Vertical counterpart (`│`), same highlight convention.
pub struct VerticalRule {
    pub highlighted: bool,
}

impl Widget for VerticalRule {
    fn render(self, area: Rect, buf: &mut Buffer) {
        if area.width == 0 {
            return;
        }
        let color = shade(if self.highlighted { DIM } else { NEAR_BLACK });
        for y in area.y..area.y + area.height {
            if let Some(cell) = buf.cell_mut((area.x, y)) {
                cell.set_char('│');
                cell.set_fg(color);
            }
        }
    }
}
