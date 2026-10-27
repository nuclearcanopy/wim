use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::widgets::Widget;

use crate::keymap::{key_label, KeyAction, Keymap};
use crate::theme::{shade, BRIGHT, DIM, NEAR_WHITE};
use crate::ui::progress::{bar, fmt_time};
use crate::ui::textfit::truncate_with_ellipsis;

/// The full "maximized" now-playing screen: takes over `main_content`
/// entirely, same as the banner/browse view it replaces. Minimizing it
/// back down to `MiniPlayerBar` is `KeyAction::ToggleMinimize`; every
/// key hinted on this screen is remappable, so all of them are
/// resolved from `keymap` rather than printed as their default.
pub struct PlayerView<'a> {
    pub title: &'a str,
    pub elapsed_secs: Option<f64>,
    pub duration_secs: Option<f64>,
    pub paused: bool,
    pub loop_label: &'a str,
    pub shuffle_on: bool,
    pub speed: f64,
    pub eq_preset: &'a str,
    pub crossfade_on: bool,
    pub previous: Option<&'a str>,
    pub next: Option<&'a str>,
    pub keymap: &'a Keymap,
}

impl Widget for PlayerView<'_> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        if area.height == 0 {
            return;
        }
        let mid_y = area.y + area.height / 2;

        if let Some(prev) = self.previous {
            draw_centered(
                buf,
                area,
                mid_y.saturating_sub(4),
                &format!("⏮ {prev}"),
                shade(DIM),
            );
        }

        draw_centered(
            buf,
            area,
            mid_y.saturating_sub(1),
            self.title,
            shade(NEAR_WHITE),
        );

        let bar_width = (area.width as usize).saturating_sub(14).min(40);
        let progress = bar(self.elapsed_secs, self.duration_secs, bar_width);
        let time_line = format!(
            "{} {progress} {}",
            fmt_time(self.elapsed_secs),
            fmt_time(self.duration_secs)
        );
        draw_centered(buf, area, mid_y + 1, &time_line, shade(BRIGHT));

        // each control's keybind sits right above its icon rather
        // than crammed onto the same line, so it reads as a labeled
        // button rather than a word salad.
        let play_icon = if self.paused { "⏸" } else { "▶" };
        let key = |action| key_label(self.keymap.get(action));
        draw_transport_row(
            buf,
            area,
            mid_y + 3,
            &[
                (key(KeyAction::SkipBack), "⏮"),
                (key(KeyAction::TogglePause), play_icon),
                (key(KeyAction::SkipNext), "⏭"),
                (key(KeyAction::CycleLoop), "↻"),
            ],
            shade(DIM),
        );

        let state_line = format!(
            "{}  shuffle: {}  speed: {:.2}x  eq: {}  crossfade ({}): {}",
            self.loop_label,
            if self.shuffle_on { "on" } else { "off" },
            self.speed,
            self.eq_preset,
            key(KeyAction::ToggleCrossfade),
            if self.crossfade_on { "on" } else { "off" },
        );
        draw_centered(buf, area, mid_y + 6, &state_line, shade(DIM));

        if let Some(next) = self.next {
            draw_centered(buf, area, mid_y + 8, &format!("{next} ⏭"), shade(DIM));
        }

        draw_centered(
            buf,
            area,
            mid_y + 10,
            &format!("{}: minimize", key(KeyAction::ToggleMinimize)),
            shade(DIM),
        );
    }
}

/// Two rows (keybind label, then its icon), each pair centered in its
/// own fixed-width column so a one-char icon and a five-char label
/// ("space") still line up over each other.
fn draw_transport_row(
    buf: &mut Buffer,
    area: Rect,
    y: u16,
    items: &[(String, &str)],
    color: ratatui::style::Color,
) {
    const COL_WIDTH: u16 = 8;
    let total_width = COL_WIDTH * items.len() as u16;
    let start_x = area.x + area.width.saturating_sub(total_width) / 2;
    for (i, (label, icon)) in items.iter().enumerate() {
        let col = Rect {
            x: start_x + i as u16 * COL_WIDTH,
            y,
            width: COL_WIDTH,
            height: 2,
        };
        draw_centered(buf, col, col.y, label, color);
        draw_centered(buf, col, col.y + 1, icon, color);
    }
}

fn draw_centered(buf: &mut Buffer, area: Rect, y: u16, text: &str, color: ratatui::style::Color) {
    if y >= area.y + area.height || y < area.y {
        return;
    }
    let text = truncate_with_ellipsis(text, area.width as usize);
    let text_width = text.chars().count() as u16;
    let start_x = area.x + area.width.saturating_sub(text_width) / 2;
    for (i, ch) in text.chars().enumerate() {
        let x = start_x + i as u16;
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

    fn render_to_string(keymap: &Keymap) -> String {
        let area = Rect::new(0, 0, 100, 30);
        let mut buf = Buffer::empty(area);
        PlayerView {
            title: "a song",
            elapsed_secs: Some(10.0),
            duration_secs: Some(100.0),
            paused: false,
            loop_label: "loop: off",
            shuffle_on: false,
            speed: 1.0,
            eq_preset: "flat",
            crossfade_on: false,
            previous: None,
            next: None,
            keymap,
        }
        .render(area, &mut buf);
        (0..area.height)
            .map(|y| {
                (0..area.width)
                    .map(|x| buf.cell((x, y)).unwrap().symbol().to_string())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn every_hint_follows_the_live_keymap() {
        let mut keymap = Keymap::default();
        keymap.set(KeyAction::ToggleMinimize, KeyCode::Char('z'));
        keymap.set(KeyAction::ToggleCrossfade, KeyCode::Char('x'));
        keymap.set(KeyAction::TogglePause, KeyCode::F(5));
        let out = render_to_string(&keymap);
        assert!(out.contains("z: minimize"), "got:\n{out}");
        assert!(out.contains("crossfade (x):"), "got:\n{out}");
        assert!(out.contains("f5"), "transport row kept a default:\n{out}");
        assert!(!out.contains("m: minimize"), "stale default key:\n{out}");
        assert!(!out.contains("crossfade (c)"), "stale default key:\n{out}");
        assert!(!out.contains("space"), "stale default key:\n{out}");
    }
}
