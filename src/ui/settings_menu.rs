use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::widgets::Widget;

use crate::keymap::{key_label, KeyAction, Keymap};
use crate::theme::{shade, DIM, NEAR_BLACK, NEAR_WHITE};
use crate::ui::header::draw_header;

pub const FIELD_COUNT: usize = 8;
pub const FIELD_LABELS: [&str; FIELD_COUNT] = [
    "album art",
    "crossfade",
    "crossfade seconds",
    "cache after days played",
    "cache eviction days",
    "scrobble % threshold",
    "scrobble seconds threshold",
    "listenbrainz token",
];

/// Takes over the whole workspace (sidebar included, never the
/// bottom bar) the same way the help overlay takes over the whole
/// terminal; a persistent settings surface is exactly the kind of
/// thing that should be easy to find rather than a small popup, which
/// is what the centered subtitle menu is for instead.
pub struct SettingsMenu<'a> {
    pub values: [&'a str; FIELD_COUNT],
    pub selected: usize,
    pub keymap: &'a Keymap,
}

impl Widget for SettingsMenu<'_> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        // only the remappable keys come from the keymap; j/k, h/l,
        // enter, esc and the arrows are the fixed conventions
        // reserved in keymap.rs, so they stay literal.
        let key = |action| key_label(self.keymap.get(action));
        let mut y = draw_header(
            buf,
            area,
            area.y,
            "settings",
            &format!("{} or esc to close", key(KeyAction::OpenSettingsMenu)),
            shade(DIM),
            shade(NEAR_BLACK),
        );
        y += 1;

        let key_width = FIELD_LABELS.iter().map(|l| l.len()).max().unwrap_or(0);
        for (i, (label, value)) in FIELD_LABELS.iter().zip(self.values.iter()).enumerate() {
            if y >= area.y + area.height {
                break;
            }
            let color = if i == self.selected {
                shade(NEAR_WHITE)
            } else {
                shade(DIM)
            };
            let marker = if i == self.selected { '>' } else { ' ' };
            draw_line(
                buf,
                area,
                y,
                &format!("{marker} {label:<key_width$}  {value}"),
                color,
            );
            y += 1;
        }
        y += 1;
        draw_line(
            buf,
            area,
            y,
            "j/k select, h/l adjust, enter on 'listenbrainz token' to set/change it",
            shade(DIM),
        );
        y += 1;
        draw_line(
            buf,
            area,
            y,
            &format!(
                "other settings: '{}' keybinds, '{}' subtitles, left/right on the home screen logo",
                key(KeyAction::OpenHelp),
                key(KeyAction::OpenSubtitleMenu),
            ),
            shade(DIM),
        );
    }
}

fn draw_line(buf: &mut Buffer, area: Rect, y: u16, text: &str, color: ratatui::style::Color) {
    crate::ui::draw::line(buf, area, y, text, color);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::testbuf::{buffer, char_at, grid};
    use crossterm::event::KeyCode;

    fn render(area: Rect, buf: &mut Buffer, values: [&str; FIELD_COUNT]) {
        SettingsMenu {
            values,
            selected: 0,
            keymap: &Keymap::default(),
        }
        .render(area, buf);
    }

    #[test]
    fn every_field_gets_a_row_with_its_value_in_an_aligned_column() {
        let area = Rect::new(0, 0, 80, 14);
        let mut buf = buffer(80, 14);
        render(
            area,
            &mut buf,
            ["on", "off", "3", "7", "30", "50", "240", "set"],
        );
        assert_eq!(
            grid(&buf),
            [
                "settings │ u or esc to close....................................................",
                "─────────┴──────────────────────────────────────────────────────────────────────",
                "................................................................................",
                "> album art                   on................................................",
                "  crossfade                   off...............................................",
                "  crossfade seconds           3.................................................",
                "  cache after days played     7.................................................",
                "  cache eviction days         30................................................",
                "  scrobble % threshold        50................................................",
                "  scrobble seconds threshold  240...............................................",
                "  listenbrainz token          set...............................................",
                "................................................................................",
                "j/k select, h/l adjust, enter on 'listenbrainz token' to set/change it..........",
                "other settings: '?' keybinds, 't' subtitles, left/right on the home screen logo.",
            ]
            .join("\n")
        );
    }

    /// `values` is the user's own config (a listenbrainz token, a path
    /// label); a wide char in one must not shove the rest of its row.
    #[test]
    fn a_wide_value_does_not_shift_the_glyphs_after_it() {
        let area = Rect::new(0, 0, 60, 14);
        let mut buf = buffer(60, 14);
        render(area, &mut buf, ["日本x", "", "", "", "", "", "", ""]);
        let row = grid(&buf).lines().nth(3).unwrap().to_string();
        let col = row.find('日').expect("the wide value");
        // two cells each, so 'x' lands four columns on, not two.
        assert_eq!(row.chars().nth(col + 1), Some(' '), "{row}");
        assert_eq!(row.chars().nth(col + 2), Some('本'), "{row}");
        assert_eq!(row.chars().nth(col + 4), Some('x'), "{row}");
    }

    #[test]
    fn a_wide_value_stays_inside_the_rect() {
        let mut buf = buffer(40, 14);
        let area = Rect::new(2, 1, 24, 11);
        let long = "日本語テスト曲日本語テスト曲日本語テスト曲";
        render(area, &mut buf, [long; FIELD_COUNT]);
        for y in 0..14u16 {
            for x in 0..40u16 {
                let inside = x >= 2 && x < 26 && y >= 1 && y < 12;
                if !inside {
                    assert_eq!(
                        char_at(&buf, x, y),
                        '.',
                        "wrote outside its rect at ({x}, {y}):\n{}",
                        grid(&buf)
                    );
                }
            }
        }
    }

    #[test]
    fn degenerate_sizes_do_not_panic() {
        for (w, h) in [(1, 1), (1, 40), (2, 1), (40, 0), (0, 0), (3, 2)] {
            let mut buf = buffer(w.max(1), h.max(1));
            render(Rect::new(0, 0, w, h), &mut buf, ["日本語"; FIELD_COUNT]);
        }
    }

    #[test]
    fn the_hints_follow_the_live_keymap_but_keep_the_fixed_keys() {
        let mut keymap = Keymap::default();
        keymap.set(KeyAction::OpenSettingsMenu, KeyCode::Char('x'));
        keymap.set(KeyAction::OpenHelp, KeyCode::Char('y'));
        keymap.set(KeyAction::OpenSubtitleMenu, KeyCode::Char('z'));
        let area = Rect::new(0, 0, 100, 20);
        let mut buf = Buffer::empty(area);
        SettingsMenu {
            values: ["", "", "", "", "", "", "", ""],
            selected: 0,
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
        assert!(out.contains("x or esc to close"), "got:\n{out}");
        assert!(out.contains("'y' keybinds"), "got:\n{out}");
        assert!(out.contains("'z' subtitles"), "got:\n{out}");
        assert!(!out.contains("u or esc to close"), "stale default:\n{out}");
        assert!(!out.contains("'?' keybinds"), "stale default:\n{out}");
        assert!(!out.contains("'t' subtitles"), "stale default:\n{out}");
        // the reserved navigation keys are not remappable and must
        // not start following the keymap.
        assert!(out.contains("j/k select, h/l adjust"), "got:\n{out}");
    }
}
