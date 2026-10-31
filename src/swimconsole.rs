//! shellswim's console: the same `shellsong::Console` state the
//! overlay in `ui::shellsong` draws, laid out as a full screen
//! instead of a centred box. Here rather than under `ui/` only so the
//! ui batch and this branch do not edit one file; the state, the
//! completion and the history are all the language's, untouched.
//!
//! The one real difference is anchoring: an overlay grows downward
//! from a centred origin, a shell grows *upward* from the prompt. The
//! prompt sits on the last row and scrollback fills whatever is left,
//! so a line arriving does not move the thing you are typing into.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Color;
use ratatui::widgets::Widget;

use crate::shellsong::{Console, LineKind};
use crate::theme::{lerp, shade, BRIGHT, DIM, MID, NEAR_BLACK, NEAR_WHITE, WHITE};
use crate::ui::controlsbar;
/// Marks the scrolled-back notice. Ascii, like every other marker
/// here: the alignment of this console depends on single cells.
const SCROLL_MARK: char = '^';

use crate::ui::progress;
use crate::ui::textfit::{char_cells, truncate_to_width, truncate_with_ellipsis, width};

/// How far in the prompt and every scrollback line sit, so there is a
/// gutter to put the `:` and the `›` in.
const GUTTER: u16 = 2;

const INFO_MARK: char = '›';

pub struct SwimConsole<'a> {
    pub console: &'a Console,
    /// Blink phase for the block cursor, toggled by the caller on a
    /// timer so this widget stays pure.
    pub cursor_visible: bool,
}

impl Widget for SwimConsole<'_> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        if area.width <= GUTTER * 2 || area.height == 0 {
            return;
        }
        // the console owns the whole screen, so it clears the whole
        // screen: an unwritten cell shows whatever was under it, and
        // the gutter between `:` and the text is exactly such a cell.
        for y in area.top()..area.bottom() {
            for x in area.left()..area.right() {
                if let Some(cell) = buf.cell_mut((x, y)) {
                    cell.reset();
                }
            }
        }
        let inner = usize::from(area.width - GUTTER * 2);

        // claimed from the bottom up: prompt, then the rows that
        // qualify what is on it, then scrollback with the remainder.
        let mut rows_from_bottom: Vec<(String, Color)> = Vec::new();
        if let Some(info) = self.info_line() {
            rows_from_bottom.push((format!("{INFO_MARK} {info}"), shade(DIM)));
        }
        // a scrolled-back console that said nothing would read as a
        // frozen one: output arrives, the screen does not move, and
        // nothing explains why. this row is the explanation, and it is
        // only there while it is true.
        if self.console.scroll() > 0 {
            rows_from_bottom.push((
                // short on purpose: printed at 46 cells it was
                // ellipsised mid-hint, which is worse than naming one
                // key. pgdn is the one that moves a page.
                format!(
                    "{} {} back · pgdn to catch up",
                    SCROLL_MARK,
                    self.console.scroll()
                ),
                shade(MID),
            ));
        }
        if let Some(error) = &self.console.error {
            rows_from_bottom.push((error.clone(), shade(WHITE)));
        }
        if let Some(cycle) = self.console.cycle() {
            rows_from_bottom.push((cycle_line(cycle.items.as_slice(), cycle.at), shade(BRIGHT)));
        }

        // the prompt is the last row, always: a shell's input does not
        // move when output arrives above it.
        let prompt_y = area.y + area.height - 1;
        let claimed = u16::try_from(rows_from_bottom.len()).unwrap_or(0);
        let mut y = prompt_y;
        self.draw_input(buf, area.x, y, inner);
        for (text, color) in &rows_from_bottom {
            if y == area.y {
                break;
            }
            y -= 1;
            text_at(buf, area.x + GUTTER, y, text, inner, *color);
        }

        // whatever is left above goes to scrollback, newest nearest
        // the prompt.
        let spare = area.height.saturating_sub(1 + claimed);
        if spare == 0 {
            return;
        }
        // the view is the only thing that knows this height, and the
        // scroll offset is clamped against it; see `Console::viewport`.
        self.console.saw_viewport(usize::from(spare));

        let total = self.console.lines.len();
        let shown = total.min(usize::from(spare));
        // `scroll` is lines back from the live tail, already clamped,
        // but clamped against *last* frame's height: a terminal that
        // just got taller would otherwise index past the start.
        let back = self.console.scroll().min(total - shown);
        let end = total - back;
        let top = area.y + (area.height - 1 - claimed) - u16::try_from(shown).unwrap_or(0);
        for (i, line) in self.console.lines[end - shown..end].iter().enumerate() {
            let level = match line.kind {
                LineKind::Command => BRIGHT,
                LineKind::Note => MID,
                LineKind::Error => WHITE,
            };
            let y = top + u16::try_from(i).unwrap_or(0);
            // a command echoes with the prompt that produced it, so
            // scrollback reads as a transcript rather than a log.
            let text = if line.kind == LineKind::Command {
                format!(": {}", line.text)
            } else {
                line.text.clone()
            };
            text_at(buf, area.x + GUTTER, y, &text, inner, shade(level));
        }
    }
}

