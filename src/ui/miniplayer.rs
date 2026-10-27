use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::widgets::Widget;

use crate::theme::{shade, DIM, NEAR_WHITE};
use crate::ui::progress::{bar, fmt_time};
use crate::ui::textfit::truncate_with_ellipsis;

/// Thin ipod-style strip above the main bottom bar, spanning only
/// `main_content`'s width (not the sidebar): shows while the full
/// player is minimized, whether that's because an album's track
/// listing is occupying the workspace or the user minimized it by
/// hand. 'm' toggles back to the full player.
pub struct MiniPlayerBar<'a> {
    pub title: &'a str,
    pub elapsed_secs: Option<f64>,
    pub duration_secs: Option<f64>,
    pub paused: bool,
}

impl Widget for MiniPlayerBar<'_> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        if area.height == 0 {
            return;
        }
        let width = area.width as usize;
        let state = if self.paused { "⏸" } else { "▶" };
        let line1 = format!("{state} {} (m: maximize)", self.title);
        draw_line(buf, area, area.y, &truncate_with_ellipsis(&line1, width), shade(NEAR_WHITE));

        if area.height > 1 {
            let progress = bar(self.elapsed_secs, self.duration_secs, width.saturating_sub(12));
            let line2 = format!("{} {progress} {}", fmt_time(self.elapsed_secs), fmt_time(self.duration_secs));
            draw_line(buf, area, area.y + 1, &truncate_with_ellipsis(&line2, width), shade(DIM));
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
