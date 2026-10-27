use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::widgets::Widget;

use crate::theme::{shade, DIM, NEAR_WHITE};
use crate::ui::textfit::truncate_with_ellipsis;

/// Bottom playback bar: as many lines as `area.height` allows, most
/// important first, rather than cramming everything onto one status
/// line. Resizable (bottom bar, vertical axis only); more rows reveal
/// more lines gracefully.
pub struct ControlsBar<'a> {
    pub status: &'a str,
    pub queue_len: usize,
    pub loop_label: &'a str,
    pub shuffle_on: bool,
    pub speed: f64,
    pub eq_preset: &'a str,
    pub resize_hint: Option<&'a str>,
}

impl Widget for ControlsBar<'_> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        let width = area.width as usize;
        let mut y = area.y;

        if let Some(hint) = self.resize_hint {
            draw_line(buf, area, y, hint, width, shade(NEAR_WHITE));
            y += 1;
        }

        draw_line(buf, area, y, self.status, width, shade(NEAR_WHITE));
        y += 1;
        if y >= area.y + area.height {
            return;
        }

        let state_line = format!(
            "queue: {}  loop: {}  shuffle: {}  speed: {:.2}x  eq: {}",
            self.queue_len,
            self.loop_label,
            if self.shuffle_on { "on" } else { "off" },
            self.speed,
            self.eq_preset,
        );
        draw_line(buf, area, y, &state_line, width, shade(DIM));
    }
}

fn draw_line(
    buf: &mut Buffer,
    area: Rect,
    y: u16,
    text: &str,
    width: usize,
    color: ratatui::style::Color,
) {
    if y >= area.y + area.height {
        return;
    }
    let fitted = truncate_with_ellipsis(text, width);
    for (col, ch) in fitted.chars().enumerate() {
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
