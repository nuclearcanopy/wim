use std::collections::HashSet;
use std::time::Duration;

use ratatui::buffer::Buffer;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::widgets::Widget;

use crate::anim;
use crate::browse::{ArtMode, BrowseView};
use crate::favorites::Favorites;
use crate::search::SearchItem;
use crate::theme::{shade, BRIGHT, DIM, NEAR_BLACK, NEAR_WHITE};
use crate::ui::albumart::CachedArt;
use crate::ui::header::draw_header;
use crate::ui::results_list::{draw_header_row, fill_row_bg};
use crate::ui::scroll;
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
    /// Whether the workspace pane holds focus. A row stays selected in
    /// a pane that doesn't; its background just holds a flat dimmer
    /// level instead of breathing (see `anim::selection_bg`).
    pub focused: bool,
    /// Time since app start; the one input to every animation, so the
    /// pulse runs at the same speed whatever the display's rate is.
    pub elapsed: Duration,
}

/// Row offsets inside this view's own header block, from `area.y`: a
/// plain rule, the title line, then the title's own rule (the latter
/// two are `draw_header`'s). `browse::HEADER_HEIGHT` reserves one
/// blank row after them.
const TOP_RULE_ROW: u16 = 0;
const TITLE_ROW: u16 = 1;
const TITLE_RULE_ROW: u16 = TITLE_ROW + 1;

/// Rows where this view draws a horizontal rule that actually reaches
/// `area`'s left edge, so the sidebar divider one column left of it
/// can join them (see `ui::junctions`). The artist profile's column
/// labels count only when no art panel pushed them right; a rule
/// starting mid-pane never touches the divider.
pub fn rule_rows(area: Rect, view: &BrowseView, art_mode: ArtMode, cell_aspect: f32) -> Vec<u16> {
    let mut rows = vec![area.y + TOP_RULE_ROW, area.y + TITLE_RULE_ROW];
    if view.is_artist {
        let has_art = view.art.is_some() && art_mode != ArtMode::Off;
        let (_, list_area) = crate::browse::layout(
            area,
            has_art,
            view.bio.is_some(),
            view.is_artist,
            cell_aspect,
        );
        if list_area.x == area.x {
            rows.push(list_area.y);
        }
    }
    rows.retain(|y| *y < area.y + area.height);
    rows
}

/// The artist profile's two columns, as one split both the render and
/// the host's hit map read; a second copy of these constraints is how
/// a click ends up a row off whatever it looks like it hit.
pub struct ArtistColumns {
    pub albums_label: Rect,
    pub albums: Rect,
    pub songs_label: Rect,
    pub filter: Rect,
    pub songs: Rect,
}

pub fn artist_columns(area: Rect) -> ArtistColumns {
    let cols = Layout::horizontal([Constraint::Percentage(35), Constraint::Min(0)]).split(area);
    let album_rows = Layout::vertical([Constraint::Length(1), Constraint::Min(0)]).split(cols[0]);
    // the blank row between the SONGS rule and the filter box is
    // load-bearing: the box's own top border is a second parallel
    // rule, and stacking the two reads as one doubled line.
    let song_rows = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(3),
        Constraint::Min(0),
    ])
    .split(cols[1]);
    ArtistColumns {
        albums_label: album_rows[0],
        albums: album_rows[1],
        songs_label: song_rows[0],
        filter: song_rows[2],
        songs: song_rows[3],
    }
}

/// Which lists a click in this pane can land on, with the item index
/// per visible row of each. An album or playlist has only `tracks`
/// (the whole list area); an artist profile has both of its columns.
/// Built from the same splits and the same `scroll::offset_for` the
/// render uses, so a click resolves to the row the user saw.
pub struct BrowseRows {
    pub albums: Option<(Rect, Vec<Option<usize>>)>,
    pub tracks: (Rect, Vec<Option<usize>>),
}

pub fn rows(area: Rect, view: &BrowseView, art_mode: ArtMode, cell_aspect: f32) -> BrowseRows {
    let has_art = view.art.is_some() && art_mode != ArtMode::Off;
    let (_, list_area) = crate::browse::layout(
        area,
        has_art,
        view.bio.is_some(),
        view.is_artist,
        cell_aspect,
    );
    if !view.is_artist {
        let all: Vec<usize> = (0..view.tracks.len()).collect();
        return BrowseRows {
            albums: None,
            tracks: (list_area, track_rows(list_area, &all, view.selected)),
        };
    }
    let c = artist_columns(list_area);
    let visible = view.visible_track_indices();
    BrowseRows {
        albums: Some((
            c.albums,
            simple_rows(c.albums, view.albums.len(), view.albums_selected),
        )),
        tracks: (c.songs, track_rows(c.songs, &visible, view.selected)),
    }
}

