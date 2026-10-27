use std::collections::HashSet;

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::widgets::Widget;

use crate::favorites::Favorites;
use crate::search::{group_boundaries, Category, SearchItem};
use crate::theme::{shade, BRIGHT, DIM, NEAR_BLACK};
use crate::ui::textfit::truncate_with_ellipsis;

/// One rendered row: either a section header or an actual result,
/// carrying its index into the original `items` slice so the caller
/// can map back to it (`self.selected` stays a plain index into
/// `items`; headers never get one).
pub enum DisplayRow {
    Header(Category),
    Item(usize),
}

/// Flattens `items` (already grouped into category runs by
/// `Corpus::search`) into a display-row sequence with a header before
/// each run, plus the scroll offset keeping `selected` visible within
/// `visible_rows`. The one place both this widget's own render and
/// main.rs's sidebar-divider header-junction marks compute this, so
/// they can't disagree about where a header actually lands.
pub fn layout(
    items: &[SearchItem],
    selected: usize,
    visible_rows: usize,
) -> (Vec<DisplayRow>, usize) {
    let boundaries = group_boundaries(items);
    let mut rows = Vec::with_capacity(items.len() + boundaries.len());
    let mut next_boundary = boundaries.into_iter().peekable();
    for i in 0..items.len() {
        if let Some((idx, cat)) = next_boundary.peek() {
            if *idx == i {
                rows.push(DisplayRow::Header(*cat));
                next_boundary.next();
            }
        }
        rows.push(DisplayRow::Item(i));
    }
    let selected_row = rows
        .iter()
        .position(|r| matches!(r, DisplayRow::Item(i) if *i == selected))
        .unwrap_or(0);
    (
        rows,
        crate::ui::scroll::offset_for(selected_row, visible_rows),
    )
}

/// Plain ASCII, not unicode pictographs: even nominally "geometric,
/// not emoji" symbols (e.g. the bullseye tried earlier for albums)
/// can still be flagged emoji-presentation in some fonts/terminals, or
/// render at ambiguous width and throw off alignment. A letter in
/// brackets can't do either.
pub(crate) fn kind_code(item: &SearchItem) -> char {
    match item {
        SearchItem::LocalMusic { .. } | SearchItem::Song { .. } => 's',
        SearchItem::LocalVideo { .. } => 'v',
        SearchItem::Artist { .. } => 'a',
        SearchItem::Album { .. } => 'l',
        SearchItem::Playlist { .. } => 'p',
    }
}

/// 'n' for navidrome (remote), 'f' for file (local).
pub(crate) fn source_code(item: &SearchItem) -> char {
    match item {
        SearchItem::LocalMusic { .. } | SearchItem::LocalVideo { .. } => 'f',
        SearchItem::Artist { .. }
        | SearchItem::Album { .. }
        | SearchItem::Song { .. }
        | SearchItem::Playlist { .. } => 'n',
    }
}

pub(crate) fn label(item: &SearchItem) -> String {
    // "·" (middle dot), not an em dash, for title/artist metadata
    // separators; same convention most music apps use.
    match item {
        SearchItem::LocalMusic { title, artist, .. } => match artist {
            Some(a) => format!("{title} · {a}"),
            None => title.clone(),
        },
        SearchItem::LocalVideo { title, .. } => title.clone(),
        SearchItem::Artist { name, .. } => name.clone(),
        SearchItem::Album { name, artist, .. } => format!("{name} · {artist}"),
        SearchItem::Song {
            title,
            artist,
            album,
            ..
        } => format!("{title} · {artist} ({album})"),
        SearchItem::Playlist { name, .. } => name.clone(),
    }
}

/// Same as `label`, but the artist/album metadata only shows on the
/// hovered (selected) row; everywhere else just the bare title, so a
/// long results list doesn't read as a wall of repeated artist names.
fn row_label(item: &SearchItem, hovered: bool) -> String {
    if hovered {
        return label(item);
    }
    match item {
        SearchItem::LocalMusic { title, .. } => title.clone(),
        SearchItem::LocalVideo { title, .. } => title.clone(),
        SearchItem::Artist { name, .. } => name.clone(),
        SearchItem::Album { name, .. } => name.clone(),
        SearchItem::Song { title, .. } => title.clone(),
        SearchItem::Playlist { name, .. } => name.clone(),
    }
}

