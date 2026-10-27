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

    // the │ is drawn in rule_color, not text_color — it belongs to the
    // same vertical line as the ┴ below it, so it must match that
    // line's color throughout, not the surrounding title/instructions
    // text. mixing brightness along what's meant to read as one
    // continuous line is exactly what looked wrong both times before.
    if y < area.y + area.height {
        for (col, ch) in fitted.chars().enumerate() {
            let x = area.x + col as u16;
            if x >= area.x + area.width {
                break;
            }
            let color = if ch == '│' { rule_color } else { text_color };
            if let Some(cell) = buf.cell_mut((x, y)) {
                cell.set_char(ch);
                cell.set_fg(color);
            }
        }
    }

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
            // into it. same rule_color as the rest of the line, same
            // as the │ above — one continuous line, one color, the ┴
            // shape alone marks where it meets the rule.
            let ch = if junction_col == Some(col) { '┴' } else { '─' };
            if let Some(cell) = buf.cell_mut((x, rule_y)) {
                cell.set_char(ch);
                cell.set_fg(rule_color);
            }
        }
    }

    rule_y + 1
}
