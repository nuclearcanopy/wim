use std::collections::HashSet;

use ratatui::buffer::Buffer;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::widgets::Widget;

use crate::browse::{ArtMode, BrowseView};
use crate::favorites::Favorites;
use crate::search::SearchItem;
use crate::theme::{shade, BRIGHT, DIM, NEAR_BLACK, NEAR_WHITE};
use crate::ui::albumart::AsciiArt;
use crate::ui::header::draw_header;
use crate::ui::searchbox::SearchBox;
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
    /// The terminal's real cell height:width ratio (see
    /// `main::cell_aspect`), threaded through to `browse::layout` and
    /// `AsciiArt` so both agree on what "square" actually means here.
    pub cell_aspect: f32,
    /// Blink phase for the song-filter box's cursor, same convention
    /// as the sidebar's own `SearchBox`; only matters while
    /// `view.filtering`.
    pub cursor_visible: bool,
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
        // a plain rule first, continuing the sidebar's own search-box
        // top border straight across (see main.rs's vdivider junction
        // at this exact row) rather than letting the title text share
        // that row and read as crammed against it; the title itself
        // sits one row lower, where that border used to be visually
        // adjacent to nothing.
        for col in 0..area.width {
            if let Some(cell) = buf.cell_mut((area.x + col, area.y)) {
                cell.set_char('─');
                cell.set_fg(shade(NEAR_BLACK));
            }
        }
        draw_header(
            buf,
            area,
            area.y + 1,
            &title_line,
            "enter: play from here, esc: close",
            shade(DIM),
            shade(NEAR_BLACK),
        );

        if let (Some(bio), Some(rect)) = (
            &self.view.bio,
            crate::browse::bio_rect(area, self.view.bio.is_some()),
        ) {
            for (i, line) in crate::browse::wrap_bio(bio, rect.width as usize, rect.height as usize)
                .into_iter()
                .enumerate()
            {
                draw_line(buf, rect, rect.y + i as u16, &line, shade(DIM));
            }
        }

        let has_art = self.view.art.is_some() && self.art_mode != ArtMode::Off;
        let (art_rect, list_area) =
            crate::browse::layout(area, has_art, self.view.bio.is_some(), self.cell_aspect);
        let y = list_area.y;
        if let (Some(rect), ArtMode::Ascii) = (art_rect, self.art_mode) {
            AsciiArt {
                image: self.view.art.as_ref(),
                cell_aspect: self.cell_aspect,
            }
            .render(rect, buf);
        }

        if self.view.is_artist {
            self.render_artist_columns(list_area, buf);
            return;
        }

        if self.view.tracks.is_empty() {
            draw_line(buf, list_area, y, "(no tracks)", shade(DIM));
            return;
        }
        draw_track_list(
            buf,
            list_area,
            &self.view.tracks,
            &(0..self.view.tracks.len()).collect::<Vec<_>>(),
            self.view.selected,
            true,
            self.favorites,
            self.remote_starred,
        );
    }
}

impl BrowseListView<'_> {
    /// ALBUMS (left) and a filterable SONGS column (right, its own
    /// search box above the list) side by side, instead of one flat
    /// list; left/right switches which column navigation applies to
    /// (`view.albums_focused`, see main.rs).
    fn render_artist_columns(&self, area: Rect, buf: &mut Buffer) {
        let cols = Layout::horizontal([Constraint::Percentage(35), Constraint::Min(0)]).split(area);
        let albums_rect = cols[0];
        let songs_col = cols[1];

        if self.view.albums.is_empty() {
            draw_line(buf, albums_rect, albums_rect.y, "(no albums)", shade(DIM));
        } else {
            draw_simple_list(
                buf,
                albums_rect,
                &self.view.albums,
                self.view.albums_selected,
                self.view.albums_focused,
            );
        }

        let song_rows =
            Layout::vertical([Constraint::Length(3), Constraint::Min(0)]).split(songs_col);
        SearchBox {
            query: &self.view.filter_query,
            editing: self.view.filtering,
            cursor_visible: self.cursor_visible,
        }
        .render(song_rows[0], buf);

        let songs_rect = song_rows[1];
        let visible = self.view.visible_track_indices();
        if visible.is_empty() {
            let msg = if self.view.filter_query.is_empty() {
                "(no tracks)"
            } else {
                "(no matches)"
            };
            draw_line(buf, songs_rect, songs_rect.y, msg, shade(DIM));
            return;
        }
        draw_track_list(
            buf,
            songs_rect,
            &self.view.tracks,
            &visible,
            self.view.selected,
            !self.view.albums_focused,
            self.favorites,
            self.remote_starred,
        );
    }
}

/// Plain name-only rows (albums have no per-row metadata worth
/// showing inline the way a song's artist does); `focused` controls
/// whether the selected row brightens, same "only one column glows
/// at a time" convention as the sidebar/workspace split.
fn draw_simple_list(
    buf: &mut Buffer,
    area: Rect,
    items: &[SearchItem],
    selected: usize,
    focused: bool,
) {
    for (row, item) in items.iter().enumerate() {
        let y = area.y + row as u16;
        if y >= area.y + area.height {
            break;
        }
        let is_selected = row == selected;
        let marker = if is_selected { "> " } else { "  " };
        let name = match item {
            SearchItem::Album { name, .. } => name.as_str(),
            _ => "",
        };
        let color = if is_selected && focused {
            shade(NEAR_WHITE)
        } else {
            shade(DIM)
        };
        draw_line(
            buf,
            area,
            y,
            &truncate_with_ellipsis(&format!("{marker}{name}"), area.width as usize),
            color,
        );
    }
}

/// Shared by the single-list (album/playlist) layout and the artist
/// profile's SONGS column; `indices` is which positions in `tracks`
/// to actually show, in order (either every track, or whatever
/// `visible_track_indices` filtered down to). `focused` is the same
/// "does this column currently glow" flag `draw_simple_list` uses.
#[allow(clippy::too_many_arguments)]
fn draw_track_list(
    buf: &mut Buffer,
    area: Rect,
    tracks: &[SearchItem],
    indices: &[usize],
    selected: usize,
    focused: bool,
    favorites: &Favorites,
    remote_starred: &HashSet<String>,
) {
    // same clamp-to-selection scroll as ResultsList: a long album
    // shouldn't be unreachable past the bottom of the pane.
    let selected_row = indices.iter().position(|&i| i == selected).unwrap_or(0);
    let visible_rows = area.height as usize;
    let scroll_offset = if selected_row >= visible_rows {
        selected_row + 1 - visible_rows
    } else {
        0
    };

    for row in 0..visible_rows {
        let Some(&idx) = indices.get(scroll_offset + row) else {
            break;
        };
        let track = &tracks[idx];
        let is_selected = idx == selected;
        let marker = if is_selected { "> " } else { "  " };
        let label = match track {
            SearchItem::Song { title, artist, .. } => format!("{title} · {artist}"),
            _ => String::new(),
        };
        let star = if is_starred(track, favorites, remote_starred) {
            "* "
        } else {
            ""
        };
        let line = format!("{marker}{:>2}. {star}{label}", idx + 1);
        draw_line(
            buf,
            area,
            area.y + row as u16,
            &truncate_with_ellipsis(&line, area.width as usize),
            if is_selected && focused {
                shade(BRIGHT)
            } else {
                shade(DIM)
            },
        );
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
