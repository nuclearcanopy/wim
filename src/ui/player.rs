use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::widgets::Widget;

use crate::theme::{shade, BRIGHT, DIM, NEAR_WHITE};
use crate::ui::progress::{bar, fmt_time};
use crate::ui::textfit::truncate_with_ellipsis;

/// The full "maximized" now-playing screen: takes over `main_content`
/// entirely, same as the banner/browse view it replaces. 'm' minimizes
/// it back down to `MiniPlayerBar`.
pub struct PlayerView<'a> {
    pub title: &'a str,
    pub elapsed_secs: Option<f64>,
    pub duration_secs: Option<f64>,
    pub paused: bool,
    pub loop_label: &'a str,
    pub shuffle_on: bool,
    pub speed: f64,
    pub eq_preset: &'a str,
    pub previous: Option<&'a str>,
    pub next: Option<&'a str>,
}

impl Widget for PlayerView<'_> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        if area.height == 0 {
            return;
        }
        let mid_y = area.y + area.height / 2;

        if let Some(prev) = self.previous {
            draw_centered(buf, area, mid_y.saturating_sub(4), &format!("⏮ {prev}"), shade(DIM));
        }

        let state_icon = if self.paused { "⏸" } else { "▶" };
        draw_centered(buf, area, mid_y.saturating_sub(2), state_icon, shade(DIM));
        draw_centered(buf, area, mid_y.saturating_sub(1), self.title, shade(NEAR_WHITE));

        let bar_width = (area.width as usize).saturating_sub(14).min(40);
        let progress = bar(self.elapsed_secs, self.duration_secs, bar_width);
        let time_line = format!("{} {progress} {}", fmt_time(self.elapsed_secs), fmt_time(self.duration_secs));
        draw_centered(buf, area, mid_y + 1, &time_line, shade(BRIGHT));

        let state_line = format!(
            "↻ {}  shuffle: {}  speed: {:.2}x  eq: {}",
            self.loop_label,
            if self.shuffle_on { "on" } else { "off" },
            self.speed,
            self.eq_preset,
        );
        draw_centered(buf, area, mid_y + 3, &state_line, shade(DIM));

        if let Some(next) = self.next {
            draw_centered(buf, area, mid_y + 5, &format!("{next} ⏭"), shade(DIM));
        }

        draw_centered(buf, area, mid_y + 7, "m: minimize", shade(DIM));
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