/// Only tracks are starrable (see `main.rs::is_starred`); this is a
/// display-only mirror of that same check, since the widget only gets
/// the raw favorites/remote-starred state, not the logic that owns it.
fn is_starred(item: &SearchItem, favorites: &Favorites, remote_starred: &HashSet<String>) -> bool {
    match item {
        SearchItem::LocalMusic { path, .. } | SearchItem::LocalVideo { path, .. } => {
            favorites.is_starred(&path.to_string_lossy())
        }
        SearchItem::Song { id, .. } => remote_starred.contains(id),
        SearchItem::Artist { .. } | SearchItem::Album { .. } | SearchItem::Playlist { .. } => false,
    }
}

/// Custom-painted results list: no stock `List` widget. Each row is
/// `[kind] label`, the selected row rendered near-white, others dim, and
/// the window scrolls to keep the selection visible without wrapping.
/// An empty `items` isn't a no-op render; a blank screen with no
/// explanation is a dead end, not a loved-app experience; `empty_message`
/// is shown instead so there's always *something* telling the user why
/// nothing's there and what to do about it.
pub struct ResultsList<'a> {
    pub items: &'a [SearchItem],
    pub selected: usize,
    pub empty_message: &'a str,
    pub favorites: &'a Favorites,
    pub remote_starred: &'a HashSet<String>,
}

impl Widget for ResultsList<'_> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        if area.height == 0 {
            return;
        }

        if self.items.is_empty() {
            let text = truncate_with_ellipsis(self.empty_message, area.width as usize);
            for (col, ch) in text.chars().enumerate() {
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
        let (rows, scroll_offset) = layout(self.items, self.selected, visible_rows);

        for row in 0..visible_rows {
            let Some(display_row) = rows.get(scroll_offset + row) else {
                break;
            };
            let y = area.y + row as u16;

            match display_row {
                DisplayRow::Header(cat) => draw_header_row(buf, area, y, cat.label()),
                DisplayRow::Item(idx) => {
                    let item = &self.items[*idx];
                    let is_selected = *idx == self.selected;

                    let fg = if is_selected {
                        shade(BRIGHT)
                    } else {
                        shade(DIM)
                    };
                    let marker = if is_selected { "> " } else { "  " };
                    let star = if is_starred(item, self.favorites, self.remote_starred) {
                        "* "
                    } else {
                        ""
                    };
                    let full = format!(
                        "{marker}[{}{}] {star}{}",
                        source_code(item),
                        kind_code(item),
                        row_label(item, is_selected)
                    );
                    let text = truncate_with_ellipsis(&full, area.width as usize);

                    for (col, ch) in text.chars().enumerate() {
                        let x = area.x + col as u16;
                        if x >= area.x + area.width {
                            break;
                        }
                        if let Some(cell) = buf.cell_mut((x, y)) {
                            cell.set_char(ch);
                            cell.set_fg(fg);
                        }
                    }
                }
            }
        }
    }
}

/// A full-width dim rule with the section name centered, rather than
/// a plain text line: it's what actually reads as "a divider between
/// groups" instead of just another row, and fills the whole width so
/// it visually meets the sidebar's own vertical divider immediately
/// to its right (see main.rs's matching junction mark at this same
/// row).
pub(crate) fn draw_header_row(buf: &mut Buffer, area: Rect, y: u16, label: &str) {
    let rule_color = shade(NEAR_BLACK);
    for col in 0..area.width {
        if let Some(cell) = buf.cell_mut((area.x + col, y)) {
            cell.set_char('─');
            cell.set_fg(rule_color);
        }
    }
    let text = format!(" {label} ");
    let text_width = text.chars().count() as u16;
    let start_x = area.x + area.width.saturating_sub(text_width) / 2;
    for (i, ch) in text.chars().enumerate() {
        let x = start_x + i as u16;
        if x >= area.x + area.width {
            break;
        }
        if let Some(cell) = buf.cell_mut((x, y)) {
            cell.set_char(ch);
            cell.set_fg(shade(DIM));
        }
    }
}
