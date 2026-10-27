use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::widgets::Widget;

use crate::eq::{BAND_HZ, PRESETS};
use crate::theme::{shade, DIM, NEAR_BLACK, NEAR_WHITE};
use crate::ui::header::draw_header;

/// Rows `0..PRESET_COUNT` pick a named preset (applied live the
/// instant j/k lands on one); rows `PRESET_COUNT..ROW_COUNT` each
/// adjust one band's gain in place with h/l, diverging into "custom"
/// the moment any band actually changes.
pub const PRESET_COUNT: usize = PRESETS.len();
const BAND_COUNT: usize = BAND_HZ.len();
pub const ROW_COUNT: usize = PRESET_COUNT + BAND_COUNT;

/// Opened by pressing enter on the bottom bar's Eq field while
/// `Focus::BottomBar` has it selected; full-workspace like the
/// settings/keybinds menus (a preset list plus 10 band rows needs more
/// room than the small centered subtitle-menu box gives).
pub struct EqMenu<'a> {
    pub gains: &'a [f64; 10],
    pub custom: bool,
    /// Index into `PRESETS` currently applied; meaningless while
    /// `custom` is true (no row claims credit for a custom sound).
    pub active_preset: usize,
    pub selected: usize,
}

impl Widget for EqMenu<'_> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        let mut y = draw_header(
            buf,
            area,
            area.y,
            "eq",
            "enter: apply preset, h/l: nudge band, esc: close",
            shade(DIM),
            shade(NEAR_BLACK),
        );
        y += 1;

        draw_line(buf, area, y, "presets", shade(NEAR_WHITE));
        y += 1;
        for (i, preset) in PRESETS.iter().enumerate() {
            if y >= area.y + area.height {
                break;
            }
            let is_selected = i == self.selected;
            let is_active = !self.custom && i == self.active_preset;
            let color = if is_selected {
                shade(NEAR_WHITE)
            } else {
                shade(DIM)
            };
            let marker = if is_selected { '>' } else { ' ' };
            let suffix = if is_active { "  (active)" } else { "" };
            draw_line(
                buf,
                area,
                y,
                &format!("{marker} {}{suffix}", preset.name),
                color,
            );
            y += 1;
        }

        y += 1;
        if y < area.y + area.height {
            draw_line(
                buf,
                area,
                y,
                "bands (h/l adjust by 1 dB, -12..12)",
                shade(NEAR_WHITE),
            );
            y += 1;
        }
        let label_width = BAND_HZ
            .iter()
            .map(|hz| format!("{hz:.0} Hz").len())
            .max()
            .unwrap_or(0);
        for (i, hz) in BAND_HZ.iter().enumerate() {
            if y >= area.y + area.height {
                break;
            }
            let row = PRESET_COUNT + i;
            let is_selected = row == self.selected;
            let color = if is_selected {
                shade(NEAR_WHITE)
            } else {
                shade(DIM)
            };
            let marker = if is_selected { '>' } else { ' ' };
            let label = format!("{hz:.0} Hz");
            draw_line(
                buf,
                area,
                y,
                &format!("{marker} {label:<label_width$}  {:+.1} dB", self.gains[i]),
                color,
            );
            y += 1;
        }

        y += 1;
        if y < area.y + area.height {
            draw_line(buf, area, y, "j/k select, h/l adjust band", shade(DIM));
            y += 1;
        }
        if y < area.y + area.height {
            let state = if self.custom { "custom" } else { "preset" };
            draw_line(buf, area, y, &format!("state: {state}"), shade(DIM));
        }
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
