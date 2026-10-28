use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Color;
use ratatui::widgets::Widget;

use crate::shellsong::{Console, LineKind};
use crate::theme::{lerp, shade, BRIGHT, DIM, MID, NEAR_BLACK, NEAR_WHITE, WHITE};
use crate::ui::textfit::{tail_fit, truncate_to_width, truncate_with_ellipsis, width};

/// The shellsong console: a centred box over everything else, same
/// hand-drawn border and `┤ label ├` as `ui::searchbox` so it reads as
/// part of the same furniture. Strictly a render of whatever `Console`
/// is given; all editing, parsing and completion lives in `shellsong`.
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

/// Marks the row carrying a value hint or a signature. Both are
/// *information*, never something a keypress can accept, which is why
/// they live on their own row rather than inline after the cursor: the
/// input row holds exactly what is acceptable and nothing else. The
/// glyph is not typeable in the language, so it cannot be mistaken for
/// text either.
const INFO_MARK: char = '›';

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

        // every row past the two borders and the input line is
        // optional, so they are claimed in order of how much they
        // matter on a short terminal: the cycle being used, then an
        // error to fix, then context, then history. Budgeting rather
        // than subtracting is what keeps `h` inside `area`.
        let info = self.info_line();
        let mut spare = area.height - 3;
        let mut claim = |wanted: bool| {
            let got = wanted && spare > 0;
            if got {
                spare -= 1;
            }
            got
        };
        let show_cycle = claim(self.console.cycle().is_some());
        let show_error = claim(self.console.error.is_some());
        let show_info = claim(info.is_some());
        let rows = (spare as usize)
            .min(SCROLLBACK_ROWS)
            .min(self.console.lines.len());
        let h =
            3 + u16::from(show_cycle) + u16::from(show_error) + u16::from(show_info) + rows as u16;

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

        self.draw_input(buf, x, row, inner);
        row += 1;

        // the cycle menu sits directly under the input, where the
        // candidate being previewed is.
        if let (true, Some(cycle)) = (show_cycle, self.console.cycle()) {
            let marked: Vec<String> = cycle
                .items
                .iter()
                .enumerate()
                .map(|(i, item)| {
                    if i == cycle.at {
                        format!("[{item}]")
                    } else {
                        item.clone()
                    }
                })
                .collect();
            let position = format!("{}/{}", cycle.at + 1, cycle.items.len());
            let budget = inner.saturating_sub(width(&position) + 1);
            text(
                buf,
                x + 2,
                row,
                &truncate_with_ellipsis(&marked.join("  "), budget),
                shade(MID),
            );
            // the indicator is right-aligned so cycling is not blind
            // about how far through the set it has got.
            let at = inner.saturating_sub(width(&position));
            text(buf, x + 2 + at as u16, row, &position, shade(BRIGHT));
            row += 1;
        }

        if let (true, Some(error)) = (show_error, &self.console.error) {
            text(
                buf,
                x + 2,
                row,
                &truncate_with_ellipsis(error, inner),
                shade(WHITE),
            );
            row += 1;
        }

        if let (true, Some(line)) = (show_info, &info) {
            put(buf, x + 2, row, INFO_MARK, shade(DIM));
            text(
                buf,
                x + 4,
                row,
                &truncate_with_ellipsis(line, inner.saturating_sub(2)),
                shade(DIM),
            );
        }
    }
}

impl ShellsongConsole<'_> {
    /// The input row: prompt, the typed line, the ghost of whatever
    /// would be accepted, and the block cursor between them.
    fn draw_input(&self, buf: &mut Buffer, x: u16, row: u16, inner: usize) {
        // the prompt is the one fixed glyph, so an empty console still
        // shows where typing lands.
        put(buf, x + 2, row, ':', shade(DIM));
        let budget = inner.saturating_sub(2);
        let (shown, cursor_col) = split_at_cursor(&self.console.input, self.console.cursor, budget);
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

        // the ghost goes immediately after the cursor, where it would
        // land if accepted, and dim enough that it cannot be mistaken
        // for text that is already there.
        let ghost = &self.console.suggestion.ghost;
        if ghost.is_empty() {
            return;
        }
        let used = cursor_col + 1;
        let ghost = truncate_to_width(ghost, budget.saturating_sub(used));
        text(
            buf,
            x + 4 + used as u16,
            row,
            &ghost,
            shade(lerp(NEAR_BLACK, DIM, 0.75)),
        );
    }

    /// The signature of the call the cursor is inside, with the active
    /// parameter named, plus the value hint when there is one. Both are
    /// read-only context.
    fn info_line(&self) -> Option<String> {
        let suggestion = &self.console.suggestion;
        let signature = suggestion
            .signature
            .as_ref()
            .map(|(sig, at)| mark_param(sig, *at));
        match (signature, suggestion.hint) {
            (Some(sig), Some(hint)) => Some(format!("{sig}   {hint}")),
            (Some(sig), None) => Some(sig),
            (None, Some(hint)) => Some(hint.to_string()),
            (None, None) => None,
        }
    }
}