impl SwimConsole<'_> {
    /// The prompt row: `:` in the gutter, the typed line, the block
    /// cursor, then the ghost completion after it.
    fn draw_input(&self, buf: &mut Buffer, x: u16, row: u16, inner: usize) {
        put(buf, x, row, ':', shade(DIM));
        let text_x = x + GUTTER;
        let (window, cursor_col) = split_at_cursor(&self.console.input, self.console.cursor, inner);
        let used = draw_str(buf, text_x, row, &window, shade(NEAR_WHITE));

        // never fully invisible, so the caret is findable mid-blink.
        let level = if self.cursor_visible { 1.0 } else { 0.45 };
        put(
            buf,
            text_x + u16::try_from(cursor_col).unwrap_or(0),
            row,
            '\u{2588}',
            shade(lerp(DIM, NEAR_WHITE, level)),
        );

        let ghost = &self.console.suggestion.ghost;
        if ghost.is_empty() || cursor_col != width(&window) {
            return;
        }
        let budget = inner.saturating_sub(used + 1);
        if budget == 0 {
            return;
        }
        draw_str(
            buf,
            text_x + u16::try_from(used).unwrap_or(0) + 1,
            row,
            &truncate_to_width(ghost, budget),
            shade(lerp(NEAR_BLACK, DIM, 0.75)),
        );
    }

    /// The signature of the call being typed, and the hint for a value
    /// that cannot be guessed. Both say what is expected; neither is
    /// insertable, which is why they live off the input row.
    fn info_line(&self) -> Option<String> {
        let s = &self.console.suggestion;
        match (&s.signature, s.hint) {
            (Some((sig, at)), Some(hint)) => Some(format!("{}   {hint}", mark_param(sig, *at))),
            (Some((sig, at)), None) => Some(mark_param(sig, *at)),
            (None, Some(hint)) => Some(hint.to_string()),
            (None, None) => None,
        }
    }
}

/// Wraps the active parameter in angle brackets, keeping a trailing
/// `?` outside it: `volume(<level>)`, `star(<terms>?)`.
fn mark_param(signature: &str, at: usize) -> String {
    let Some(open) = signature.find('(') else {
        return signature.to_string();
    };
    let (name, rest) = signature.split_at(open);
    let inside = rest.trim_start_matches('(').trim_end_matches(')');
    let marked: Vec<String> = inside
        .split(", ")
        .enumerate()
        .map(|(i, p)| {
            if i != at {
                return p.to_string();
            }
            match p.strip_suffix('?') {
                Some(bare) => format!("<{bare}>?"),
                None => format!("<{p}>"),
            }
        })
        .collect();
    format!("{name}({})", marked.join(", "))
}

/// The tab menu, with the current item bracketed and a position
/// counter so cycling tells you how far round you are.
fn cycle_line(items: &[String], at: usize) -> String {
    let shown: Vec<String> = items
        .iter()
        .enumerate()
        .map(|(i, it)| {
            if i == at {
                format!("[{it}]")
            } else {
                it.clone()
            }
        })
        .collect();
    format!("{}  {}/{}", shown.join("  "), at + 1, items.len())
}

