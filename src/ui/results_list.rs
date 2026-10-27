use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::widgets::Widget;

use crate::search::SearchItem;
use crate::theme::{shade, BRIGHT, DIM, NEAR_WHITE};

pub(crate) fn label(item: &SearchItem) -> String {
    match item {
        SearchItem::LocalMusic { title, artist, .. } => match artist {
            Some(a) => format!("{title} — {a}"),
            None => title.clone(),
        },
        SearchItem::LocalVideo { title, .. } => title.clone(),
        SearchItem::Artist { name, .. } => name.clone(),
        SearchItem::Album { name, artist, .. } => format!("{name} — {artist}"),
        SearchItem::Song { title, artist, album, .. } => format!("{title} — {artist} ({album})"),
        SearchItem::Playlist { name, .. } => name.clone(),
    }
}

/// Custom-painted results list: no stock `List` widget. Each row is
/// `[kind] label`, the selected row rendered near-white, others dim, and
/// the window scrolls to keep the selection visible without wrapping.
/// An empty `items` isn't a no-op render — a blank screen with no
/// explanation is a dead end, not a loved-app experience; `empty_message`
/// is shown instead so there's always *something* telling the user why
/// nothing's there and what to do about it.
pub struct ResultsList<'a> {
    pub items: &'a [SearchItem],
    pub selected: usize,
    pub empty_message: &'a str,
}

impl Widget for ResultsList<'_> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        if area.height == 0 {
            return;
        }

        if self.items.is_empty() {
            for (col, ch) in self.empty_message.chars().enumerate() {
                let x = area.x + col as u16;
                if x >= area.x + area.width {
                    break;
                }
                if let Some(cell) = buf.cell_mut((x, area.y)) {
                    cell.set_char(ch);
                    cell.set_fg(shade(DIM));
                }
            }
            return;
        }

        let visible_rows = area.height as usize;
        let scroll_offset = if self.selected >= visible_rows {
            self.selected + 1 - visible_rows
        } else {
            0
        };

        for row in 0..visible_rows {
            let idx = scroll_offset + row;
            let Some(item) = self.items.get(idx) else {
                break;
            };
            let is_selected = idx == self.selected;
            let text = format!("[{}] {}", item.kind_label(), label(item));
            let y = area.y + row as u16;

            let fg = if is_selected { shade(NEAR_WHITE) } else { shade(DIM) };
            let marker = if is_selected { "> " } else { "  " };

            for (col, ch) in marker.chars().chain(text.chars()).enumerate() {
                let x = area.x + col as u16;
                if x >= area.x + area.width {
                    break;
                }
                if let Some(cell) = buf.cell_mut((x, y)) {
                    cell.set_char(ch);
                    cell.set_fg(if is_selected { shade(BRIGHT) } else { fg });
                }
            }
        }
    }
}
