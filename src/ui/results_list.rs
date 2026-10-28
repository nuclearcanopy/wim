use std::collections::HashSet;
use std::time::Duration;

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::widgets::Widget;

use crate::anim;
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

/// Absolute rows of `area` on which `render` draws a section-header
/// rule, which is what the divider immediately right of this list
/// needs to join them (see `ui::junctions`); derived from the same
/// `layout` call the render itself uses, so the two can't disagree
/// about where a header actually lands.
pub fn header_rows(items: &[SearchItem], selected: usize, area: Rect) -> Vec<u16> {
    if items.is_empty() || area.height == 0 {
        return Vec::new();
    }
    let visible_rows = area.height as usize;
    let (rows, scroll_offset) = layout(items, selected, visible_rows);
    (0..visible_rows)
        .filter(|row| matches!(rows.get(scroll_offset + row), Some(DisplayRow::Header(_))))
        .map(|row| area.y + row as u16)
        .collect()
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
    /// Whether the sidebar itself currently holds focus: the selected
    /// row's background only breathes in the focused pane, and sits at
    /// a flat dimmer level otherwise (see `anim::selection_bg`).
    pub focused: bool,
    /// Time since app start; the one input to every animation, so the
    /// pulse runs at the same speed whatever the display's rate is.
    pub elapsed: Duration,
}

impl Widget for ResultsList<'_> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        if area.height == 0 {
            return;
        }

        if self.items.is_empty() {
            let text = truncate_with_ellipsis(self.empty_message, area.width as usize);
            crate::ui::draw::line(buf, area, area.y, &text, shade(DIM));
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

                    // the selection reads off the row's background,
                    // not its borders; the marker and brightness bump
                    // below stay on top of it.
                    if is_selected {
                        fill_row_bg(buf, area, y, anim::selection_bg(self.elapsed, self.focused));
                    }

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

                    crate::ui::draw::line(buf, area, y, &text, fg);
                }
            }
        }
    }
}

/// Paints one row of `area` a flat background level across its whole
/// width, so a selection highlight is a full bar rather than ending
/// wherever that row's label happens to stop. Shared with
/// `ui::browse`'s lists, same as `draw_header_row` below.
pub(crate) fn fill_row_bg(buf: &mut Buffer, area: Rect, y: u16, level: f32) {
    if y >= area.y + area.height {
        return;
    }
    let bg = shade(level);
    for col in 0..area.width {
        if let Some(cell) = buf.cell_mut((area.x + col, y)) {
            cell.set_bg(bg);
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
    let row = Rect {
        y,
        height: 1,
        ..area
    };
    crate::ui::draw::centered(buf, row, y, &text, shade(DIM));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::testbuf::buffer;
    use ratatui::style::Color;

    fn items(n: usize) -> Vec<SearchItem> {
        (0..n)
            .map(|i| SearchItem::LocalMusic {
                path: format!("/m/{i}.flac").into(),
                title: format!("track {i}"),
                artist: None,
                album: None,
            })
            .collect()
    }

    fn render(selected: usize, focused: bool, elapsed: Duration) -> Buffer {
        let area = Rect::new(0, 0, 20, 5);
        let mut buf = buffer(20, 5);
        ResultsList {
            items: &items(3),
            selected,
            empty_message: "",
            favorites: &Favorites::default(),
            remote_starred: &HashSet::new(),
            focused,
            elapsed,
        }
        .render(area, &mut buf);
        buf
    }

    fn row_bgs(buf: &Buffer, y: u16) -> Vec<Color> {
        (0..buf.area.width)
            .map(|x| buf.cell((x, y)).unwrap().bg)
            .collect()
    }

    /// Row 0 is the category header `layout` inserts, so the items
    /// start at row 1.
    const FIRST_ITEM_ROW: u16 = 1;

    #[test]
    fn the_selected_row_carries_a_background_across_its_full_width() {
        let buf = render(0, true, Duration::ZERO);
        let expected = shade(anim::selection_bg(Duration::ZERO, true));
        assert_eq!(row_bgs(&buf, FIRST_ITEM_ROW), vec![expected; 20]);
    }

    #[test]
    fn unselected_rows_keep_the_default_background() {
        let buf = render(0, true, Duration::ZERO);
        for y in [FIRST_ITEM_ROW + 1, FIRST_ITEM_ROW + 2] {
            assert_eq!(
                row_bgs(&buf, y),
                vec![Color::Reset; 20],
                "row {y} must not be highlighted"
            );
        }
    }

    #[test]
    fn the_focused_panes_selection_pulses_over_time() {
        let trough = render(0, true, Duration::ZERO);
        let peak = render(0, true, anim::SELECTION_PULSE_PERIOD / 2);
        assert_ne!(
            row_bgs(&trough, FIRST_ITEM_ROW),
            row_bgs(&peak, FIRST_ITEM_ROW),
            "the background has to move, not sit on one level"
        );
    }

    /// A row stays selected in a pane that lost focus; it just holds
    /// a flat dimmer background instead of competing for attention.
    #[test]
    fn an_unfocused_panes_selection_does_not_pulse() {
        let flat = shade(anim::SELECTION_BG_UNFOCUSED);
        for t in [Duration::ZERO, anim::SELECTION_PULSE_PERIOD / 2] {
            let buf = render(0, false, t);
            assert_eq!(row_bgs(&buf, FIRST_ITEM_ROW), vec![flat; 20]);
        }
    }
}