/// `volume(level)` with the active parameter picked out. Monochrome, so
/// the mark is a glyph rather than a colour: the parameter the cursor
/// is in gets angle brackets.
fn mark_param(signature: &str, at: usize) -> String {
    let Some(open) = signature.find('(') else {
        return signature.to_string();
    };
    let (name, rest) = signature.split_at(open);
    let inside = rest.trim_start_matches('(').trim_end_matches(')');
    if inside.is_empty() {
        return signature.to_string();
    }
    let marked: Vec<String> = inside
        .split(", ")
        .enumerate()
        .map(|(i, param)| {
            if i != at {
                return param.to_string();
            }
            // the optional marker stays outside the brackets, so
            // `<terms>?` reads as "optional" rather than as part of
            // the parameter's name.
            match param.strip_suffix('?') {
                Some(name) => format!("<{name}>?"),
                None => format!("<{param}>"),
            }
        })
        .collect();
    format!("{name}({})", marked.join(", "))
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
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    use crate::shellsong::{Candidates, Console, NoCandidates};
    use crate::ui::testbuf::{buffer, grid};

    /// A fake corpus, so the overlay tests can produce a real ghost and
    /// a real cycle without a library.
    struct Fake(Vec<&'static str>);

    impl Candidates for Fake {
        fn corpus(&self, _token: &str) -> Vec<String> {
            self.0.iter().map(|s| s.to_string()).collect()
        }
    }

    /// Types `input` through the real console, so what is rendered is
    /// whatever the real editor and completer produced.
    fn typed(cand: &dyn Candidates, input: &str) -> Console {
        let mut c = Console::empty();
        for ch in input.chars() {
            c.handle_key(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE), cand);
        }
        c
    }

    fn console(input: &str) -> Console {
        typed(&NoCandidates, input)
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
        let fake = Fake(vec!["flatline", "floor", "flowers"]);
        let mut cycling = typed(&fake, "queue(fl");
        cycling.handle_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE), &fake);
        let mut erroring = console("volume(5");
        erroring.error = Some("col 8: not a number".to_string());

        for state in [&console("queue([hex])"), &cycling, &erroring] {
            for (w, h) in [
                (1u16, 0u16),
                (1, 1),
                (0, 1),
                (3, 3),
                (13, 10),
                (60, 1),
                (60, 2),
                (60, 3),
                (60, 4),
                (14, 5),
            ] {
                let mut buf = buffer(w.max(1), h.max(1));
                ShellsongConsole {
                    console: state,
                    cursor_visible: true,
                }
                .render(Rect::new(0, 0, w, h), &mut buf);
            }
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

    // --------------------------------------------------- completion

    /// The maintainer's example, rendered: typing `queue(fl` shows
    /// `atline` after the cursor without having typed it.
    #[test]
    fn ghost_text_is_drawn_inline_after_the_cursor() {
        let fake = Fake(vec!["flatline"]);
        let c = typed(&fake, "queue(fl");
        let out = render(&c, 60, 20);
        assert!(out.contains(": queue(fl"), "got:\n{out}");
        assert!(out.contains("atline"), "the ghost is missing:\n{out}");
    }

    /// Dimmer than the typed text, so it cannot be read as something
    /// already in the buffer.
    #[test]
    fn the_ghost_is_dimmer_than_what_was_actually_typed() {
        let fake = Fake(vec!["flatline"]);
        let c = typed(&fake, "queue(fl");
        let mut buf = buffer(60, 20);
        ShellsongConsole {
            console: &c,
            cursor_visible: true,
        }
        .render(Rect::new(0, 0, 60, 20), &mut buf);

        let row = (0..20)
            .find(|y| (0..60).any(|x| char_at(&buf, x, *y) == ':'))
            .expect("the input row");
        let typed_cell = (0..60)
            .find(|x| char_at(&buf, *x, row) == 'q')
            .expect("the typed text");
        let ghost_cell = (0..60)
            .find(|x| char_at(&buf, *x, row) == 'a' && *x > typed_cell)
            .expect("the ghost");
        assert!(
            brightness(&buf, ghost_cell, row) < brightness(&buf, typed_cell, row),
            "the ghost reads as typed text"
        );
    }

    fn char_at(buf: &Buffer, x: u16, y: u16) -> char {
        buf.cell((x, y))
            .and_then(|c| c.symbol().chars().next())
            .unwrap_or(' ')
    }

    fn brightness(buf: &Buffer, x: u16, y: u16) -> u8 {
        match buf.cell((x, y)).map(|c| c.fg) {
            Some(Color::Rgb(v, _, _)) => v,
            _ => 0,
        }
    }

    #[test]
    fn an_open_cycle_lists_the_candidates_and_says_where_it_is() {
        let fake = Fake(vec!["flatline", "floor", "flowers"]);
        let mut c = typed(&fake, "queue(fl");
        c.handle_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE), &fake);
        let out = render(&c, 60, 20);
        assert!(out.contains("[flatline]"), "current not marked:\n{out}");
        assert!(out.contains("floor"), "got:\n{out}");
        assert!(out.contains("1/3"), "no position indicator:\n{out}");

        c.handle_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE), &fake);
        let out = render(&c, 60, 20);
        assert!(out.contains("[floor]"), "the mark did not move:\n{out}");
        assert!(out.contains("2/3"), "got:\n{out}");
    }

    /// A hint is not acceptable, so it never shares the input row with
    /// the ghost: it gets its own marked row.
    #[test]
    fn a_value_hint_is_set_apart_from_the_input_row() {
        let c = console("volume(");
        let out = render(&c, 60, 20);
        let rows: Vec<&str> = out.lines().collect();
        let input = rows
            .iter()
            .position(|r| r.contains(": volume("))
            .expect("the input row");
        let hint = rows
            .iter()
            .position(|r| r.contains("0-100"))
            .expect("the hint row");
        assert_ne!(input, hint, "the hint sits on the input row:\n{out}");
        assert!(rows[hint].contains(INFO_MARK), "unmarked hint:\n{out}");
        // nothing acceptable-looking may follow the cursor on the input
        // row: between it and the right border there is only blank.
        let cells: Vec<char> = rows[input].chars().collect();
        let cursor = cells.iter().position(|c| *c == '█').expect("the cursor");
        let edge = cells.iter().rposition(|c| *c == '│').expect("the border");
        assert!(
            cells[cursor + 1..edge].iter().all(|c| *c == ' '),
            "something follows the cursor:\n{out}"
        );
    }

    #[test]
    fn the_signature_names_the_parameter_the_cursor_is_in() {
        let out = render(&console("volume("), 60, 20);
        assert!(out.contains("volume(<level>)"), "got:\n{out}");

        let out = render(&console("add_to_queue(front, "), 60, 20);
        assert!(
            out.contains("add_to_queue(position, <terms>?)"),
            "got:\n{out}"
        );
    }

    #[test]
    fn mark_param_leaves_a_zero_parameter_signature_alone() {
        assert_eq!(mark_param("pause()", 0), "pause()");
        assert_eq!(mark_param("seek(offset)", 0), "seek(<offset>)");
        assert_eq!(
            mark_param("repeat(count, call)", 1),
            "repeat(count, <call>)"
        );
        assert_eq!(mark_param("star(terms?)", 0), "star(<terms>?)");
        // an index past the end marks nothing rather than panicking
        assert_eq!(mark_param("skip()", 3), "skip()");
        assert_eq!(mark_param("no parens", 0), "no parens");
    }

    /// Both extra rows at once, in a box only just tall enough.
    #[test]
    fn a_cycle_a_hint_and_an_error_all_fit_without_overflowing() {
        let fake = Fake(vec!["flatline", "floor"]);
        let mut c = typed(&fake, "queue(fl");
        c.handle_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE), &fake);
        c.error = Some("col 1: something went wrong".to_string());
        c.note("a previous line");
        let out = render(&c, 50, 12);
        for row in out.lines() {
            assert_eq!(row.chars().count(), 50, "a row overflowed: {row}");
        }
        assert!(out.contains("1/2"), "got:\n{out}");
        assert!(out.contains("something went wrong"), "got:\n{out}");
    }

    #[test]
    fn a_very_long_candidate_set_is_cut_but_keeps_the_indicator() {
        let fake = Fake(vec![
            "flatline one",
            "flatline two",
            "flatline three",
            "flatline four",
            "flatline five",
            "flatline six",
            "flatline seven",
            "flatline eight",
        ]);
        let mut c = typed(&fake, "queue(fl");
        c.handle_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE), &fake);
        let out = render(&c, 40, 14);
        assert!(out.contains("1/8"), "the indicator was cut away:\n{out}");
        for row in out.lines() {
            assert_eq!(row.chars().count(), 40, "a row overflowed: {row}");
        }
    }
}
