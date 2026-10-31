use ratatui::buffer::Buffer;
use ratatui::layout::Rect;

use crate::ui::textfit::{truncate_with_ellipsis, width as text_width};

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
    let kept = fit_hints(instructions, width.saturating_sub(text_width(title) + 3));
    let header = if kept.is_empty() {
        title.to_string()
    } else {
        format!("{title} │ {kept}")
    };
    let fitted = truncate_with_ellipsis(&header, width);

    // the │ is drawn in rule_color, not text_color; it belongs to the
    // same vertical line as the ┴ below it, so it must match that
    // line's color throughout, not the surrounding title/instructions
    // text. mixing brightness along what's meant to read as one
    // continuous line is exactly what looked wrong both times before.
    // the junction lines up with the cell the │ above it was actually
    // painted on, taken from the drawing loop itself rather than
    // recomputed: a char index would sit a cell left of the │ for every
    // wide glyph in the title (album and artist names reach here), and
    // the two lines that have to meet would not. if truncation ate the
    // separator entirely there is no junction, just a plain rule.
    let mut junction_col = None;
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
            if ch == '│' {
                junction_col = Some((x - area.x) as usize);
            }
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

/// `instructions` fitted to `room` cells, dropping a trailing hint cut
/// so short it names a key and no action: a 28-cell pane headed
/// "Working Class Woman │ j/k: …" spends its last six columns on an
/// ellipsis where the verb should be. A partial action still reads
/// ("enter: play fr…"), so only the verbless fragment goes, back to
/// the last whole hint or to nothing, which gives the title the row.
fn fit_hints(instructions: &str, room: usize) -> String {
    if text_width(instructions) <= room {
        return instructions.to_string();
    }
    let cut = truncate_with_ellipsis(instructions, room);
    // which hint the surviving tail is a piece of, so the test below
    // can tell a key cut before its colon from a hint that never had
    // one.
    let tail = cut.rsplit(", ").next().unwrap_or("");
    let nth = cut.matches(", ").count();
    if names_an_action(tail, instructions.split(", ").nth(nth).unwrap_or("")) {
        return cut;
    }
    match cut.rsplit_once(", ") {
        // shorter than `cut` was by the ", " it drops, so it still fits.
        Some((whole, _)) => format!("{whole}…"),
        None => String::new(),
    }
}

