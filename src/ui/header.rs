use ratatui::buffer::Buffer;
use ratatui::layout::Rect;

use crate::ui::textfit::truncate_with_ellipsis;

/// Draws a "title │ instructions" header line followed by a horizontal
/// rule with a T-junction (┬) directly below the │ separator, so the
/// divider molds around the actual text instead of being a generic
/// flat line unrelated to what's above it. No em dashes, ever; a
/// box-drawing vertical bar is the separator, matching the rest of the
/// monochrome/line-drawing aesthetic. Returns the y of the next free
/// row after the rule.
pub fn draw_header(
    buf: &mut Buffer,
    area: Rect,
    y: u16,
    title: &str,
    instructions: &str,
    text_color: ratatui::style::Color,
    rule_color: ratatui::style::Color,
) -> u16 {
    let width = area.width as usize;
    let header = format!("{title} │ {instructions}");
    let fitted = truncate_with_ellipsis(&header, width);

    draw_text(buf, area, y, &fitted, text_color);

    // the junction lines up with wherever │ actually landed in the
    // (possibly truncated) fitted text, not a recomputed offset; if
    // truncation ate the separator entirely, there's no junction, just
    // a plain rule.
    let junction_col = fitted.chars().position(|c| c == '│');

    let rule_y = y + 1;
    if rule_y < area.y + area.height {
        for col in 0..width {
            let x = area.x + col as u16;
            // ┴ (stem up), not ┬ (stem down): the │ this connects to is
            // in the row *above* the rule, so the joint must point up
            // into it, not down into nothing. the junction cell also
            // takes text_color, matching the │ above it, rather than
            // the rule's dimmer color — it's visually a continuation of
            // that vertical bar, not of the horizontal rule.
            // same color as the rest of the rule — the ┴ shape alone
            // conveys the connection; a brightness jump just for one
            // character reads as a stray artifact, not a joint.
            let ch = if junction_col == Some(col) { '┴' } else { '─' };
            if let Some(cell) = buf.cell_mut((x, rule_y)) {
                cell.set_char(ch);
                cell.set_fg(rule_color);
            }
        }
    }

    rule_y + 1
}

fn draw_text(buf: &mut Buffer, area: Rect, y: u16, text: &str, color: ratatui::style::Color) {
    if y >= area.y + area.height {
        return;
    }
    for (col, ch) in text.chars().enumerate() {
        let x = area.x + col as u16;
        if x >= area.x + area.width {
            break;
        }
        if let Some(cell) = buf.cell_mut((x, y)) {
            cell.set_char(ch);
            cell.set_fg(color);
        }
    }
}
