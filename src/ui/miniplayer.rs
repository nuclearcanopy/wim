use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::widgets::Widget;

use crate::keymap::{key_label, KeyAction, Keymap};
use crate::theme::{shade, DIM, NEAR_BLACK, NEAR_WHITE};
use crate::ui::progress::{bar, fmt_time};
use crate::ui::textfit::truncate_with_ellipsis;

/// Thin ipod-style strip above the main bottom bar, spanning only
/// `main_content`'s width (not the sidebar): shows while the full
/// player is minimized, whether that's because an album's track
/// listing is occupying the workspace or the user minimized it by
/// hand. `KeyAction::ToggleMinimize` toggles back to the full player;
/// both keys hinted here are remappable, so they're resolved from
/// `keymap` rather than printed as their defaults.
pub struct MiniPlayerBar<'a> {
    pub title: &'a str,
    pub elapsed_secs: Option<f64>,
    pub duration_secs: Option<f64>,
    pub paused: bool,
    pub loop_label: &'a str,
    pub shuffle_on: bool,
    pub keymap: &'a Keymap,
}

/// Fixed height this widget always wants; the caller carves out
/// exactly this many rows above the main bottom bar.
pub const HEIGHT: u16 = 3;

impl Widget for MiniPlayerBar<'_> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        if area.height == 0 {
            return;
        }
        let width = area.width as usize;

        // a rule of its own, not just relying on the text above it
        // being a different brightness, so the strip reads as its own
        // separated panel rather than text that happens to float above
        // the bottom bar.
        for col in 0..area.width {
            if let Some(cell) = buf.cell_mut((area.x + col, area.y)) {
                cell.set_char('─');
                cell.set_fg(shade(NEAR_BLACK));
            }
        }
        if area.height == 1 {
            return;
        }

        let state = if self.paused { "⏸" } else { "▶" };
        let line1 = format!(
            "{state} {} ({}: pause, {}: maximize)",
            self.title,
            key_label(self.keymap.get(KeyAction::TogglePause)),
            key_label(self.keymap.get(KeyAction::ToggleMinimize)),
        );
        draw_line(
            buf,
            area,
            area.y + 1,
            &truncate_with_ellipsis(&line1, width),
            shade(NEAR_WHITE),
        );
        if area.height == 2 {
            return;
        }

        let progress = bar(
            self.elapsed_secs,
            self.duration_secs,
            width.saturating_sub(24),
        );
        let line2 = format!(
            "{} {progress} {}  ↻ {}  shuffle: {}",
            fmt_time(self.elapsed_secs),
            fmt_time(self.duration_secs),
            self.loop_label,
            if self.shuffle_on { "on" } else { "off" },
        );
        draw_line(
            buf,
            area,
            area.y + 2,
            &truncate_with_ellipsis(&line2, width),
            shade(DIM),
        );
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

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::KeyCode;

    #[test]
    fn the_hints_follow_the_live_keymap() {
        let mut keymap = Keymap::default();
        keymap.set(KeyAction::TogglePause, KeyCode::F(5));
        keymap.set(KeyAction::ToggleMinimize, KeyCode::Char('z'));
        let area = Rect::new(0, 0, 60, HEIGHT);
        let mut buf = Buffer::empty(area);
        MiniPlayerBar {
            title: "a song",
            elapsed_secs: Some(10.0),
            duration_secs: Some(100.0),
            paused: false,
            loop_label: "off",
            shuffle_on: false,
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
}
