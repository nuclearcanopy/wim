use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::widgets::Widget;

use crate::keymap::ALL_ACTIONS;
use crate::theme::{shade, DIM, NEAR_BLACK, NEAR_WHITE};
use crate::ui::header::draw_header;

/// Every remappable action and its current key, full-workspace like
/// the settings menu (this effectively *is* one of its settings,
/// just big enough to deserve its own screen rather than a row
/// inside that one). `capturing` shows the "press a key..." prompt
/// in place of the selected row's key while main.rs is waiting for
/// the next keypress to bind.
pub struct KeybindsMenu<'a> {
    pub keys: &'a [String],
    pub selected: usize,
    pub capturing: bool,
    pub conflict: Option<&'a str>,
}

impl Widget for KeybindsMenu<'_> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        let mut y = draw_header(
            buf,
            area,
            area.y,
            "keybinds",
            "k to close",
            shade(DIM),
            shade(NEAR_BLACK),
        );
        y += 1;

        let label_width = ALL_ACTIONS
            .iter()
            .map(|a| a.label().len())
            .max()
            .unwrap_or(0);
        for (i, action) in ALL_ACTIONS.iter().enumerate() {
            if y >= area.y + area.height {
                break;
            }
            let is_selected = i == self.selected;
            let color = if is_selected {
                shade(NEAR_WHITE)
            } else {
                shade(DIM)
            };
            let marker = if is_selected { '>' } else { ' ' };
            let key_text = if is_selected && self.capturing {
                "press a key...".to_string()
            } else {
                self.keys[i].clone()
            };
            draw_line(
                buf,
                area,
                y,
                &format!("{marker} {:<label_width$}  {key_text}", action.label(),),
                color,
            );
            y += 1;
        }
        y += 1;
        draw_line(
            buf,
            area,
            y,
            "j/k select, r rebind selected, 0 reset selected, shift+r reset all",
            shade(DIM),
        );
        if let Some(owner) = self.conflict {
            y += 1;
            draw_line(
                buf,
                area,
                y,
                &format!("already bound to \"{owner}\"; rebinding anyway"),
                shade(NEAR_WHITE),
            );
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
