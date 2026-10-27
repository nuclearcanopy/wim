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
    /// `(x, glyph)` pairs where some other vertical divider meets this
    /// rule, so the two lines actually join instead of crossing with a
    /// gap: `┴` where one ends from above (e.g. the sidebar/main-
    /// content split), `┬` where one starts below (e.g. a boxed hint
    /// carved out of the bottom bar's own row). This is not optional
    /// polish: two dividers that are structurally meant to meet but are
    /// drawn by two widgets with zero knowledge of each other will
    /// visibly not connect unless something tells them to.
    pub junctions: Vec<(u16, char)>,
}

impl Widget for HorizontalRule {
    fn render(self, area: Rect, buf: &mut Buffer) {
        if area.height == 0 {
            return;
        }
        let color = shade(if self.highlighted { DIM } else { NEAR_BLACK });
        for x in area.x..area.x + area.width {
            let ch = self
                .junctions
                .iter()
                .find(|(jx, _)| *jx == x)
                .map_or('─', |(_, ch)| *ch);
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
    /// `(y, glyph, color)` triples where something on the other side
    /// (the search box's open right edge, the browse header's own
    /// internal rule, the banner's rule) meets this divider; drawn as
    /// `glyph` there instead of a plain `│`, in the given color rather
    /// than the rule's own, same reasoning as `HorizontalRule`'s
    /// `junctions`: two lines meeting from different widgets don't
    /// connect (or animate in sync) on their own. A static color is
    /// just `shade(level)` with a fixed level; a caller animating its
    /// own rule can pass its current pulse level instead, so the
    /// junction stays in lockstep rather than sitting there as the one
    /// dead pixel on an otherwise alive line.
    pub junction_rows: Vec<(u16, char, ratatui::style::Color)>,
}

impl Widget for VerticalRule {
    fn render(self, area: Rect, buf: &mut Buffer) {
        if area.width == 0 {
            return;
        }
        let default_color = shade(if self.highlighted { DIM } else { NEAR_BLACK });
        for y in area.y..area.y + area.height {
            let junction = self.junction_rows.iter().find(|(jy, _, _)| *jy == y);
            let (ch, color) = match junction {
                Some((_, ch, color)) => (*ch, *color),
                None => ('│', default_color),
            };
            if let Some(cell) = buf.cell_mut((area.x, y)) {
                cell.set_char(ch);
                cell.set_fg(color);
            }
        }
    }
}
