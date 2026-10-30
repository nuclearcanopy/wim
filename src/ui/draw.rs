//! The one place text gets written into a `Buffer`.
//!
//! Every widget used to carry its own `draw_line`/`draw_text`/`put`
//! copy, all of them stepping the cursor one cell per char. That is
//! only correct for ascii: a CJK or emoji title measures two cells but
//! advanced one, so every glyph after it printed a cell to the left of
//! where it belonged and the row's right edge overran. `textfit` was
//! already width-aware; the *drawing* was not, which is the half that
//! actually shows.
//!
//! These advance by `char_cells` and blank the trailing cell of a wide
//! glyph, so what was underneath cannot show through beside it.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Color;

use crate::theme::{shade, NEAR_BLACK};
use crate::ui::textfit::{char_cells, truncate_to_width, width};

/// One cell, bounds-checked by `Buffer` itself.
pub fn put(buf: &mut Buffer, x: u16, y: u16, ch: char, color: Color) {
    if let Some(cell) = buf.cell_mut((x, y)) {
        cell.set_char(ch);
        cell.set_fg(color);
    }
}

/// Writes `text` at `x`, clipped to `budget` cells, and returns the x
/// one past the last cell written. The core of this module; everything
/// else here is a convenience over it.
///
/// `truncate_to_width` drops a double-width char that would straddle
/// the budget rather than half-drawing it, so the result is never
/// wider than asked for.
pub fn at(buf: &mut Buffer, x: u16, y: u16, text: &str, color: Color, budget: usize) -> u16 {
    at_styled(buf, x, y, text, color, None, budget)
}

/// `at` with an optional background, for rows drawn on a plate (see
/// `results_list::fill_row_bg`): the text has to carry the background
/// too or the glyph cells punch holes in it.
pub fn at_styled(
    buf: &mut Buffer,
    x: u16,
    y: u16,
    text: &str,
    color: Color,
    bg: Option<Color>,
    budget: usize,
) -> u16 {
    let mut cursor = x;
    for ch in truncate_to_width(text, budget).chars() {
        let cells = char_cells(ch) as u16;
        if let Some(cell) = buf.cell_mut((cursor, y)) {
            cell.set_char(ch);
            cell.set_fg(color);
            if let Some(bg) = bg {
                cell.set_bg(bg);
            }
        }
        // a wide glyph owns the cell after it; leaving whatever was
        // there visible is how half-overwritten text appears.
        for off in 1..cells {
            if let Some(cell) = buf.cell_mut((cursor + off, y)) {
                cell.set_char(' ');
                cell.set_fg(color);
                if let Some(bg) = bg {
                    cell.set_bg(bg);
                }
            }
        }
        cursor += cells.max(1);
    }
    cursor
}

/// A whole row of `area`, starting at its left edge. Guards `y`
/// against both of `area`'s vertical edges: one of the eight copies
/// this replaces was missing the bottom guard and drew outside its
/// own rect.
pub fn line(buf: &mut Buffer, area: Rect, y: u16, text: &str, color: Color) {
    if y < area.y || y >= area.y + area.height {
        return;
    }
    at(buf, area.x, y, text, color, area.width as usize);
}

/// Blanks every cell of `area`: a space, and the style reset. An
/// overlay that owns a rect owns all of it; none of the full-screen
/// menus cleared theirs, so a sidebar's results, a track list and a
/// stale selection plate all showed through the cells the menu
/// happened not to write. Resetting the background matters as much as
/// the char: `results_list::fill_row_bg` leaves a plate behind that a
/// space alone would not cover.
pub fn clear(buf: &mut Buffer, area: Rect) {
    for y in area.y..area.y + area.height {
        for x in area.x..area.x + area.width {
            if let Some(cell) = buf.cell_mut((x, y)) {
                cell.reset();
            }
        }
    }
}

