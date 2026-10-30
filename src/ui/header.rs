use ratatui::buffer::Buffer;
use ratatui::layout::Rect;

use crate::ui::textfit::truncate_with_ellipsis;

/// Rows `draw_header` occupies: the title line and its rule. The
/// widgets it heads need that number outside their own render too (to
/// name the rect their rows landed in, for the host's hit map), and
/// `draw_header`'s return value is only ever visible inside one.
pub const ROWS: u16 = 2;

/// The rows below a header, which is where a headed widget draws its
/// body. Checked against `draw_header`'s own return value in the tests
/// below, so the constant cannot drift from the drawing.
pub fn body(area: Rect) -> Rect {
    Rect {
        y: area.y + ROWS,
        height: area.height.saturating_sub(ROWS),
        ..area
    }
}

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

    // the │ is drawn in rule_color, not text_color; it belongs to the
    // same vertical line as the ┴ below it, so it must match that
    // line's color throughout, not the surrounding title/instructions
    // text. mixing brightness along what's meant to read as one
    // continuous line is exactly what looked wrong both times before.
    if y < area.y + area.height {
        // written a glyph at a time because the │ takes rule_color
        // while the text around it does not, but the cursor still
        // advances by display width rather than by char.
        let mut x = area.x;
        let right = area.x + area.width;
        for ch in fitted.chars() {
            if x >= right {
                break;
            }
            let color = if ch == '│' { rule_color } else { text_color };
            x = crate::ui::draw::at(
                buf,
                x,
                y,
                &ch.to_string(),
                color,
                right.saturating_sub(x) as usize,
            );
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
            // as the │ above; one continuous line, one color, the ┴
            // shape alone marks where it meets the rule.
            let ch = if junction_col == Some(col) {
                '┴'
            } else {
                '─'
            };
            if let Some(cell) = buf.cell_mut((x, rule_y)) {
                cell.set_char(ch);
                cell.set_fg(rule_color);
            }
        }
    }

    rule_y + 1
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::theme::{shade, DIM, NEAR_BLACK};
    use crate::ui::testbuf::buffer;

    /// `ROWS` and `body` are the constant form of what `draw_header`
    /// returns; a header that grew a row would otherwise leave every
    /// hit-tested list a row out.
    #[test]
    fn the_body_starts_where_draw_header_says_the_next_free_row_is() {
        for area in [
            Rect::new(0, 0, 40, 10),
            Rect::new(3, 7, 40, 10),
            Rect::new(0, 0, 8, 10),
        ] {
            let mut buf = buffer(area.x + area.width, area.y + area.height);
            let next = draw_header(
                &mut buf,
                area,
                area.y,
                "a title",
                "some keys",
                shade(DIM),
                shade(NEAR_BLACK),
            );
            assert_eq!(next, area.y + ROWS, "{area:?}");
            let body = body(area);
            assert_eq!(body.y, next, "{area:?}");
            assert_eq!(body.y + body.height, area.y + area.height, "{area:?}");
        }
    }

    #[test]
    fn a_pane_shorter_than_the_header_has_no_body() {
        for height in 0..=ROWS {
            let body = body(Rect::new(0, 5, 40, height));
            assert_eq!(body.height, 0, "height {height}");
        }
    }
}