/// Enough of a hint survived to say what the key does: two cells past
/// its `key:`, or two cells of a hint that has no key prefix ("enter
/// to advance"). One cell is not enough; "d d: r…" could be remove,
/// rename or reorder.
///
/// `original` is the hint the fragment came from, and it is needed:
/// the fragment alone cannot tell a prefix-less hint cut short (still
/// informative) from a `key: action` cut *before* its colon, which
/// leaves a bare half-key naming nothing. that second case read as
/// prefix-less and was kept, so a 40-cell header showed "ente…".
fn names_an_action(cut: &str, original: &str) -> bool {
    match (original.contains(": "), cut.split_once(": ")) {
        // the colon survived: judge what follows it.
        (true, Some((_, action))) => text_width(action.trim_end_matches('…')) >= 2,
        // it did not: nothing of the action is left at all.
        (true, None) => false,
        (false, _) => text_width(cut.trim_end_matches('…')) >= 2,
    }
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

    /// The whole point of the ┴: it is the rule meeting the │ above it.
    /// A cjk album title (`browse` and `lyrics_view` both head their
    /// pane with one) put the separator past where a char index said,
    /// so the stem landed under plain `─` with a visible gap.
    #[test]
    fn the_junction_sits_under_the_separator_even_with_a_wide_title() {
        for title in ["a title", "日本語", "日本語 · アーティスト", "ab日cd"] {
            let area = Rect::new(2, 1, 50, 6);
            let mut buf = buffer(area.x + area.width, area.y + area.height);
            draw_header(
                &mut buf,
                area,
                area.y,
                title,
                "some keys",
                shade(DIM),
                shade(NEAR_BLACK),
            );
            let col_of = |y: u16, want: &str| {
                (area.x..area.x + area.width)
                    .find(|x| buf.cell((*x, y)).unwrap().symbol() == want)
                    .unwrap_or_else(|| panic!("no {want:?} on row {y} for {title:?}"))
            };
            assert_eq!(
                col_of(area.y + 1, "┴"),
                col_of(area.y, "│"),
                "{title:?}: the rule's stem has to meet the separator"
            );
        }
    }

    /// A joined-then-truncated header read "j/k: …" on a narrow pane:
    /// an ellipsis where the verb should be. Whole hints only, and the
    /// title keeps its room ahead of them.
    #[test]
    fn a_narrow_header_drops_whole_hints_instead_of_cutting_one() {
        for w in 4..=60u16 {
            let area = Rect::new(0, 0, w, 3);
            let mut buf = buffer(w, 3);
            draw_header(
                &mut buf,
                area,
                area.y,
                "Working Class Woman",
                "j/k: scroll, enter: play",
                shade(DIM),
                shade(NEAR_BLACK),
            );
            let row: String = (0..w).map(|x| buf.cell((x, 0)).unwrap().symbol()).collect();
            // every hint on the row is whole: a ':' is only ever
            // followed by its own action, never by the ellipsis.
            for hint in row.split(", ") {
                assert!(
                    !hint.ends_with(": …") && !hint.ends_with(':'),
                    "{w} cols named a key with no action: {row:?}"
                );
            }
        }
    }

    #[test]
    fn a_verbless_fragment_goes_but_a_partial_action_stays() {
        let hints = "j/k: scroll, enter: play, esc: close";
        assert_eq!(fit_hints(hints, 40), hints, "everything fits");
        // "esc: cl…" still points at close.
        assert_eq!(fit_hints(hints, 34), "j/k: scroll, enter: play, esc: cl…");
        // "esc: …" does not, so it drops back to the whole hint before.
        assert_eq!(fit_hints(hints, 32), "j/k: scroll, enter: play…");
        // and with no whole hint left the title takes the row.
        assert_eq!(fit_hints(hints, 6), "");
        // a hint with no `key:` prefix is all action; nothing to drop.
        assert_eq!(
            fit_hints("enter to advance, esc to skip (local files)", 36),
            "enter to advance, esc to skip (loca…"
        );
    }

    /// The cut that lands *inside* a key, before its colon.
    ///
    /// Found by printing the header at 40 cells and reading it: it
    /// said "j/k: scroll, ente…", a half-key naming nothing. the
    /// fragment has no `": "`, so the prefix-less branch claimed it as
    /// all-action and kept it. no assertion here would have caught
    /// that, because every case anyone thought to write cut *after* a
    /// colon.
    #[test]
    fn a_key_cut_before_its_colon_names_nothing_and_goes() {
        let hints = "j/k: scroll, enter: play";
        assert_eq!(fit_hints(hints, 18), "j/k: scroll…");
        // every width from "the whole thing" down to "nothing fits"
        // either keeps a colon with two cells after it or has dropped
        // the fragment entirely.
        for room in 1..=text_width(hints) {
            let out = fit_hints(hints, room);
            let tail = out.rsplit(", ").next().unwrap_or("");
            if !tail.ends_with('…') || tail.is_empty() {
                continue;
            }
            let (_, action) = tail
                .split_once(": ")
                .unwrap_or_else(|| panic!("room {room}: {out:?} ends in a bare half-key"));
            assert!(
                text_width(action.trim_end_matches('…')) >= 2,
                "room {room}: {out:?} names no action"
            );
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

#[cfg(test)]
mod eyeball {
    use super::*;
    use crate::theme::{shade, DIM, NEAR_BLACK};
    use crate::ui::testbuf::{buffer, grid};

    /// Prints the header at the widths a pane actually gets, including
    /// the ones that eat the separator:
    /// `cargo test --lib ui::header::eyeball -- --nocapture`.
    /// Asserts the floor too, so it is a test and not just a printer.
    #[test]
    fn print_the_header() {
        for (w, h, title, instructions) in [
            (
                60u16,
                4u16,
                "Working Class Woman",
                "j/k: scroll, enter: play",
            ),
            (40, 4, "Working Class Woman", "j/k: scroll, enter: play"),
            (28, 4, "Working Class Woman", "j/k: scroll, enter: play"),
            (20, 4, "Working Class Woman", "j/k: scroll, enter: play"),
            (12, 4, "Working Class Woman", "j/k: scroll, enter: play"),
            (4, 4, "Working Class Woman", "j/k: scroll, enter: play"),
            (60, 2, "Working Class Woman", "j/k: scroll, enter: play"),
            (60, 1, "Working Class Woman", "j/k: scroll, enter: play"),
        ] {
            let area = Rect::new(0, 0, w, h);
            let mut buf = buffer(w, h);
            draw_header(
                &mut buf,
                area,
                area.y,
                title,
                instructions,
                shade(DIM),
                shade(NEAR_BLACK),
            );
            let text = grid(&buf);
            println!("--- {w}x{h}\n{text}");
            assert!(
                text.chars().any(|c| c != '.' && c != '\n'),
                "{w}x{h} drew nothing at all"
            );
        }
    }
}
