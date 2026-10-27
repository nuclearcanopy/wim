use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::widgets::Widget;

use crate::browse::BrowseView;
use crate::search::SearchItem;
use crate::theme::{shade, BRIGHT, DIM, NEAR_BLACK};
use crate::ui::header::draw_header;
use crate::ui::textfit::truncate_with_ellipsis;

/// Track listing for an opened album/playlist, in `main_content`
/// (not full-screen: the sidebar and bottom bar stay visible, this is
/// "the main workspace" getting overwritten, not a modal on top of it).
pub struct BrowseListView<'a> {
    pub view: &'a BrowseView,
}

impl Widget for BrowseListView<'_> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        let width = area.width as usize;

        let subtitle = match (self.view.subtitle.is_empty(), self.view.year) {
            (false, Some(y)) => format!(" · {} · {y}", self.view.subtitle),
            (false, None) => format!(" · {}", self.view.subtitle),
            (true, Some(y)) => format!(" · {y}"),
            (true, None) => String::new(),
        };
        let title_line = format!("{}{subtitle}", self.view.title);
        let mut y = draw_header(
            buf,
            area,
            area.y,
            &title_line,
            "enter: play from here, esc: close",
            shade(DIM),
            shade(NEAR_BLACK),
        );
        y += 1;

        if self.view.tracks.is_empty() {
            draw_line(buf, area, y, "(no tracks)", shade(DIM));
            return;
        }

        // same clamp-to-selection scroll as ResultsList: a long album
        // shouldn't be unreachable past the bottom of the pane.
        let visible_rows = (area.y + area.height).saturating_sub(y) as usize;
        let scroll_offset = if self.view.selected >= visible_rows {
            self.view.selected + 1 - visible_rows
        } else {
            0
        };

        for row in 0..visible_rows {
            let idx = scroll_offset + row;
            let Some(track) = self.view.tracks.get(idx) else { break };
            let is_selected = idx == self.view.selected;
            let marker = if is_selected { "> " } else { "  " };
            let label = match track {
                SearchItem::Song { title, artist, .. } => format!("{title} · {artist}"),
                _ => String::new(),
            };
            let line = format!("{marker}{:>2}. {label}", idx + 1);
            draw_line(
                buf,
                area,
                y + row as u16,
                &truncate_with_ellipsis(&line, width),
                if is_selected { shade(BRIGHT) } else { shade(DIM) },
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