/// The window of `input` to draw and the cursor's column within it.
/// Cell-width aware: a wide glyph is two columns, and a cursor past
/// the right edge scrolls the window rather than drawing off it.
fn split_at_cursor(input: &str, cursor: usize, budget: usize) -> (String, usize) {
    let before = &input[..cursor.min(input.len())];
    if width(input) <= budget {
        return (input.to_string(), width(before));
    }
    // keep the cursor one cell in from the right edge.
    let keep = budget.saturating_sub(1);
    let mut start = 0;
    let mut w = 0;
    for (i, ch) in before.char_indices().rev() {
        w += char_cells(ch);
        if w > keep {
            break;
        }
        start = i;
    }
    let window = truncate_to_width(&input[start..], budget);
    (
        window.clone(),
        width(&input[start..cursor.min(input.len())]).min(budget),
    )
}

fn text_at(buf: &mut Buffer, x: u16, y: u16, s: &str, budget: usize, color: Color) {
    draw_str(buf, x, y, &truncate_with_ellipsis(s, budget), color);
}

/// Writes `s` one cell at a time and returns the cells used, so a
/// caller can place the next thing after it; wide glyphs advance by
/// two the way the terminal will draw them.
fn draw_str(buf: &mut Buffer, x: u16, y: u16, s: &str, color: Color) -> usize {
    let mut used = 0usize;
    for ch in s.chars() {
        put(buf, x + u16::try_from(used).unwrap_or(0), y, ch, color);
        used += char_cells(ch);
    }
    used
}

fn put(buf: &mut Buffer, x: u16, y: u16, ch: char, color: Color) {
    if let Some(cell) = buf.cell_mut((x, y)) {
        cell.set_char(ch);
        cell.set_fg(color);
    }
}

/// The one row of chrome above the transport strip: what is playing,
/// and whether anything is behind it. The bar itself deliberately has
/// no words on it (see `ui::controlsbar`), so the name lives here.
/// shellswim's whole bottom bar: the playing title on the left half,
/// its scrubber on the right.
///
/// **The maintainer's spec, verbatim in shape:**
/// `BLADEE - FLATLINE  --:--  (*)-----------  --:--`. No transport
/// glyphs, no queue count, no eq or loop indicator -- those came from
/// reusing wim's `ControlsBar` here, which was never asked for. Only
/// the title and the bar.
///
/// The two halves are **exactly half the width each**, so the
/// scrubber never moves as the title changes length; a title that
/// would reach the bar is ellipsised instead of pushing it.
pub struct SwimBar<'a> {
    pub title: Option<&'a str>,
    pub elapsed_secs: Option<f64>,
    pub duration_secs: Option<f64>,
    /// Seconds left of a between-track gap. It takes the title's half
    /// while it runs, which is not a liberty: the track whose name
    /// would be there has *ended*, and a full scrubber on a paused
    /// player with no explanation is the "looks broken" state this
    /// replaces.
    pub gap_remaining: Option<f64>,
}

/// Cells kept between the two halves, so a full-width title and the
/// scrubber's first readout cannot sit against each other.
const SPLIT_GAP: u16 = 2;

