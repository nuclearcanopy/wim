use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::widgets::Widget;

use crate::config::NavidromeConfig;
use crate::theme::{shade, DIM, NEAR_BLACK, NEAR_WHITE};
use crate::ui::textfit::truncate_with_ellipsis;

/// The profile list — not shown when adding a new one (that reuses
/// `LoginView` directly). Selecting an existing profile reconnects/
/// refreshes it; `d` removes it; the last row is always "add new".
pub struct NavManagerListView<'a> {
    pub servers: &'a [NavidromeConfig],
    pub selected: usize,
    pub message: &'a str,
}

impl Widget for NavManagerListView<'_> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        let width = area.width as usize;
        let mut y = area.y;
        draw_line(
            buf,
            area,
            y,
            "navidrome profiles — enter: reconnect/add, d: remove, esc: close",
            shade(DIM),
        );
        y += 1;
        draw_line(buf, area, y, &"─".repeat(width), shade(NEAR_BLACK));
        y += 2;

        if self.servers.is_empty() {
            draw_line(buf, area, y, "(no profiles yet)", shade(DIM));
            y += 1;
        }

        for (i, server) in self.servers.iter().enumerate() {
            let active = i == self.selected;
            let marker = if active { "> " } else { "  " };
            let line = format!("{marker}{} ({})", server.url, server.username);
            draw_line(
                buf,
                area,
                y,
                &truncate_with_ellipsis(&line, width),
                if active { shade(NEAR_WHITE) } else { shade(DIM) },
            );
            y += 1;
        }

        let add_active = self.selected == self.servers.len();
        let marker = if add_active { "> " } else { "  " };
        draw_line(
            buf,
            area,
            y,
            &format!("{marker}[ add new profile ]"),
            if add_active { shade(NEAR_WHITE) } else { shade(DIM) },
        );
        y += 2;

        draw_line(buf, area, y, &truncate_with_ellipsis(self.message, width), shade(DIM));
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
