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
    /// Absolute buffer x-coordinate of a vertical divider ending right
    /// above this rule (e.g. the sidebar/main-content split meeting the
    /// bottom bar), if any; drawn as `┴` there instead of `─` so the
    /// two lines actually join instead of crossing with a gap. This is
    /// not optional polish: two dividers that are structurally meant to
    /// meet but are drawn by two widgets with zero knowledge of each
    /// other will visibly not connect unless something tells them to.
    pub junction_x: Option<u16>,
}

impl Widget for HorizontalRule {
    fn render(self, area: Rect, buf: &mut Buffer) {
        if area.height == 0 {
            return;
        }
        let color = shade(if self.highlighted { DIM } else { NEAR_BLACK });
        for x in area.x..area.x + area.width {
            let ch = if self.junction_x == Some(x) {
                '┴'
            } else {
                '─'
            };
            if let Some(cell) = buf.cell_mut((x, area.y)) {
                cell.set_char(ch);
                cell.set_fg(color);
            }
        }
    }
}

/// Vertical counterpart (`│`), same highlight convention.
pub struct VerticalRule {
    pub highlighted: bool,
    /// Absolute buffer y-coordinates where something on the other side
    /// (main content, e.g. the banner's own internal rules) draws a
    /// horizontal line that starts right at this divider; drawn as `├`
    /// there instead of a plain `│`, same reasoning as `HorizontalRule`'s
    /// `junction_x`: two lines meeting from different widgets don't
    /// connect on their own.
    pub junction_rows: Vec<u16>,
}

impl Widget for VerticalRule {
    fn render(self, area: Rect, buf: &mut Buffer) {
        if area.width == 0 {
            return;
        }
        let color = shade(if self.highlighted { DIM } else { NEAR_BLACK });
        for y in area.y..area.y + area.height {
            let ch = if self.junction_rows.contains(&y) {
                '├'
            } else {
                '│'
            };
            if let Some(cell) = buf.cell_mut((area.x, y)) {
                cell.set_char(ch);
                cell.set_fg(color);
            }
        }
    }
}
