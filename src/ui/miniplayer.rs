use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::widgets::Widget;

use crate::keymap::{key_label, KeyAction, Keymap};
use crate::theme::{shade, BRIGHT, DIM, NEAR_BLACK, NEAR_WHITE};
use crate::ui::controlsbar::{GLYPH_PAUSE, GLYPH_PLAY};
use crate::ui::textfit::{truncate_with_ellipsis, width as text_width};

/// Thin ipod-style strip above the main bottom bar, spanning only
/// `main_content`'s width (not the sidebar): shows while the full
/// player is minimized, whether that's because an album's track
/// listing is occupying the workspace or the user minimized it by
/// hand. `KeyAction::ToggleMinimize` toggles back to the full player;
/// both keys hinted here are remappable, so they're resolved from
/// `keymap` rather than printed as their defaults.
pub struct MiniPlayerBar<'a> {
    pub title: &'a str,
    pub paused: bool,
    pub keymap: &'a Keymap,
}

/// Fixed height this widget always wants; the caller carves out
/// exactly this many rows above the main bottom bar. Two: its own
/// rule, then the title. It used to be three, the extra row carrying a
/// progress bar plus loop and shuffle, all of which the bottom bar now
/// owns (and its bar is scrubbable, which this one never was). Two
/// progress bars on screen at once read as two players.
pub const HEIGHT: u16 = 2;

/// Cells the title keeps for itself before the key hints are allowed
/// any of the row at all. Below this the hints are dropped whole
/// rather than cut: half a hint ("(space: paus…") answers nothing, and
/// the one thing the strip exists to say is what is playing.
const TITLE_FLOOR: u16 = 18;

