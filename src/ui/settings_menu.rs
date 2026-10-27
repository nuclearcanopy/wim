use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::widgets::Widget;

use crate::theme::{shade, DIM, NEAR_BLACK, NEAR_WHITE};
use crate::ui::header::draw_header;

pub const FIELD_COUNT: usize = 8;
pub const FIELD_LABELS: [&str; FIELD_COUNT] = [
    "album art",
    "crossfade",
    "crossfade seconds",
    "cache after plays",
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
}

impl Widget for SettingsMenu<'_> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        let mut y = draw_header(
            buf,
            area,
            area.y,
            "settings",
            "u or esc to close",
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
            "other settings: '?' keybinds, 't' subtitles, left/right on the home screen logo",
            shade(DIM),
        );
    }
}

fn draw_line(buf: &mut Buffer, area: Rect, y: u16, text: &str, color: ratatui::style::Color) {
    if y >= area.y + area.height {
        return;
    }
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