/// Which of `indices` each visible row of `area` shows, at the
/// clamp-to-selection offset `draw_track_list` draws from.
fn track_rows(area: Rect, indices: &[usize], selected: usize) -> Vec<Option<usize>> {
    let selected_row = indices.iter().position(|&i| i == selected).unwrap_or(0);
    let visible_rows = area.height as usize;
    let offset = scroll::offset_for(selected_row, visible_rows);
    (0..visible_rows)
        .map(|row| indices.get(offset + row).copied())
        .collect()
}

/// Same for the albums column, whose rows are its indices outright (no
/// filter sits between the two).
fn simple_rows(area: Rect, len: usize, selected: usize) -> Vec<Option<usize>> {
    let visible_rows = area.height as usize;
    let offset = scroll::offset_for(selected, visible_rows);
    (0..visible_rows)
        .map(|row| (offset + row < len).then_some(offset + row))
        .collect()
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
        // while fetches are still in flight the header says so, so a
        // short list is never mistaken for the whole album.
        let still_loading = if self.view.loading {
            " · loading..."
        } else {
            ""
        };
        let title_line = format!("{}{subtitle}{still_loading}", self.view.title);
        // exactly one rule above the title and one below it: this
        // first one continues the sidebar's search-box top border
        // straight across (`ui::junctions` puts the ┬ there),
        // draw_header draws the second under the title itself.
        for col in 0..area.width {
            if let Some(cell) = buf.cell_mut((area.x + col, area.y + TOP_RULE_ROW)) {
                cell.set_char('─');
                cell.set_fg(shade(NEAR_BLACK));
            }
        }
        draw_header(
            buf,
            area,
            area.y + TITLE_ROW,
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
        let (art_rect, list_area) = crate::browse::layout(
            area,
            has_art,
            self.view.bio.is_some(),
            self.view.is_artist,
            self.cell_aspect,
        );
        let y = list_area.y;
        if let (Some(rect), ArtMode::Ascii) = (art_rect, self.art_mode) {
            CachedArt {
                image: self.view.art.as_ref(),
                cell_aspect: self.cell_aspect,
                key: &self.view.art_key(),
            }
            .render(rect, buf);
        }

        if self.view.is_artist {
            self.render_artist_columns(list_area, buf);
            return;
        }

        if self.view.tracks.is_empty() {
            let msg = crate::browse::pane_message(self.view.loading, false, "tracks");
            draw_line(buf, list_area, y, &msg, shade(DIM));
            return;
        }
        draw_track_list(
            buf,
            list_area,
            &self.view.tracks,
            &(0..self.view.tracks.len()).collect::<Vec<_>>(),
            self.view.selected,
            true,
            self.focused,
            self.elapsed,
            self.view.show_track_artist,
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
        let c = artist_columns(area);
        draw_header_row(buf, c.albums_label, c.albums_label.y, "ALBUMS");
        let albums_rect = c.albums;

        if self.view.albums.is_empty() {
            let msg = crate::browse::pane_message(self.view.loading, false, "albums");
            draw_line(buf, albums_rect, albums_rect.y, &msg, shade(DIM));
        } else {
            draw_simple_list(
                buf,
                albums_rect,
                &self.view.albums,
                self.view.albums_selected,
                self.view.albums_focused,
                self.focused,
                self.elapsed,
            );
        }

        draw_header_row(buf, c.songs_label, c.songs_label.y, "SONGS");
        SearchBox {
            query: &self.view.filter_query,
            editing: self.view.filtering,
            label: "filter songs",
            cursor_visible: self.cursor_visible,
            // nothing to the right of this one but the terminal edge,
            // so it closes itself rather than trailing off.
            right_border: true,
        }
        .render(c.filter, buf);

        let songs_rect = c.songs;
        let visible = self.view.visible_track_indices();
        if visible.is_empty() {
            let msg = crate::browse::pane_message(
                self.view.loading,
                !self.view.filter_query.is_empty(),
                "tracks",
            );
            draw_line(buf, songs_rect, songs_rect.y, &msg, shade(DIM));
            return;
        }
        draw_track_list(
            buf,
            songs_rect,
            &self.view.tracks,
            &visible,
            self.view.selected,
            !self.view.albums_focused,
            self.focused,
            self.elapsed,
            self.view.show_track_artist,
            self.favorites,
            self.remote_starred,
        );
    }
}

/// Plain name-only rows (albums have no per-row metadata worth
/// showing inline the way a song's artist does); `focused` controls
/// whether the selected row brightens, same "only one column glows
/// at a time" convention as the sidebar/workspace split, and
/// `pane_focused` whether its background breathes or sits flat.
fn draw_simple_list(
    buf: &mut Buffer,
    area: Rect,
    items: &[SearchItem],
    selected: usize,
    focused: bool,
    pane_focused: bool,
    elapsed: Duration,
) {
    // same clamp-to-selection scroll as the track list beside it; it
    // had none at all, so past the pane height `j` moved a cursor
    // nothing on screen ever showed.
    for (row, idx) in simple_rows(area, items.len(), selected)
        .into_iter()
        .enumerate()
    {
        let Some(item) = idx.and_then(|i| items.get(i)) else {
            break;
        };
        let y = area.y + row as u16;
        let is_selected = idx == Some(selected);
        if is_selected {
            fill_row_bg(
                buf,
                area,
                y,
                anim::selection_bg(elapsed, focused && pane_focused),
            );
        }
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
/// `visible_track_indices` filtered down to). `focused`/`pane_focused`
/// are the same two flags `draw_simple_list` uses.
#[allow(clippy::too_many_arguments)]
fn draw_track_list(
    buf: &mut Buffer,
    area: Rect,
    tracks: &[SearchItem],
    indices: &[usize],
    selected: usize,
    focused: bool,
    pane_focused: bool,
    elapsed: Duration,
    show_artist: bool,
    favorites: &Favorites,
    remote_starred: &HashSet<String>,
) {
    // same clamp-to-selection scroll as ResultsList: a long album
    // shouldn't be unreachable past the bottom of the pane.
    for (row, idx) in track_rows(area, indices, selected).into_iter().enumerate() {
        let Some(idx) = idx else {
            break;
        };
        let track = &tracks[idx];
        let is_selected = idx == selected;
        if is_selected {
            let level = anim::selection_bg(elapsed, focused && pane_focused);
            fill_row_bg(buf, area, area.y + row as u16, level);
        }
        let marker = if is_selected { "> " } else { "  " };
        let label = match track {
            SearchItem::Song { title, artist, .. } if show_artist => format!("{title} · {artist}"),
            SearchItem::Song { title, .. } => title.clone(),
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
    crate::ui::draw::line(buf, area, y, text, color);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::theme::NEAR_BLACK;
    use crate::ui::divider::VerticalRule;
    use crate::ui::junctions::SidebarDivider;
    use crate::ui::testbuf::{buffer, grid};

    fn view(title: &str, is_artist: bool) -> BrowseView {
        BrowseView {
            title: title.to_string(),
            subtitle: String::new(),
            year: None,
            tracks: Vec::new(),
            selected: 0,
            art: None,
            bio: None,
            is_artist,
            albums: Vec::new(),
            albums_selected: 0,
            albums_focused: true,
            filter_query: String::new(),
            filtering: false,
            show_track_artist: false,
            loading: false,
        }
    }

    fn render(area: Rect, view: &BrowseView, buf: &mut Buffer) {
        BrowseListView {
            view,
            favorites: &Favorites::default(),
            remote_starred: &HashSet::new(),
            art_mode: ArtMode::Off,
            cell_aspect: 2.0,
            cursor_visible: false,
            focused: true,
            elapsed: Duration::ZERO,
        }
        .render(area, buf);
    }

    /// One rule above the title and one below it, nothing stacked.
    #[test]
    fn header_is_rule_title_rule() {
        let area = Rect::new(0, 0, 24, 6);
        let mut buf = buffer(24, 6);
        render(area, &view("Bladee", false), &mut buf);
        assert_eq!(
            grid(&buf),
            [
                "────────────────────────",
                "Bladee │ enter: play fr…",
                "───────┴────────────────",
                "........................",
                "(no tracks).............",
                "........................",
            ]
            .join("\n")
        );
    }

    /// An open that has not answered yet says so in both places: the
    /// header, and the row where the tracks will go.
    #[test]
    fn a_still_loading_album_says_so_instead_of_reading_as_empty() {
        let area = Rect::new(0, 0, 30, 6);
        let mut buf = buffer(30, 6);
        let mut loading = view("Bladee", false);
        loading.loading = true;
        render(area, &loading, &mut buf);
        assert_eq!(
            grid(&buf),
            [
                "──────────────────────────────",
                "Bladee · loading... │ enter: …",
                "────────────────────┴─────────",
                "..............................",
                "loading tracks................",
                "..............................",
            ]
            .join("\n")
        );
    }

    #[test]
    fn rule_rows_are_the_two_header_rules_for_an_album() {
        let area = Rect::new(10, 0, 24, 10);
        let rows = rule_rows(area, &view("Bladee", false), ArtMode::Off, 2.0);
        assert_eq!(rows, vec![0, 2]);
    }

    /// The artist profile's column labels are a third rule at the
    /// left edge, so the divider has to join that row too.
    #[test]
    fn rule_rows_include_the_artist_column_labels() {
        let area = Rect::new(10, 0, 24, 10);
        let rows = rule_rows(area, &view("Bladee", true), ArtMode::Off, 2.0);
        assert_eq!(rows, vec![0, 2, 4]);
    }

    #[test]
    fn rule_rows_outside_a_short_area_are_dropped() {
        let area = Rect::new(10, 0, 24, 2);
        let rows = rule_rows(area, &view("Bladee", true), ArtMode::Off, 2.0);
        assert_eq!(rows, vec![0]);
    }

    /// The artist page as actually laid out next to the sidebar: the
    /// divider joins every rule that reaches it (┬ then ┼ then ├), the
    /// song filter box closes its own right edge, and a blank row
    /// keeps the SONGS rule off that box's top border. '.' is a cell
    /// nothing drew into.
    #[test]
    fn artist_page_next_to_the_sidebar_renders_connected() {
        let search = Rect::new(0, 0, 16, 3);
        let main = Rect::new(17, 0, 40, 12);
        let bv = view("Bladee", true);
        let mut buf = buffer(57, 12);

        SearchBox {
            query: "",
            editing: false,
            label: "search",
            cursor_visible: false,
            right_border: false,
        }
        .render(search, &mut buf);
        render(main, &bv, &mut buf);
        VerticalRule {
            highlighted: false,
            junction_rows: SidebarDivider {
                search,
                results_header_rows: &[],
                main_content_rule_rows: &rule_rows(main, &bv, ArtMode::Off, 2.0),
                bottom_rule_row: 12,
            }
            .junctions(shade(NEAR_BLACK)),
        }
        .render(Rect::new(16, 0, 1, 12), &mut buf);

        assert_eq!(
            grid(&buf),
            [
                "┌┤ search ├─────┬────────────────────────────────────────",
                "│...............│Bladee │ enter: play from here, esc: cl…",
                "└───────────────┼───────┴────────────────────────────────",
                "................│........................................",
                "................├─── ALBUMS ──────────── SONGS ──────────",
                "................│(no albums).............................",
                "................│..............┌┤ filter songs ├────────┐",
                "................│..............│........................│",
                "................│..............└────────────────────────┘",
                "................│..............(no tracks)...............",
                "................│........................................",
                "................│........................................",
            ]
            .join("\n")
        );
    }

    use crate::browse::tests::{test_client, test_songs, test_view};

    fn row_text(buf: &Buffer, y: u16) -> String {
        (0..buf.area.width)
            .map(|x| {
                buf.cell((x, y))
                    .map_or(' ', |c| c.symbol().chars().next().unwrap_or(' '))
            })
            .collect()
    }

    fn render_view(view: &BrowseView, area: Rect) -> Buffer {
        render_view_at(view, area, true, Duration::ZERO)
    }

    fn render_view_at(view: &BrowseView, area: Rect, focused: bool, elapsed: Duration) -> Buffer {
        let mut buf = Buffer::empty(area);
        let favorites = Favorites::default();
        let remote_starred = HashSet::new();
        BrowseListView {
            view,
            favorites: &favorites,
            remote_starred: &remote_starred,
            art_mode: ArtMode::Off,
            cell_aspect: 2.0,
            cursor_visible: false,
            focused,
            elapsed,
        }
        .render(area, &mut buf);
        buf
    }

    fn test_albums(n: usize) -> Vec<SearchItem> {
        let client = test_client();
        (0..n)
            .map(|i| SearchItem::Album {
                id: format!("album-{i}"),
                name: format!("album {i}"),
                artist: "artist".to_string(),
                client: client.clone(),
            })
            .collect()
    }

    /// The row map a click resolves against has to name the rows the
    /// render painted, in both columns and at a scroll offset; the
    /// songs column also has a filter between its rows and its item
    /// indices, which is exactly where an independent derivation goes
    /// wrong.
    #[test]
    fn the_recorded_row_map_matches_the_rows_that_were_drawn() {
        let mut view = test_view(test_songs(20));
        view.is_artist = true;
        view.albums = test_albums(20);
        view.albums_selected = 15;
        view.selected = 17;

        let area = Rect::new(0, 0, 70, 16);
        let buf = render_view(&view, area);
        let map = rows(area, &view, ArtMode::Off, 2.0);

        let (albums_rect, album_rows) = map.albums.expect("an artist profile has an albums column");
        assert!(album_rows.contains(&Some(15)), "{album_rows:?}");
        for (row, idx) in album_rows.iter().enumerate() {
            let text = row_text(&buf, albums_rect.y + row as u16);
            match idx {
                Some(i) => assert!(
                    text.contains(&format!("album {i}")),
                    "albums row {row} maps to {i} but reads {text:?}"
                ),
                None => assert!(
                    !text.contains("album "),
                    "albums row {row} maps to nothing but reads {text:?}"
                ),
            }
        }

        let (songs_rect, song_rows) = map.tracks;
        assert!(song_rows.contains(&Some(17)), "{song_rows:?}");
        for (row, idx) in song_rows.iter().enumerate() {
            let text = row_text(&buf, songs_rect.y + row as u16);
            match idx {
                // the row prints the track's own 1-based position,
                // which is what a click has to come back with.
                Some(i) => assert!(
                    text.contains(&format!("{:>2}. ", i + 1)),
                    "songs row {row} maps to {i} but reads {text:?}"
                ),
                None => assert!(
                    text.trim().is_empty(),
                    "songs row {row} maps to nothing but reads {text:?}"
                ),
            }
        }
    }

    /// A filtered songs column's rows are its visible indices, not
    /// `row + offset`: the map has to carry the filter's mapping.
    #[test]
    fn a_filtered_songs_column_maps_rows_to_the_tracks_own_indices() {
        let mut view = test_view(test_songs(20));
        view.is_artist = true;
        view.filter_query = "7".to_string();
        view.clamp_selection();

        let area = Rect::new(0, 0, 70, 16);
        let visible = view.visible_track_indices();
        assert!(visible.len() > 1 && visible.len() < 20, "{visible:?}");
        let (_, song_rows) = rows(area, &view, ArtMode::Off, 2.0).tracks;
        let mapped: Vec<usize> = song_rows.iter().flatten().copied().collect();
        assert_eq!(mapped, visible);
    }

    /// An album has one full-width list and no albums column at all,
    /// so a click in the left third of the pane is a track.
    #[test]
    fn an_album_has_one_list_and_no_second_column() {
        let view = test_view(test_songs(4));
        let area = Rect::new(0, 0, 70, 16);
        let map = rows(area, &view, ArtMode::Off, 2.0);
        assert!(map.albums.is_none());
        let (rect, song_rows) = map.tracks;
        assert_eq!(rect.x, area.x);
        assert_eq!(rect.width, area.width);
        assert_eq!(song_rows.iter().flatten().count(), 4);
    }

    #[test]
    fn a_pane_with_no_room_has_no_rows_to_click() {
        let view = test_view(test_songs(4));
        for area in [
            Rect::new(0, 0, 70, 0),
            Rect::new(0, 0, 0, 16),
            Rect::new(0, 0, 70, 5),
        ] {
            let map = rows(area, &view, ArtMode::Off, 2.0);
            assert!(map.tracks.1.iter().flatten().count() <= 4, "{area:?}");
        }
    }

    #[test]
    fn the_albums_column_scrolls_to_keep_the_selection_on_screen() {
        let mut view = test_view(test_songs(3));
        view.is_artist = true;
        view.albums = test_albums(20);
        view.albums_selected = 15;
        view.albums_focused = true;

        let area = Rect::new(0, 0, 60, 14);
        let buf = render_view(&view, area);
        let screen: String = (0..area.height)
            .map(|y| row_text(&buf, y))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            screen.contains("> album 15"),
            "a selection past the pane height must scroll into view:\n{screen}"
        );
    }

    #[test]
    fn the_track_cursor_lands_on_the_row_enter_would_act_on() {
        let mut view = test_view(test_songs(5));
        view.selected = 4;
        // what a reloaded/shortened listing does; the cursor has to
        // follow `selected`, not stay on a row that no longer exists.
        view.set_tracks(test_songs(3));

        let area = Rect::new(0, 0, 40, 12);
        let buf = render_view(&view, area);
        let marked: Vec<String> = (0..area.height)
            .map(|y| row_text(&buf, y))
            .filter(|row| row.starts_with('>'))
            .collect();
        assert_eq!(marked.len(), 1, "exactly one row carries the cursor");
        assert!(
            marked[0].contains("song 2"),
            "cursor must sit on the selected track: {:?}",
            marked[0]
        );
    }

    fn row_bgs(buf: &Buffer, y: u16) -> Vec<ratatui::style::Color> {
        (0..buf.area.width)
            .map(|x| buf.cell((x, y)).unwrap().bg)
            .collect()
    }

    fn cursor_row(buf: &Buffer) -> u16 {
        (0..buf.area.height)
            .find(|y| row_text(buf, *y).starts_with('>'))
            .expect("some row carries the cursor")
    }

    /// Every cell holding a `>` marker, i.e. one per list on screen.
    fn cursor_cells(buf: &Buffer) -> Vec<(u16, u16)> {
        (0..buf.area.height)
            .flat_map(|y| (0..buf.area.width).map(move |x| (x, y)))
            .filter(|(x, y)| buf.cell((*x, *y)).unwrap().symbol() == ">")
            .collect()
    }

    #[test]
    fn the_selected_track_row_carries_a_background_across_its_full_width() {
        let view = test_view(test_songs(3));
        let area = Rect::new(0, 0, 40, 12);
        let buf = render_view_at(&view, area, true, Duration::ZERO);
        let y = cursor_row(&buf);
        let expected = shade(anim::selection_bg(Duration::ZERO, true));
        assert_eq!(row_bgs(&buf, y), vec![expected; 40]);
        for other in [y + 1, y + 2] {
            assert_eq!(
                row_bgs(&buf, other),
                vec![ratatui::style::Color::Reset; 40],
                "row {other} must not be highlighted"
            );
        }
    }

    #[test]
    fn the_focused_panes_track_selection_pulses_over_time() {
        let view = test_view(test_songs(3));
        let area = Rect::new(0, 0, 40, 12);
        let trough = render_view_at(&view, area, true, Duration::ZERO);
        let peak = render_view_at(&view, area, true, anim::SELECTION_PULSE_PERIOD / 2);
        let y = cursor_row(&trough);
        assert_ne!(
            row_bgs(&trough, y),
            row_bgs(&peak, y),
            "the background has to move, not sit on one level"
        );
    }

    /// The track stays selected while the sidebar has focus; its row
    /// just holds a flat dimmer background instead of breathing.
    #[test]
    fn an_unfocused_panes_track_selection_does_not_pulse() {
        let view = test_view(test_songs(3));
        let area = Rect::new(0, 0, 40, 12);
        let flat = shade(anim::SELECTION_BG_UNFOCUSED);
        for t in [Duration::ZERO, anim::SELECTION_PULSE_PERIOD / 2] {
            let buf = render_view_at(&view, area, false, t);
            let y = cursor_row(&buf);
            assert_eq!(row_bgs(&buf, y), vec![flat; 40]);
        }
    }

    /// Both of an artist page's columns keep a selected row, but only
    /// the one navigation is currently acting on pulses.
    #[test]
    fn only_the_focused_column_of_an_artist_page_pulses() {
        let mut view = test_view(test_songs(3));
        view.is_artist = true;
        view.albums = test_albums(3);
        view.albums_focused = true;

        let area = Rect::new(0, 0, 60, 14);
        let at = anim::SELECTION_PULSE_PERIOD / 2;
        let buf = render_view_at(&view, area, true, at);
        let cells = cursor_cells(&buf);
        assert_eq!(cells.len(), 2, "one cursor per column: {cells:?}");
        let bg = |(x, y): (u16, u16)| buf.cell((x, y)).unwrap().bg;
        // the ALBUMS column's cursor sits four rows above the SONGS
        // column's, so it comes first in row order.
        assert_eq!(bg(cells[0]), shade(anim::selection_bg(at, true)));
        assert_eq!(bg(cells[1]), shade(anim::SELECTION_BG_UNFOCUSED));
    }
}