impl Widget for MiniPlayerBar<'_> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        if area.height == 0 || area.width == 0 {
            return;
        }

        // squeezed to one row, that row goes to the title: the rule is
        // decoration and the title is the information, and a lone rule
        // said nothing about what was playing.
        let text_y = if area.height == 1 {
            area.y
        } else {
            // a rule of its own, not just relying on the text below it
            // being a different brightness, so the strip reads as its
            // own separated panel rather than text that happens to
            // float above the bottom bar.
            for col in 0..area.width {
                if let Some(cell) = buf.cell_mut((area.x + col, area.y)) {
                    cell.set_char('─');
                    cell.set_fg(shade(NEAR_BLACK));
                }
            }
            area.y + 1
        };

        // the bar's own curated alphabet, not the emoji-presentation
        // pair this used to draw (U+23F8, U+25B6). a terminal with an
        // emoji font paints those in colour, and often double width,
        // neither of which `unicode-width` models: it calls both one
        // cell, so no width assertion can catch them. using the
        // alphabet is the guard, which is why the alphabet exists.
        let state = if self.paused { GLYPH_PAUSE } else { GLYPH_PLAY };
        let hints = format!(
            "({}: pause, {}: maximize)",
            key_label(self.keymap.get(KeyAction::TogglePause)),
            key_label(self.keymap.get(KeyAction::ToggleMinimize)),
        );
        let hint_cells = text_width(&hints) as u16;

        // hints against the right edge, title against the left, so the
        // strip reads as a bar with two ends instead of one sentence
        // huddled left of 29 dead columns.
        let hints_fit = area.width >= hint_cells + TITLE_FLOOR;
        let title_budget = if hints_fit {
            // a cell of clearance so a long title never touches the
            // opening paren.
            area.width - hint_cells - 1
        } else {
            area.width
        } as usize;

        // three tiers down from the glyph: it is the state, the title
        // is the headline, the hints are chrome. one flat NEAR_WHITE
        // across all three read as an undifferentiated sentence.
        let x = crate::ui::draw::at(
            buf,
            area.x,
            text_y,
            &format!("{state} "),
            shade(BRIGHT),
            title_budget,
        );
        let left = (x - area.x) as usize;
        let room = title_budget.saturating_sub(left);
        crate::ui::draw::at(
            buf,
            x,
            text_y,
            &truncate_with_ellipsis(self.title, room),
            shade(NEAR_WHITE),
            room,
        );
        if hints_fit {
            crate::ui::draw::at(
                buf,
                area.x + area.width - hint_cells,
                text_y,
                &hints,
                shade(DIM),
                hint_cells as usize,
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::testbuf::{buffer, grid};
    use crossterm::event::KeyCode;

    fn strip(w: u16, h: u16, title: &str) -> Buffer {
        let keymap = Keymap::default();
        let area = Rect::new(0, 0, w, h);
        let mut buf = buffer(w, h);
        MiniPlayerBar {
            title,
            paused: false,
            keymap: &keymap,
        }
        .render(area, &mut buf);
        buf
    }

    /// The strip used to be one left-aligned sentence, so a 72-cell
    /// row was title, hints, then 29 dead columns. The hints are the
    /// right end of a bar now.
    #[test]
    fn the_hints_end_on_the_right_edge() {
        for w in [48u16, 60, 72, 120] {
            let row = grid(&strip(w, HEIGHT, "Crest · Bladee"))
                .lines()
                .nth(1)
                .unwrap()
                .to_string();
            assert!(
                row.ends_with("maximize)"),
                "{w} cols left the hints floating mid-row: {row:?}"
            );
        }
    }

    /// Cut to fit, the hints became "(space: paus…" while the title
    /// beside them still had room: a half-hint answers nothing, so
    /// they go whole or not at all and the title takes the row.
    #[test]
    fn a_hint_too_wide_for_the_row_is_dropped_whole_not_cut() {
        for w in 1..=90u16 {
            let buf = strip(w, HEIGHT, "Crest · Bladee");
            let row = grid(&buf).lines().nth(1).unwrap().to_string();
            assert_eq!(
                row.contains('('),
                row.contains(')'),
                "{w} cols painted half a hint: {row:?}"
            );
            // and the title never loses the row to them.
            assert!(
                row.starts_with(GLYPH_PLAY),
                "{w} cols dropped the state glyph: {row:?}"
            );
        }
    }

    /// One row is all the strip gets on a short terminal; it used to
    /// spend it on the rule and say nothing about what was playing.
    #[test]
    fn a_single_row_strip_spends_it_on_the_title_not_the_rule() {
        let row = grid(&strip(60, 1, "Crest · Bladee"));
        assert!(row.contains("Crest · Bladee"), "{row:?}");
        assert!(!row.contains('─'), "the rule ate the only row: {row:?}");
    }

    /// Hierarchy, not decoration: state, then headline, then chrome.
    /// One flat NEAR_WHITE across all three read as a sentence.
    #[test]
    fn the_glyph_the_title_and_the_hints_are_three_brightnesses() {
        let buf = strip(72, HEIGHT, "Crest · Bladee");
        let fg = |x: u16| buf.cell((x, 1)).unwrap().fg;
        assert_eq!(fg(0), shade(BRIGHT), "the state glyph");
        assert_eq!(fg(2), shade(NEAR_WHITE), "the title");
        assert_eq!(fg(71), shade(DIM), "the hints");
        const {
            assert!(DIM < NEAR_WHITE && NEAR_BLACK < DIM);
        }
    }

    #[test]
    fn the_hints_follow_the_live_keymap() {
        let mut keymap = Keymap::default();
        keymap.set(KeyAction::TogglePause, KeyCode::F(5));
        keymap.set(KeyAction::ToggleMinimize, KeyCode::Char('z'));
        let area = Rect::new(0, 0, 60, HEIGHT);
        let mut buf = Buffer::empty(area);
        MiniPlayerBar {
            title: "a song",
            paused: false,
            keymap: &keymap,
        }
        .render(area, &mut buf);
        let out: String = (0..area.height)
            .map(|y| {
                (0..area.width)
                    .map(|x| buf.cell((x, y)).unwrap().symbol().to_string())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(out.contains("(f5: pause, z: maximize)"), "got:\n{out}");
        assert!(!out.contains("space: pause"), "stale default key:\n{out}");
        assert!(!out.contains("m: maximize"), "stale default key:\n{out}");
    }

    /// Renders the strip and returns it as one string per row.
    fn rendered(paused: bool) -> (Buffer, Rect) {
        let keymap = Keymap::default();
        let area = Rect::new(0, 0, 60, HEIGHT);
        let mut buf = Buffer::empty(area);
        MiniPlayerBar {
            title: "a song",
            paused,
            keymap: &keymap,
        }
        .render(area, &mut buf);
        (buf, area)
    }

    /// Every glyph on this row is one cell by `unicode-width`, and
    /// nothing takes a hue.
    ///
    /// Worth knowing what this does *not* catch: the emoji-presentation
    /// pair the strip used to draw (U+23F8, U+25B6) passes this test,
    /// because `unicode-width` reports both as a single cell. A
    /// terminal with an emoji font still renders them coloured and
    /// often double width. `the_state_glyph_is_the_bars_own_play_pause_pair`
    /// is the test with teeth there; this one guards the rest of the
    /// row.
    #[test]
    fn every_glyph_on_the_strip_is_one_cell_and_monochrome() {
        use crate::ui::textfit::width;
        for paused in [false, true] {
            let (buf, area) = rendered(paused);
            for y in 0..area.height {
                for x in 0..area.width {
                    let cell = buf.cell((x, y)).unwrap();
                    let sym = cell.symbol();
                    assert!(
                        width(sym) <= 1,
                        "{sym:?} at {x},{y} is {} cells wide",
                        width(sym)
                    );
                    if let ratatui::style::Color::Rgb(r, g, b) = cell.fg {
                        assert!(r == g && g == b, "{sym:?} took a hue: {:?}", cell.fg);
                    }
                }
            }
        }
    }

    #[test]
    fn the_state_glyph_is_the_bars_own_play_pause_pair() {
        for (paused, want) in [(false, GLYPH_PLAY), (true, GLYPH_PAUSE)] {
            let (buf, area) = rendered(paused);
            // row 0 is the rule; the content sits below it.
            let row: String = (0..area.height)
                .flat_map(|y| (0..area.width).map(move |x| (x, y)))
                .map(|(x, y)| buf.cell((x, y)).unwrap().symbol().to_string())
                .collect();
            assert!(
                row.contains(want),
                "paused={paused} should use the shared alphabet's {want:?}: {row:?}"
            );
        }
    }
}

#[cfg(test)]
mod eyeball {
    use super::*;
    use crate::ui::testbuf::{buffer, grid};

    /// Prints the strip at the widths main_content actually takes,
    /// including the narrow ones a wide sidebar leaves:
    /// `cargo test --lib ui::miniplayer::eyeball -- --nocapture`.
    /// Asserts the floor too, so it is a test and not just a printer.
    #[test]
    fn print_the_strip() {
        let keymap = Keymap::default();
        for (w, h, title) in [
            (72u16, HEIGHT, "Crest · Bladee"),
            (48, HEIGHT, "Crest · Bladee"),
            (30, HEIGHT, "Crest · Bladee"),
            (18, HEIGHT, "Crest · Bladee"),
            (8, HEIGHT, "Crest · Bladee"),
            (72, 1, "Crest · Bladee"),
            (
                72,
                HEIGHT,
                "A Very Long Track Title That Will Not Fit · Some Artist",
            ),
        ] {
            let area = Rect::new(0, 0, w, h);
            let mut buf = buffer(w, h);
            MiniPlayerBar {
                title,
                paused: false,
                keymap: &keymap,
            }
            .render(area, &mut buf);
            let text = grid(&buf);
            println!("--- {w}x{h} {title:?}\n{text}");
            assert!(
                text.chars().any(|c| c != '.' && c != '\n'),
                "{w}x{h} drew nothing at all"
            );
        }
    }
}
