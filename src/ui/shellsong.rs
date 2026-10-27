// not drawn by the main loop yet; the host renders this once it owns an
// open console (see shellsong.rs's own note).
#![allow(dead_code)]

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Color;
use ratatui::widgets::Widget;

use crate::shellsong::{Console, LineKind};
use crate::theme::{lerp, shade, BRIGHT, DIM, MID, NEAR_WHITE, WHITE};
use crate::ui::textfit::{tail_fit, truncate_to_width, truncate_with_ellipsis, width};

/// The shellsong console: a centred box over everything else, same
/// hand-drawn border and `┤ label ├` as `ui::searchbox` so it reads as
/// part of the same furniture. Strictly a render of whatever `Console`
/// is given; all editing and parsing lives in `shellsong`.
pub struct ShellsongConsole<'a> {
    pub console: &'a Console,
    /// Blink phase for the block cursor, toggled by the caller on a
    /// timer so this widget stays pure.
    pub cursor_visible: bool,
}

/// How much scrollback the box shows when there is room; enough to see
/// a command and what it reported without covering the whole screen.
const SCROLLBACK_ROWS: usize = 8;

/// Narrow enough that a split pane still gets a usable console; past
/// this there is nowhere to put the input line and the box is skipped.
const MIN_WIDTH: u16 = 14;
const MAX_WIDTH: u16 = 76;

impl Widget for ShellsongConsole<'_> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        // borders (2) + at least the input row; anything smaller has
        // nowhere to draw and is skipped rather than half-drawn.
        if area.width < MIN_WIDTH || area.height < 3 {
            return;
        }
        let w = area
            .width
            .saturating_sub(8)
            .clamp(MIN_WIDTH, MAX_WIDTH)
            .min(area.width);
        let inner = w.saturating_sub(4) as usize;

        let error_rows = u16::from(self.console.error.is_some());
        let room = area.height.saturating_sub(3 + error_rows) as usize;
        let rows = room.min(SCROLLBACK_ROWS).min(self.console.lines.len());
        let h = 3 + error_rows + rows as u16;

        let x = area.x + (area.width - w) / 2;
        let y = area.y + (area.height - h) / 2;
        let border = shade(MID);

        frame(buf, x, y, w, h, border);

        let mut row = y + 1;
        for line in self.console.lines.iter().rev().take(rows).rev() {
            let color = shade(match line.kind {
                LineKind::Command => BRIGHT,
                LineKind::Note => MID,
                LineKind::Error => WHITE,
            });
            text(
                buf,
                x + 2,
                row,
                &truncate_with_ellipsis(&line.text, inner),
                color,
            );
            row += 1;
        }

        // the prompt is the one fixed glyph, so an empty console still
        // shows where typing lands.
        put(buf, x + 2, row, ':', shade(DIM));
        let (shown, cursor_col) = split_at_cursor(
            &self.console.input,
            self.console.cursor,
            inner.saturating_sub(2),
        );
        text(buf, x + 4, row, &shown, shade(NEAR_WHITE));
        // never fully invisible: it dims on the off phase rather than
        // disappearing, same rule as the search box's cursor.
        let level = if self.cursor_visible { 1.0 } else { 0.45 };
        put(
            buf,
            x + 4 + cursor_col as u16,
            row,
            '█',
            shade(lerp(DIM, NEAR_WHITE, level)),
        );
        row += 1;

        if let Some(error) = &self.console.error {
            text(
                buf,
                x + 2,
                row,
                &truncate_with_ellipsis(error, inner),
                shade(WHITE),
            );
        }
    }
}

/// The window of `input` to draw plus the cursor's column in it. The
/// cursor is always inside the window (the end of the text before it is
/// kept, the way any real text input scrolls), and widths are cells not
/// chars so a cjk title cannot push the border out of alignment.
fn split_at_cursor(input: &str, cursor: usize, budget: usize) -> (String, usize) {
    let cursor = cursor.min(input.len());
    let left = tail_fit(&input[..cursor], budget);
    let used = width(&left);
    let right = truncate_to_width(&input[cursor..], budget.saturating_sub(used));
    (format!("{left}{right}"), used)
}

/// Hand-drawn border with the label set into the top edge, and the
/// interior blanked so the box actually occludes what it sits over.
fn frame(buf: &mut Buffer, x: u16, y: u16, w: u16, h: u16, color: Color) {
    for row in 1..h.saturating_sub(1) {
        for col in 1..w.saturating_sub(1) {
            put(buf, x + col, y + row, ' ', color);
        }
    }
    put(buf, x, y, '┌', color);
    put(buf, x + w - 1, y, '┐', color);
    put(buf, x, y + h - 1, '└', color);
    put(buf, x + w - 1, y + h - 1, '┘', color);
    for col in 1..w - 1 {
        put(buf, x + col, y, '─', color);
        put(buf, x + col, y + h - 1, '─', color);
    }
    for row in 1..h - 1 {
        put(buf, x, y + row, '│', color);
        put(buf, x + w - 1, y + row, '│', color);
    }
    label(buf, x, y, w, color);
}

const LABEL: &str = "shellsong";

fn label(buf: &mut Buffer, x: u16, y: u16, w: u16, color: Color) {
    // `┤ shellsong ├` plus a cell of border either side; too narrow to
    // hold it all means no label, not a fragment of one.
    let needed = LABEL.len() + 6;
    if needed > w as usize {
        return;
    }
    let mut col = x + 1;
    put(buf, col, y, '┤', color);
    col += 1;
    put(buf, col, y, ' ', color);
    col += 1;
    for ch in LABEL.chars() {
        put(buf, col, y, ch, shade(NEAR_WHITE));
        col += 1;
    }
    put(buf, col, y, ' ', color);
    col += 1;
    put(buf, col, y, '├', color);
}

