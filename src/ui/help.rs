use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::widgets::Widget;

use crate::theme::{shade, DIM, NEAR_BLACK, NEAR_WHITE};
use crate::ui::header::draw_header;

/// Full keybind reference, opened with `?` (hinted by `HelpBox` in the
/// bottom-right corner); takes over the whole screen the same way
/// onboarding/the nav manager do, since it's a brief reference lookup
/// rather than something you'd want to see alongside anything else.
pub struct HelpOverlay;

const SECTIONS: &[(&str, &[(&str, &str)])] = &[
    (
        "navigation",
        &[
            ("/", "search"),
            ("j / k", "move selection"),
            ("gg / G", "jump to top / bottom"),
            ("enter", "play / open"),
            ("esc", "back / close"),
        ],
    ),
    (
        "playback",
        &[
            ("space", "pause / resume"),
            ("a", "queue"),
            ("f", "skip to next"),
            ("b", "back to previous"),
            ("l", "cycle loop: off / one / all"),
            ("s", "shuffle"),
            ("[ / ]", "speed"),
            ("e", "cycle eq preset"),
            ("c", "toggle crossfade"),
            (", / .", "subtitle delay -/+ 0.5s"),
        ],
    ),
    (
        "library",
        &[
            ("n", "navidrome profiles"),
            ("S", "starred songs"),
            ("I", "index breakdown"),
            ("*", "star / unstar"),
            ("L", "lyrics"),
        ],
    ),
    (
        "view",
        &[
            ("m", "minimize / maximize player"),
            ("i", "cycle album art: ascii / real / off"),
            ("r", "resize mode (a/w/s/d select, arrows stretch)"),
        ],
    ),
    ("other", &[("q", "quit"), ("?", "close this")]),
];

impl Widget for HelpOverlay {
    fn render(self, area: Rect, buf: &mut Buffer) {
        let mut y = draw_header(
            buf,
            area,
            area.y,
            "keybinds",
            "? or esc to close",
            shade(DIM),
            shade(NEAR_BLACK),
        );
        y += 1;

        for (section, binds) in SECTIONS {
            if y >= area.y + area.height {
                break;
            }
            draw_line(buf, area, y, section, shade(NEAR_WHITE));
            y += 1;
            for (key, desc) in *binds {
                if y >= area.y + area.height {
                    break;
                }
                draw_line(buf, area, y, &format!("  {key:<10} {desc}"), shade(DIM));
                y += 1;
            }
            y += 1;
        }
    }
}

fn draw_line(buf: &mut Buffer, area: Rect, y: u16, text: &str, color: ratatui::style::Color) {
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
