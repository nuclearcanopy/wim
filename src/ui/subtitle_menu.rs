use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::widgets::Widget;

use crate::keymap::{key_label, KeyAction, Keymap};
use crate::theme::{shade, DIM, NEAR_BLACK, NEAR_WHITE};

/// One adjustable row: label plus the already-formatted current
/// value, so this widget stays pure rendering and knows nothing about
/// `SubtitleConfig`'s actual field types (a mix of f64/u32/f32) or
/// their individual step sizes/bounds; main.rs owns all of that.
pub const FIELD_COUNT: usize = 7;
pub const FIELD_LABELS: [&str; FIELD_COUNT] = [
    "delay",
    "font size",
    "position",
    "outline",
    "background",
    "blur",
    "color",
];

/// Centered bordered box, not a full-workspace takeover like the
/// help/settings overlays: "centered... and cover it" was the
/// explicit ask for this one specifically, smaller and opaque over
/// just its own footprint rather than the whole screen.
pub struct SubtitleMenu<'a> {
    pub values: [&'a str; FIELD_COUNT],
    pub selected: usize,
    pub keymap: &'a Keymap,
}

const WIDTH: u16 = 32;

impl Widget for SubtitleMenu<'_> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        let height = FIELD_COUNT as u16 + 4;
        if area.width < WIDTH || area.height < height {
            return;
        }
        let x = area.x + (area.width - WIDTH) / 2;
        let y = area.y + area.height.saturating_sub(height) / 2;
        let rect = Rect {
            x,
            y,
            width: WIDTH,
            height,
        };
        draw_box(buf, rect, shade(NEAR_BLACK));
        // the close key is remappable; j/k and h/l are the fixed
        // conventions reserved in keymap.rs, so they stay literal.
        let close = key_label(self.keymap.get(KeyAction::OpenSubtitleMenu));
        draw_text(
            buf,
            x + 2,
            y,
            &format!("subtitles ({close} to close)"),
            shade(NEAR_WHITE),
        );
        draw_text(buf, x + 2, y + 1, "j/k select, h/l adjust", shade(DIM));

        for (i, (label, value)) in FIELD_LABELS.iter().zip(self.values.iter()).enumerate() {
            let row_y = y + 3 + i as u16;
            let color = if i == self.selected {
                shade(NEAR_WHITE)
            } else {
                shade(DIM)
            };
            let marker = if i == self.selected { '>' } else { ' ' };
            draw_text(buf, x + 2, row_y, &format!("{marker} {label}"), color);
            let value_x = x + WIDTH - 2 - value.chars().count() as u16;
            draw_text(buf, value_x, row_y, value, color);
        }
    }
}

fn draw_box(buf: &mut Buffer, rect: Rect, color: ratatui::style::Color) {
    put(buf, rect.x, rect.y, '┌', color);
    put(buf, rect.x + rect.width - 1, rect.y, '┐', color);
    put(buf, rect.x, rect.y + rect.height - 1, '└', color);
    put(
        buf,
        rect.x + rect.width - 1,
        rect.y + rect.height - 1,
        '┘',
        color,
    );
    for x in 1..rect.width - 1 {
        put(buf, rect.x + x, rect.y, '─', color);
        put(buf, rect.x + x, rect.y + rect.height - 1, '─', color);
    }
    for y in 1..rect.height - 1 {
        put(buf, rect.x, rect.y + y, '│', color);
        put(buf, rect.x + rect.width - 1, rect.y + y, '│', color);
        for x in 1..rect.width - 1 {
            put(buf, rect.x + x, rect.y + y, ' ', color);
        }
    }
}

fn draw_text(buf: &mut Buffer, x: u16, y: u16, text: &str, color: ratatui::style::Color) {
    for (i, ch) in text.chars().enumerate() {
        put(buf, x + i as u16, y, ch, color);
    }
}

fn put(buf: &mut Buffer, x: u16, y: u16, ch: char, color: ratatui::style::Color) {
    if let Some(cell) = buf.cell_mut((x, y)) {
        cell.set_char(ch);
        cell.set_fg(color);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::KeyCode;

    #[test]
    fn the_close_hint_follows_the_live_keymap() {
        let mut keymap = Keymap::default();
        keymap.set(KeyAction::OpenSubtitleMenu, KeyCode::Char('z'));
        let area = Rect::new(0, 0, 60, 20);
        let mut buf = Buffer::empty(area);
        SubtitleMenu {
            values: ["", "", "", "", "", "", ""],
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
        assert!(out.contains("subtitles (z to close)"), "got:\n{out}");
        assert!(!out.contains("(t to close)"), "stale default key:\n{out}");
        // h/l and j/k are reserved, not keymap entries.
        assert!(out.contains("j/k select, h/l adjust"), "got:\n{out}");
    }
}