/// `┌─┐`/`└─┘` one cell thick around `area`, interior untouched (clear
/// it first). Callers pass `NEAR_BLACK`: a border at the same
/// brightness as its content reads as noise rather than structure, the
/// same rule `ui::divider` states.
pub fn frame(buf: &mut Buffer, area: Rect, color: Color) {
    if area.width < 2 || area.height < 2 {
        return;
    }
    let right = area.x + area.width - 1;
    let bottom = area.y + area.height - 1;
    put(buf, area.x, area.y, '┌', color);
    put(buf, right, area.y, '┐', color);
    put(buf, area.x, bottom, '└', color);
    put(buf, right, bottom, '┘', color);
    for x in area.x + 1..right {
        put(buf, x, area.y, '─', color);
        put(buf, x, bottom, '─', color);
    }
    for y in area.y + 1..bottom {
        put(buf, area.x, y, '│', color);
        put(buf, right, y, '│', color);
    }
}

/// The content rect inside a `frame`: the border column plus one of
/// horizontal breathing room, matching `subtitle_menu`'s box. Zero
/// width when `area` is too small to hold both, so the writers clip to
/// nothing instead of drawing over the border.
pub fn framed_inner(area: Rect) -> Rect {
    if area.width < 5 || area.height < 3 {
        return Rect::new(area.x, area.y, 0, 0);
    }
    Rect {
        x: area.x + 2,
        y: area.y + 1,
        width: area.width - 4,
        height: area.height - 2,
    }
}

/// What every full-screen takeover does first: own the whole rect,
/// frame it so it reads as a surface rather than text floating over
/// the layout, and hand back the content rect inside the frame.
pub fn overlay(buf: &mut Buffer, area: Rect) -> Rect {
    scrim(buf, area);
    frame(buf, area, shade(NEAR_BLACK));
    framed_inner(area)
}

/// How much brightness survives the scrim. Low enough that an
/// overlay's own text reads cleanly on top, high enough that the shape
/// of what is behind still comes through.
const SCRIM_KEEP: f32 = 0.22;

/// Darkens `area` to one flat block per cell instead of wiping it.
///
/// Each cell's foreground and background collapse into a single level,
/// which is where the softness comes from: a quadrant glyph carries
/// two shades in one cell, so merging them throws the sub-cell detail
/// away on purpose. The result is darkened and painted as a full
/// block, so what sits behind a menu reads as a dim, chunky version of
/// itself rather than a black hole.
///
/// This started as a bug (a menu that forgot to clear its rect) and is
/// deliberate now, because over album art it looks far better than a
/// wipe: the cover is still legibly *there* behind the menu. Every
/// cell is still written, so an overlay owns its whole rect and
/// nothing underneath shows through unscrimmed.
pub fn scrim(buf: &mut Buffer, area: Rect) {
    for y in area.y..area.y + area.height {
        for x in area.x..area.x + area.width {
            if let Some(cell) = buf.cell_mut((x, y)) {
                let behind = (level_of(cell.fg) + level_of(cell.bg)) * 0.5;
                cell.reset();
                // nothing behind this cell, so there is nothing to
                // show through: leave it blank. without this the
                // menu's own padding fills with dim blocks, which is
                // noise rather than frosted glass, and a menu opened
                // over the banner or an empty pane looks broken.
                if behind <= NOTHING_BEHIND {
                    continue;
                }
                cell.set_char(FULL_BLOCK);
                cell.set_fg(shade(behind * SCRIM_KEEP));
            }
        }
    }
}

/// A `theme::shade` level back out of a `Color`. Luma rather than one
/// channel, so a coloured cover scrims by how bright it reads; `Reset`
/// is the terminal's own background, i.e. black as far as this is
/// concerned.
fn level_of(c: Color) -> f32 {
    match c {
        Color::Rgb(r, g, b) => (0.299 * r as f32 + 0.587 * g as f32 + 0.114 * b as f32) / 255.0,
        Color::Reset | Color::Black => 0.0,
        // nothing in this app sets a named colour; treat an unknown one
        // as mid rather than guessing it dark and losing the shape.
        _ => 0.5,
    }
}

/// The scrim's glyph: one solid cell, which is the point (it is what
/// discards the sub-cell detail).
const FULL_BLOCK: char = '\u{2588}';

/// At or below this, a cell is reading as the terminal's own black and
/// there is nothing to scrim.
const NOTHING_BEHIND: f32 = 0.02;