fn text(buf: &mut Buffer, x: u16, y: u16, s: &str, color: Color) {
    let mut col = 0u16;
    for ch in s.chars() {
        put(buf, x + col, y, ch, color);
        col += width(&ch.to_string()).max(1) as u16;
    }
}

fn put(buf: &mut Buffer, x: u16, y: u16, ch: char, color: Color) {
    if let Some(cell) = buf.cell_mut((x, y)) {
        cell.set_char(ch);
        cell.set_fg(color);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::shellsong::{edit_insert, Console};
    use crate::ui::testbuf::{buffer, grid};

    fn console(input: &str) -> Console {
        let mut c = Console::empty();
        let mut cursor = 0usize;
        let mut text = String::new();
        for ch in input.chars() {
            edit_insert(&mut text, &mut cursor, ch);
        }
        c.input = text;
        c.cursor = cursor;
        c
    }

    fn render(c: &Console, w: u16, h: u16) -> String {
        let mut buf = buffer(w, h);
        ShellsongConsole {
            console: c,
            cursor_visible: true,
        }
        .render(Rect::new(0, 0, w, h), &mut buf);
        grid(&buf)
    }

    #[test]
    fn the_box_is_labelled_in_its_own_top_border() {
        let out = render(&console(""), 60, 20);
        assert!(out.contains("┤ shellsong ├"), "got:\n{out}");
    }

    #[test]
    fn the_prompt_and_the_typed_line_are_both_visible() {
        let out = render(&console("queue([hex])"), 60, 20);
        assert!(out.contains(": queue([hex])"), "got:\n{out}");
    }

    #[test]
    fn the_cursor_block_is_drawn_on_both_blink_phases() {
        for visible in [true, false] {
            let mut buf = buffer(60, 20);
            ShellsongConsole {
                console: &console("pause()"),
                cursor_visible: visible,
            }
            .render(Rect::new(0, 0, 60, 20), &mut buf);
            assert!(
                grid(&buf).contains('█'),
                "no cursor on blink phase {visible}; the box looks unfocused"
            );
        }
    }

    #[test]
    fn the_box_occludes_what_it_sits_over() {
        let out = render(&console(""), 60, 20);
        let row = out
            .lines()
            .find(|r| r.contains('│'))
            .expect("a side border row");
        let cells: Vec<char> = row.chars().collect();
        let first = cells.iter().position(|c| *c == '│').unwrap();
        let last = cells.iter().rposition(|c| *c == '│').unwrap();
        let interior: String = cells[first + 1..last].iter().collect();
        assert!(
            !interior.contains('.'),
            "the interior still shows the background: {row}"
        );
    }

    #[test]
    fn scrollback_shows_the_most_recent_lines_newest_last() {
        let mut c = console("");
        for i in 0..20 {
            c.note(format!("note {i}"));
        }
        let out = render(&c, 60, 20);
        assert!(out.contains("note 19"), "got:\n{out}");
        assert!(!out.contains("note 0 "), "stale lines should scroll off");
    }

    #[test]
    fn an_error_gets_its_own_line_under_the_input() {
        let mut c = console("shufle()");
        c.error = Some("col 1: unknown command 'shufle'".to_string());
        let out = render(&c, 60, 20);
        assert!(out.contains("unknown command"), "got:\n{out}");
    }

    #[test]
    fn a_long_line_is_cut_with_an_ellipsis_rather_than_overflowing() {
        let mut c = console("");
        c.note("x".repeat(200));
        let out = render(&c, 40, 20);
        assert!(out.contains('…'), "got:\n{out}");
        for row in out.lines() {
            assert_eq!(row.chars().count(), 40, "a row overflowed: {row}");
        }
    }

    #[test]
    fn the_cursor_stays_visible_once_the_line_is_wider_than_the_box() {
        let c = console(&format!("queue([{}", "a".repeat(200)));
        let out = render(&c, 40, 10);
        assert!(out.contains('█'), "the cursor scrolled off:\n{out}");
    }

    /// Nothing here may panic on a terminal too small to hold the box;
    /// a zero-height area in particular is a real resize transient.
    #[test]
    fn degenerate_sizes_draw_nothing_instead_of_panicking() {
        for (w, h) in [
            (1u16, 0u16),
            (1, 1),
            (0, 1),
            (3, 3),
            (23, 10),
            (60, 1),
            (60, 2),
        ] {
            let mut buf = buffer(w.max(1), h.max(1));
            ShellsongConsole {
                console: &console("queue([hex])"),
                cursor_visible: true,
            }
            .render(Rect::new(0, 0, w, h), &mut buf);
        }
    }

    #[test]
    fn a_narrow_box_drops_the_label_rather_than_a_fragment() {
        let out = render(&console(""), 20, 10);
        let top = out
            .lines()
            .find(|r| r.contains('┌'))
            .expect("a top border row");
        assert!(!top.contains('┤'), "half a label is worse than none: {top}");
        // but it is back as soon as there is room for the whole thing
        assert!(render(&console(""), 30, 10).contains("┤ shellsong ├"));
    }

    #[test]
    fn wide_characters_do_not_push_the_border_out_of_alignment() {
        let mut c = console("");
        c.note("日本語のタイトルがとても長い場合でも枠は崩れない");
        let out = render(&c, 40, 12);
        for row in out.lines() {
            assert!(
                row.chars().count() <= 40,
                "a row got wider than the terminal: {row}"
            );
        }
    }
}
