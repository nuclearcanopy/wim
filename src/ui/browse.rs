use std::collections::HashSet;

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::widgets::Widget;

use crate::browse::{ArtMode, BrowseView};
use crate::favorites::Favorites;
use crate::search::SearchItem;
use crate::theme::{shade, BRIGHT, DIM, NEAR_BLACK};
use crate::ui::albumart::AsciiArt;
use crate::ui::header::draw_header;
use crate::ui::textfit::truncate_with_ellipsis;

/// Track listing for an opened album/playlist, in `main_content`
/// (not full-screen: the sidebar and bottom bar stay visible, this is
/// "the main workspace" getting overwritten, not a modal on top of it).
pub struct BrowseListView<'a> {
    pub view: &'a BrowseView,
    pub favorites: &'a Favorites,
    pub remote_starred: &'a HashSet<String>,
    /// In `Real` mode the art panel is left to main.rs, which overlays
    /// a `ratatui-image` stateful widget on the exact same rect
    /// `browse::layout` gives this widget; this just skips drawing the
    /// ascii fallback there instead of drawing over it.
    pub art_mode: ArtMode,
}

fn is_starred(track: &SearchItem, favorites: &Favorites, remote_starred: &HashSet<String>) -> bool {
    match track {
        SearchItem::LocalMusic { path, .. } | SearchItem::LocalVideo { path, .. } => {
            favorites.is_starred(&path.to_string_lossy())
        }
        SearchItem::Song { id, .. } => remote_starred.contains(id),
        SearchItem::Artist { .. } | SearchItem::Album { .. } | SearchItem::Playlist { .. } => false,
    }
}

impl Widget for BrowseListView<'_> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        let subtitle = match (self.view.subtitle.is_empty(), self.view.year) {
            (false, Some(y)) => format!(" · {} · {y}", self.view.subtitle),
            (false, None) => format!(" · {}", self.view.subtitle),
            (true, Some(y)) => format!(" · {y}"),
            (true, None) => String::new(),
        };
        let title_line = format!("{}{subtitle}", self.view.title);
        draw_header(
            buf,
            area,
            area.y,
            &title_line,
            "enter: play from here, esc: close",
            shade(DIM),
            shade(NEAR_BLACK),
        );

        let (art_rect, list_area) = crate::browse::layout(area, self.view.art.is_some());
        let y = list_area.y;
        if let (Some(rect), ArtMode::Ascii) = (art_rect, self.art_mode) {
            AsciiArt {
                image: self.view.art.as_ref(),
            }
            .render(rect, buf);
        }

        if self.view.tracks.is_empty() {
            draw_line(buf, list_area, y, "(no tracks)", shade(DIM));
            return;
        }

        // same clamp-to-selection scroll as ResultsList: a long album
        // shouldn't be unreachable past the bottom of the pane.
        let visible_rows = list_area.height as usize;
        let scroll_offset = if self.view.selected >= visible_rows {
            self.view.selected + 1 - visible_rows
        } else {
            0
        };

        for row in 0..visible_rows {
            let idx = scroll_offset + row;
            let Some(track) = self.view.tracks.get(idx) else {
                break;
            };
            let is_selected = idx == self.view.selected;
            let marker = if is_selected { "> " } else { "  " };
            let label = match track {
                SearchItem::Song { title, artist, .. } => format!("{title} · {artist}"),
                _ => String::new(),
            };
            let star = if is_starred(track, self.favorites, self.remote_starred) {
                "* "
            } else {
                ""
            };
            let line = format!("{marker}{:>2}. {star}{label}", idx + 1);
            draw_line(
                buf,
                list_area,
                y + row as u16,
                &truncate_with_ellipsis(&line, list_area.width as usize),
                if is_selected {
                    shade(BRIGHT)
                } else {
                    shade(DIM)
                },
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