/// Centred by *display* width, so a wide-char title still sits in the
/// middle rather than drifting right by its own overhang.
pub fn centered(buf: &mut Buffer, area: Rect, y: u16, text: &str, color: Color) {
    if y < area.y || y >= area.y + area.height {
        return;
    }
    let fitted = truncate_to_width(text, area.width as usize);
    let pad = (area.width as usize).saturating_sub(width(&fitted)) / 2;
    at(
        buf,
        area.x + pad as u16,
        y,
        &fitted,
        color,
        area.width as usize - pad,
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::theme::NEAR_WHITE;
    use crate::ui::testbuf::{assert_no_bleed_through, buffer, grid, plated_buffer};

    fn row(buf: &Buffer, y: u16, w: u16) -> String {
        (0..w)
            .map(|x| buf.cell((x, y)).unwrap().symbol().to_string())
            .collect()
    }

    #[test]
    fn ascii_advances_one_cell_per_char() {
        let mut buf = buffer(8, 1);
        let end = at(&mut buf, 0, 0, "abc", shade(NEAR_WHITE), 8);
        assert_eq!(end, 3);
        assert_eq!(row(&buf, 0, 8), "abc.....");
    }

    #[test]
    fn a_wide_char_advances_two_cells_and_blanks_its_tail() {
        let mut buf = buffer(8, 1);
        // CJK: two cells each, so "日本" then "x" must land at x=4.
        let end = at(&mut buf, 0, 0, "日本x", shade(NEAR_WHITE), 8);
        assert_eq!(end, 5, "two wide chars plus one narrow");
        let r = row(&buf, 0, 8);
        // the fill char is gone from the tail cells, not left showing
        assert!(r.starts_with("日 本 x"), "got {r:?}");
    }

    #[test]
    fn text_after_a_wide_char_is_not_shifted_left() {
        let mut buf = buffer(10, 1);
        at(&mut buf, 0, 0, "日ab", shade(NEAR_WHITE), 10);
        // with a per-char advance this wrote 'a' at x=1, inside the
        // wide glyph; it belongs at x=2.
        assert_eq!(buf.cell((2, 0)).unwrap().symbol(), "a");
        assert_eq!(buf.cell((3, 0)).unwrap().symbol(), "b");
    }

    #[test]
    fn a_wide_char_straddling_the_budget_is_dropped_not_halved() {
        let mut buf = buffer(8, 1);
        // budget 3: "a" then one wide char would need 3 cells exactly,
        // a second would straddle and is dropped.
        let end = at(&mut buf, 0, 0, "a日日", shade(NEAR_WHITE), 3);
        assert_eq!(end, 3);
        assert_eq!(row(&buf, 0, 8), "a日 .....", "{}", row(&buf, 0, 8));
    }

    #[test]
    fn line_respects_both_vertical_edges_of_its_rect() {
        let mut buf = buffer(6, 4);
        let area = Rect::new(0, 1, 6, 2);
        line(&mut buf, area, 0, "above", shade(NEAR_WHITE));
        line(&mut buf, area, 3, "below", shade(NEAR_WHITE));
        line(&mut buf, area, 1, "in", shade(NEAR_WHITE));
        let g = grid(&buf);
        assert!(!g.contains("above"), "drew above its rect:\n{g}");
        assert!(!g.contains("below"), "drew below its rect:\n{g}");
        assert!(g.contains("in"));
    }

    #[test]
    fn line_clips_at_the_right_edge() {
        let mut buf = buffer(10, 1);
        let area = Rect::new(0, 0, 4, 1);
        line(&mut buf, area, 0, "abcdefgh", shade(NEAR_WHITE));
        assert_eq!(row(&buf, 0, 10), "abcd......");
    }

    #[test]
    fn centered_measures_in_cells_not_chars() {
        let mut buf = buffer(10, 1);
        let area = Rect::new(0, 0, 10, 1);
        // "日本" is 4 cells wide, so it centres at x=3, not x=4.
        centered(&mut buf, area, 0, "日本", shade(NEAR_WHITE));
        assert_eq!(buf.cell((3, 0)).unwrap().symbol(), "日");
    }

    #[test]
    fn clear_blanks_the_char_and_the_background() {
        let mut buf = buffer(6, 3);
        let plated = Rect::new(1, 1, 3, 1);
        for x in 1..4u16 {
            let cell = buf.cell_mut((x, 1)).unwrap();
            cell.set_char('z');
            cell.set_bg(shade(NEAR_WHITE));
        }
        clear(&mut buf, plated);
        for x in 1..4u16 {
            let cell = buf.cell((x, 1)).unwrap();
            assert_eq!(cell.symbol(), " ");
            assert_eq!(cell.bg, Color::Reset, "a stale plate still shows through");
        }
        // outside the cleared rect nothing moved
        assert_eq!(row(&buf, 0, 6), "......");
    }

    #[test]
    fn frame_draws_corners_edges_and_leaves_the_interior_alone() {
        let mut buf = buffer(6, 4);
        frame(&mut buf, Rect::new(0, 0, 6, 4), shade(NEAR_BLACK));
        assert_eq!(
            grid(&buf),
            ["┌────┐", "│....│", "│....│", "└────┘"].join("\n")
        );
    }

    #[test]
    fn frame_and_framed_inner_do_nothing_at_degenerate_sizes() {
        for (w, h) in [(0u16, 0u16), (1, 1), (2, 1), (1, 4), (4, 2)] {
            let mut buf = buffer(w.max(1), h.max(1));
            let area = Rect::new(0, 0, w, h);
            frame(&mut buf, area, shade(NEAR_BLACK));
            let inner = framed_inner(area);
            line(&mut buf, inner, inner.y, "bleed", shade(NEAR_WHITE));
            assert!(!grid(&buf).contains('b'), "{}x{} drew content", w, h);
        }
    }

    #[test]
    fn framed_inner_sits_one_row_and_two_columns_inside() {
        let inner = framed_inner(Rect::new(3, 2, 20, 10));
        assert_eq!(inner, Rect::new(5, 3, 16, 8));
    }

    #[test]
    fn zero_budget_and_zero_height_write_nothing() {
        let mut buf = buffer(4, 1);
        assert_eq!(at(&mut buf, 0, 0, "abc", shade(NEAR_WHITE), 0), 0);
        let empty = Rect::new(0, 0, 4, 0);
        line(&mut buf, empty, 0, "abc", shade(NEAR_WHITE));
        assert_eq!(row(&buf, 0, 4), "....");
    }

    /// The effect, deliberately: a quadrant cell carries two shades,
    /// the scrim merges them into one dim full block. That merge is
    /// what reads as softness, and the block is what reads as chunky.
    #[test]
    fn the_scrim_collapses_a_two_shade_cell_into_one_dim_block() {
        let mut buf = buffer(3, 1);
        if let Some(cell) = buf.cell_mut((1u16, 0u16)) {
            cell.set_char('\u{2580}');
            cell.set_fg(shade(1.0));
            cell.set_bg(shade(0.0));
        }
        scrim(&mut buf, Rect::new(0, 0, 3, 1));

        let cell = buf.cell((1u16, 0u16)).unwrap();
        assert_eq!(cell.symbol(), "\u{2588}", "the sub-cell detail should go");
        let Color::Rgb(v, _, _) = cell.fg else {
            panic!("scrim must stay on the grey ramp: {:?}", cell.fg)
        };
        assert!(v > 0, "something was behind it, so it is not black");
        assert!(
            v < 128,
            "and it must be darker than what it replaced, not brighter: {v}"
        );
    }

    /// The bug the first version had: a menu over the banner or an
    /// empty pane filled its own padding with dim blocks. Nothing
    /// behind a cell means nothing to show through.
    #[test]
    fn the_scrim_leaves_a_cell_with_nothing_behind_it_blank() {
        let mut buf = buffer(3, 1);
        scrim(&mut buf, Rect::new(0, 0, 3, 1));
        for x in 0..3u16 {
            assert_eq!(
                buf.cell((x, 0u16)).unwrap().symbol(),
                " ",
                "an empty cell should stay empty"
            );
        }
    }

    /// An overlay still owns every cell of its rect: the scrim writes
    /// or blanks all of them, so nothing underneath survives
    /// unscrimmed. This is what `assert_no_bleed_through` checks for
    /// the real menus.
    #[test]
    fn an_overlay_still_owns_every_cell_it_covers() {
        let mut buf = plated_buffer(10, 4);
        overlay(&mut buf, Rect::new(0, 0, 10, 4));
        assert_no_bleed_through(&buf, Rect::new(0, 0, 10, 4));
    }
}