impl Widget for SwimBar<'_> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        if area.width <= GUTTER * 2 || area.height == 0 {
            return;
        }
        for x in area.left()..area.right() {
            if let Some(cell) = buf.cell_mut((x, area.y)) {
                cell.reset();
            }
        }

        // precisely 50/50, measured off the full row rather than what
        // is left after the gutter, so the split sits where the eye
        // expects it to.
        let half = area.width / 2;

        let left = match self.gap_remaining {
            Some(left) => format!("gap · {}s", left.max(0.0).ceil() as u64),
            None => self.title.unwrap_or("nothing playing").to_string(),
        };
        let dim = self.title.is_none() && self.gap_remaining.is_none();
        let title_room = half.saturating_sub(GUTTER + SPLIT_GAP);
        draw_str(
            buf,
            area.x + GUTTER,
            area.y,
            &truncate_with_ellipsis(&left, usize::from(title_room)),
            shade(if dim { DIM } else { NEAR_WHITE }),
        );

        // the scrubber owns the right half: `elapsed <track> total`,
        // with the readouts reserved first so the track absorbs the
        // remainder and never shoves them off the end.
        let (elapsed, total) = progress::readouts(self.elapsed_secs, self.duration_secs);
        let reserved = width(&elapsed) + width(&total) + 2;
        let right_room = usize::from(area.width - half - GUTTER);
        if right_room <= reserved {
            return;
        }
        let frac = match (self.elapsed_secs, self.duration_secs) {
            (Some(e), Some(d)) if d > 0.0 => e / d,
            _ => 0.0,
        };
        let line = format!(
            "{elapsed} {} {total}",
            controlsbar::track(frac, right_room - reserved, false)
        );
        draw_str(buf, area.x + half, area.y, &line, shade(DIM));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::shellsong::Console;
    use crate::ui::testbuf::{buffer, grid};

    fn draw(console: &Console, w: u16, h: u16) -> Buffer {
        let mut buf = buffer(w, h);
        SwimConsole {
            console,
            cursor_visible: true,
        }
        .render(Rect::new(0, 0, w, h), &mut buf);
        buf
    }

    fn row(buf: &Buffer, y: u16) -> String {
        grid(buf).lines().nth(y as usize).unwrap().to_string()
    }

    #[test]
    fn the_prompt_is_the_last_row_whatever_the_height() {
        let mut console = Console::empty();
        console.input = "play".to_string();
        console.cursor = 4;
        for h in [1u16, 3, 24, 60] {
            let buf = draw(&console, 40, h);
            assert!(
                row(&buf, h - 1).starts_with(": play"),
                "height {h} put the prompt somewhere else: {:?}",
                row(&buf, h - 1)
            );
        }
    }

    #[test]
    fn scrollback_is_newest_nearest_the_prompt() {
        let mut console = Console::empty();
        for i in 0..3 {
            console.note(format!("line {i}"));
        }
        let buf = draw(&console, 40, 8);
        // prompt on row 7, newest note immediately above it.
        assert!(row(&buf, 6).contains("line 2"), "{}", grid(&buf));
        assert!(row(&buf, 5).contains("line 1"), "{}", grid(&buf));
        assert!(row(&buf, 4).contains("line 0"), "{}", grid(&buf));
    }

    #[test]
    fn scrollback_longer_than_the_screen_keeps_the_newest() {
        let mut console = Console::empty();
        for i in 0..50 {
            console.note(format!("line {i}"));
        }
        let buf = draw(&console, 40, 5);
        let text = grid(&buf);
        assert!(text.contains("line 49"), "{text}");
        assert!(!text.contains("line 10"), "oldest must scroll off: {text}");
    }

    #[test]
    fn a_command_echoes_with_its_prompt() {
        let mut console = Console::empty();
        console.push(crate::shellsong::LineKind::Command, "play(crest)");
        let buf = draw(&console, 40, 4);
        assert!(
            row(&buf, 2).contains(": play(crest)"),
            "a transcript shows the colon it was typed after: {}",
            grid(&buf)
        );
    }

    #[test]
    fn ghost_text_follows_the_cursor_and_never_precedes_it() {
        let mut console = Console::empty();
        console.input = "vol".to_string();
        console.cursor = 3;
        console.suggestion.ghost = "ume".to_string();
        let buf = draw(&console, 40, 3);
        let line = row(&buf, 2);
        let cursor = line.find('\u{2588}').expect("a cursor");
        let ghost = line.find("ume").expect("the ghost");
        assert!(
            ghost > cursor,
            "the ghost is what comes next, so it draws after the caret: {line:?}"
        );
    }

    #[test]
    fn a_value_hint_is_never_drawn_on_the_input_row() {
        // the whole point of the ghost/hint split: `volume(5` must not
        // suggest `50%` as if it were insertable.
        let mut console = Console::empty();
        console.input = "volume(5".to_string();
        console.cursor = 8;
        console.suggestion.hint = Some("0-100, or +N% / -N%");
        let buf = draw(&console, 44, 4);
        assert!(
            !row(&buf, 3).contains("0-100"),
            "the input row must stay what was typed: {:?}",
            row(&buf, 3)
        );
        assert!(
            row(&buf, 2).contains("0-100"),
            "the hint belongs on its own row: {}",
            grid(&buf)
        );
    }

    #[test]
    fn nothing_is_drawn_into_a_degenerate_rect() {
        let mut console = Console::empty();
        console.note("something");
        for (w, h) in [(0u16, 0u16), (4, 1), (40, 0)] {
            let mut buf = buffer(40, 4);
            SwimConsole {
                console: &console,
                cursor_visible: true,
            }
            .render(Rect::new(0, 0, w, h), &mut buf);
            assert!(
                !grid(&buf).contains("something"),
                "{w}x{h} must draw nothing at all"
            );
        }
    }

    #[test]
    fn a_line_wider_than_the_screen_is_cut_not_wrapped() {
        let mut console = Console::empty();
        console.note("x".repeat(200));
        let buf = draw(&console, 20, 4);
        for y in 0..4 {
            assert_eq!(
                row(&buf, y).chars().count(),
                20,
                "row {y} is not exactly the rect wide: {:?}",
                row(&buf, y)
            );
        }
    }

    #[test]
    fn the_console_occludes_everything_under_it() {
        // it is the whole screen, so nothing it was painted over may
        // show through, background included.
        let mut console = Console::empty();
        console.input = "play".to_string();
        console.cursor = 4;
        console.note("a note");
        let mut buf = crate::ui::testbuf::plated_buffer(30, 6);
        let area = Rect::new(0, 0, 30, 6);
        SwimConsole {
            console: &console,
            cursor_visible: true,
        }
        .render(area, &mut buf);
        crate::ui::testbuf::assert_no_bleed_through(&buf, area);
    }

    #[test]
    fn it_stays_inside_the_rect_it_was_given() {
        let mut console = Console::empty();
        console.input = "play".to_string();
        console.cursor = 4;
        console.note("a note");
        let mut buf = buffer(30, 8);
        let area = Rect::new(4, 2, 20, 4);
        SwimConsole {
            console: &console,
            cursor_visible: true,
        }
        .render(area, &mut buf);
        crate::ui::testbuf::assert_outside_untouched(&buf, area);
    }

    fn bar<'a>(w: u16, f: impl FnOnce(&mut SwimBar<'a>)) -> Buffer {
        let mut v = SwimBar {
            title: Some("BLADEE - FLATLINE"),
            elapsed_secs: Some(44.0),
            duration_secs: Some(214.0),
            gap_remaining: None,
        };
        f(&mut v);
        let mut buf = buffer(w, 1);
        v.render(Rect::new(0, 0, w, 1), &mut buf);
        buf
    }

    #[test]
    fn the_bar_is_the_title_then_its_scrubber() {
        let line = row(&bar(60, |_| {}), 0);
        assert!(line.contains("BLADEE - FLATLINE"), "{line:?}");
        assert!(line.contains("0:44"), "elapsed: {line:?}");
        assert!(line.contains("3:34"), "total: {line:?}");
    }

    /// The halves are exactly half each, so the scrubber does not
    /// shift as titles change length.
    #[test]
    fn the_scrubber_starts_at_the_halfway_column_whatever_the_title() {
        let long = "n".repeat(200);
        for title in ["x", long.as_str(), "BLADEE - FLATLINE"] {
            let buf = bar(60, |v| v.title = Some(title));
            let line = row(&buf, 0);
            // columns, not bytes: an ellipsised title carries `…`,
            // three bytes to one cell, so `find` would answer the
            // wrong question. the same trap the old queue-count test
            // called out.
            let cells: Vec<char> = line.chars().collect();
            let at = cells
                .windows(4)
                .position(|w| w.iter().collect::<String>() == "0:44")
                .unwrap_or_else(|| panic!("{line:?}"));
            assert_eq!(at, 30, "half of 60, for {:?}", &title[..title.len().min(8)]);
        }
    }

    /// A title that would reach the scrubber is cut with an ellipsis
    /// rather than pushing it or overwriting it.
    #[test]
    fn a_long_title_is_ellipsised_and_never_touches_the_bar() {
        let long = "n".repeat(200);
        let buf = bar(60, |v| v.title = Some(long.as_str()));
        let line = row(&buf, 0);
        assert!(line.contains('…'), "{line:?}");
        // two cells of clearance between the halves, so nothing in the
        // title's half is drawn against the readout.
        let half: String = line.chars().take(30).collect();
        assert!(half.ends_with("  "), "no clearance: {half:?}");
    }

    #[test]
    fn the_title_is_left_aligned() {
        let line = row(&bar(60, |_| {}), 0);
        assert!(
            line.starts_with("  BLADEE"),
            "one gutter, then the title: {line:?}"
        );
    }

    #[test]
    fn nothing_playing_says_so() {
        let buf = bar(60, |v| {
            v.title = None;
            v.elapsed_secs = None;
            v.duration_secs = None;
        });
        let line = row(&buf, 0);
        assert!(line.contains("nothing playing"), "{line:?}");
        assert!(line.contains("--:--"), "the scrubber still reads: {line:?}");
    }

    /// A gap takes the title's half: the track whose name would be
    /// there has ended, and a full scrubber on a paused player with no
    /// explanation is the state this avoids.
    #[test]
    fn a_running_gap_takes_the_title_half() {
        let buf = bar(60, |v| v.gap_remaining = Some(6.2));
        let line = row(&buf, 0);
        assert!(line.contains("gap · 7s"), "{line:?}");
        assert!(!line.contains("BLADEE"), "{line:?}");
    }

    /// None of wim's transport glyphs: those were never asked for
    /// here, and reusing `ControlsBar` is how they arrived.
    #[test]
    fn the_bar_carries_no_transport_controls() {
        let line = row(&bar(60, |_| {}), 0);
        for glyph in ['▸', '‖', '↻', '⇄'] {
            assert!(
                !line.contains(glyph),
                "{glyph} is wim's, not here: {line:?}"
            );
        }
    }

    #[test]
    fn a_narrow_bar_draws_nothing_rather_than_rubble() {
        let buf = bar(4, |_| {});
        // `.` is testbuf's untouched-cell fill, so this says the
        // widget declined rather than that it drew blanks.
        assert_eq!(row(&buf, 0), "....");
    }

    /// Prints the console at the tail and scrolled back, so the
    /// window actually moves and the notice is readable in place:
    /// `cargo test --lib print_the_scroll -- --nocapture`.
    #[test]
    fn print_the_scroll() {
        let mut console = Console::default();
        for i in 0..12 {
            console.note(format!("output line {i}"));
        }
        console.saw_viewport(6);
        for (label, back) in [("tail", 0usize), ("4 back", 4), ("6 back", 6)] {
            for _ in 0..back {
                console.scroll_lines(1);
            }
            let mut buf = buffer(46, 9);
            SwimConsole {
                console: &console,
                cursor_visible: true,
            }
            .render(Rect::new(0, 0, 46, 9), &mut buf);
            println!("--- {label}\n{}", grid(&buf));
            console.scroll_to_tail();
        }
    }

    #[test]
    fn print_the_bar() {
        for (w, f) in [(72u16, 0usize), (48, 0), (72, 1)] {
            let buf = bar(w, |v| {
                if f == 1 {
                    v.gap_remaining = Some(6.2);
                }
            });
            println!("--- {w} gap={f}\n[{}]", row(&buf, 0));
        }
    }

    #[test]
    fn the_active_parameter_is_the_one_marked() {
        assert_eq!(mark_param("volume(level)", 0), "volume(<level>)");
        assert_eq!(mark_param("star(terms?)", 0), "star(<terms>?)");
        assert_eq!(
            mark_param("fade(into, secs)", 1),
            "fade(into, <secs>)",
            "the second argument is the one being typed"
        );
    }
}
