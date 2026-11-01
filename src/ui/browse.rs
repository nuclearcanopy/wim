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
use crate::ui::textfit::truncate_into;

/// Track listing for an opened album/playlist, in `main_content`
/// (not full-screen: the sidebar and bottom bar stay visible, this is
/// "the main workspace" getting overwritten, not a modal on top of it).
pub struct BrowseListView<'a> {
    pub view: &'a BrowseView,
    pub favorites: &'a Favorites,
    pub remote_starred: &'a HashSet<String>,
    /// In a `Full*` mode the art panel is left to main.rs, which
    /// overlays a `ratatui-image` stateful widget on the exact same
    /// rect `browse::layout` gives this widget; this just skips drawing
    /// the rasterized art there instead of drawing under it.
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
        // an album or playlist has no filter box, so the render shows
        // every track and so does the map.
        let shown = Shown::All(view.tracks.len());
        return BrowseRows {
            albums: None,
            tracks: (list_area, track_rows(list_area, shown, view.selected)),
        };
    }
    let filtered = filtered_tracks(view);
    let shown = Shown::new(view, &filtered);
    let c = artist_columns(list_area);
    BrowseRows {
        albums: Some((
            c.albums,
            simple_rows(c.albums, view.albums.len(), view.albums_selected).collect(),
        )),
        tracks: (c.songs, track_rows(c.songs, shown, view.selected)),
    }
}

/// Which positions in `view.tracks` a list shows, in order: either
/// every one of them or whatever the song filter narrowed them to.
///
/// `All` is both the album/playlist case and an artist page with an
/// empty filter box, and it is the one that used to cost: a `Vec<usize>`
/// as long as the whole listing, built twice a frame (the click map,
/// then the paint), with a `position(|i| i == selected)` walk over it
/// for the cursor's row. Both are arithmetic here; see `frame_budget`.
#[derive(Debug, Clone, Copy)]
enum Shown<'a> {
    All(usize),
    Filtered(&'a [usize]),
}

impl<'a> Shown<'a> {
    fn new(view: &BrowseView, filtered: &'a Option<Vec<usize>>) -> Self {
        match filtered {
            Some(indices) => Shown::Filtered(indices),
            None => Shown::All(view.tracks.len()),
        }
    }

    fn len(&self) -> usize {
        match self {
            Shown::All(len) => *len,
            Shown::Filtered(indices) => indices.len(),
        }
    }

    /// The track at display row `row`, or `None` past the end.
    fn get(&self, row: usize) -> Option<usize> {
        match self {
            Shown::All(len) => (row < *len).then_some(row),
            Shown::Filtered(indices) => indices.get(row).copied(),
        }
    }

    /// Which display row holds `selected`; row 0 when it shows none,
    /// the same answer the `position(..).unwrap_or(0)` this replaced
    /// gave by failing to find it.
    fn row_of(&self, selected: usize) -> usize {
        match self {
            Shown::All(len) => match selected < *len {
                true => selected,
                false => 0,
            },
            Shown::Filtered(indices) => indices.iter().position(|&i| i == selected).unwrap_or(0),
        }
    }
}

/// The filter's answer, materialized only when there is a filter:
/// `visible_track_indices`' own first branch is `(0..tracks.len())`
/// collected, which is the `Vec` `Shown::All` exists to not build.
fn filtered_tracks(view: &BrowseView) -> Option<Vec<usize>> {
    (!view.filter_query.is_empty()).then(|| view.visible_track_indices())
}

/// How a list area splits: rows the items get, and whether the last
/// one is the scroll footer.
///
/// The footer is only owed when the list does not fit, so a short one
/// loses no row to it (the rule `help`'s hint row follows). One
/// direction only, so it cannot oscillate: taking a row away can only
/// leave more hidden, never make it fit.
///
/// Byte for byte `ui::queue`'s own `body_split`. `ui::scroll` is where
/// the shared copy belongs: it already owns the one
/// clamp-to-selection rule every custom-painted list here uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Body {
    rows: usize,
    footer: bool,
}

/// A list this short keeps every row for its items: a footer saying
/// how much is hidden, over one visible row, is a column that reports
/// on itself instead of showing anything.
const FOOTER_FLOOR: usize = 3;

fn list_split(total: usize, height: usize) -> Body {
    if height < FOOTER_FLOOR || total <= height {
        return Body {
            rows: height,
            footer: false,
        };
    }
    Body {
        rows: height - 1,
        footer: true,
    }
}

/// Which track each visible row of `area` shows, at the
/// clamp-to-selection offset `draw_track_list` draws from; `None` on
/// the footer row, which is not a track and so is nothing to click.
fn track_rows(area: Rect, shown: Shown, selected: usize) -> Vec<Option<usize>> {
    let visible_rows = area.height as usize;
    let split = list_split(shown.len(), visible_rows);
    let offset = scroll::offset_for(shown.row_of(selected), split.rows);
    (0..visible_rows)
        .map(|row| match row < split.rows {
            true => shown.get(offset + row),
            false => None,
        })
        .collect()
}

/// Same for the albums column, whose rows are its indices outright (no
/// filter sits between the two); an iterator rather than a `Vec`, so
/// the paint next door walks it without allocating a row map it then
/// throws away. Still exactly `area.height` answers long, which is
/// what the hit map it feeds indexes by row.
fn simple_rows(area: Rect, len: usize, selected: usize) -> impl Iterator<Item = Option<usize>> {
    let visible_rows = area.height as usize;
    let split = list_split(len, visible_rows);
    let offset = scroll::offset_for(selected, split.rows);
    (0..visible_rows)
        .map(move |row| (row < split.rows && offset + row < len).then_some(offset + row))
}

fn is_starred(track: &SearchItem, favorites: &Favorites, remote_starred: &HashSet<String>) -> bool {
    match track {
        SearchItem::LocalMusic { path, .. } | SearchItem::LocalVideo { path, .. } => {
            favorites.is_starred(&path.to_string_lossy())
        }
        SearchItem::Song { id, .. } => remote_starred.contains(id),
        SearchItem::Silence { .. }
        | SearchItem::Artist { .. }
        | SearchItem::Album { .. }
        | SearchItem::Playlist { .. } => false,
        SearchItem::TorBoxFile { .. } => false,
        SearchItem::WatchSource { .. } => false,
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
        // painted only where `rule_rows` says a rule goes, rather than
        // repeating its bounds check: the reporting side already
        // dropped rows past a short pane and this side did not, so a
        // 0-row pane laid a full `─` across the bottom bar.
        let rules = rule_rows(area, self.view, self.art_mode, self.cell_aspect);
        if rules.contains(&(area.y + TOP_RULE_ROW)) {
            for col in 0..area.width {
                if let Some(cell) = buf.cell_mut((area.x + col, area.y + TOP_RULE_ROW)) {
                    cell.set_char('─');
                    cell.set_fg(shade(NEAR_BLACK));
                }
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
            // `bio_rect` is a fixed three rows four down, whatever the
            // pane's height; clipped to what the pane owns, a short
            // one drops bio rows instead of printing them over the
            // bottom bar.
            let rect = rect.intersection(area);
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
        if let Some(rect) = art_rect.filter(|_| self.art_mode.is_ascii()) {
            CachedArt {
                image: self.view.art.as_ref(),
                cell_aspect: self.cell_aspect,
                key: &self.view.art_key(),
                tone: self.art_mode.tone(),
            }
            .render(rect, buf);
        }

        if self.view.is_artist {
            self.render_artist_columns(list_area, buf);
            return;
        }

        if self.view.tracks.is_empty() {
            let msg = crate::browse::pane_message(self.view.loading, false, "tracks");
            draw_info(buf, list_area, y, &msg);
            return;
        }
        // an album or playlist has no filter box, so every track shows
        // and `Shown::All` answers its rows without a row list.
        draw_track_list(
            buf,
            list_area,
            &self.view.tracks,
            Shown::All(self.view.tracks.len()),
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
            draw_info(buf, albums_rect, albums_rect.y, &msg);
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
        let filtered = filtered_tracks(self.view);
        let shown = Shown::new(self.view, &filtered);
        if shown.len() == 0 {
            let msg = crate::browse::pane_message(
                self.view.loading,
                !self.view.filter_query.is_empty(),
                "tracks",
            );
            draw_info(buf, songs_rect, songs_rect.y, &msg);
            return;
        }
        draw_track_list(
            buf,
            songs_rect,
            &self.view.tracks,
            shown,
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
    // one pair of buffers for every row of the frame, where a
    // `format!` plus a `truncate` was two allocations per visible row.
    let mut text = String::new();
    let mut fitted = String::new();
    let names = Rect {
        width: area.width.saturating_sub(COLUMN_GUTTER),
        ..area
    };
    let split = list_split(items.len(), area.height as usize);
    let mut drawn = 0usize;
    for (row, idx) in simple_rows(area, items.len(), selected).enumerate() {
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
        let color = if is_selected && focused {
            shade(NEAR_WHITE)
        } else {
            shade(DIM)
        };
        write_album_text(&mut text, item, is_selected);
        draw_fitted(buf, names, y, &text, color, &mut fitted);
        drawn = row + 1;
    }
    // an album's row is its own index, so the cursor's row is the
    // selection itself; `simple_rows` above draws from the same offset.
    let offset = scroll::offset_for(selected, split.rows);
    scroll_feedback(
        buf,
        area,
        Rect {
            // one cell in from the right, so the air after the cue is
            // what meets the songs column.
            width: area.width.saturating_sub(COLUMN_GUTTER - 1),
            height: drawn as u16,
            ..area
        },
        split,
        offset,
        items.len(),
        &mut text,
        &mut fitted,
    );
}

/// Cells the albums column holds back off its right edge: one for its
/// `▴`/`▾` cue and one of air after it.
///
/// Both are owed because the songs column begins in the very next
/// cell. Without the cue column a full-width album name ran straight
/// into the song titles; without the air after it the `▾` did, and a
/// cue butted onto the row beside it reads as part of that row (the
/// same cell of air `help`'s headings keep off their own cue).
const COLUMN_GUTTER: u16 = 2;

/// `▴`/`▾` for which way there is more and a footer for how much, the
/// pair every windowed list in the ui carries (`help`, `suggestions`,
/// `lyrics_view`, `queue`). Nothing at all when the list fits, so a
/// short one loses neither a row nor a column to it.
///
/// `cues` is the rect whose last column the cues go in, over the rows
/// that were actually drawn; the albums column narrows it by one more
/// cell than it drew its names in (see `COLUMN_GUTTER`).
#[allow(clippy::too_many_arguments)]
fn scroll_feedback(
    buf: &mut Buffer,
    area: Rect,
    cues: Rect,
    split: Body,
    offset: usize,
    total: usize,
    text: &mut String,
    fitted: &mut String,
) {
    if !split.footer {
        return;
    }
    crate::ui::results_list::scroll_cues(buf, cues, offset, total);
    draw_footer(
        buf,
        area,
        area.y + split.rows as u16,
        total.saturating_sub(offset + split.rows),
        text,
        fitted,
    );
}

/// `21 more below · j/k, wheel`, right-aligned, which is the footer
/// `ui::help` carries and the count the `▴`/`▾` cannot give. The short
/// forms are for a column too narrow for the long one rather than an
/// ellipsis eating the number, which is the one part of it that
/// matters; the artist page's albums column is a third of the pane.
fn draw_footer(
    buf: &mut Buffer,
    area: Rect,
    y: u16,
    left: usize,
    text: &mut String,
    fitted: &mut String,
) {
    use std::fmt::Write;
    if y < area.y || y >= area.y + area.height {
        return;
    }
    let budget = area.width as usize;
    for form in 0..3 {
        text.clear();
        let _ = match (left, form) {
            // scrolled to the bottom: "0 more below" is a count where
            // the answer is that there is no more, which is what the
            // missing `▾` says and what this has to agree with.
            (0, 0) => write!(text, "end of the list · k, wheel up "),
            (0, 1) => write!(text, "end of the list "),
            (0, _) => write!(text, "end "),
            (_, 0) => write!(text, "{left} more below · j/k, wheel "),
            (_, 1) => write!(text, "{left} more below "),
            (_, _) => write!(text, "+{left} "),
        };
        if crate::ui::textfit::width(text) <= budget || form == 2 {
            truncate_into(fitted, text, budget);
            let used = crate::ui::textfit::width(fitted) as u16;
            crate::ui::draw::at(
                buf,
                area.x + area.width.saturating_sub(used),
                y,
                fitted,
                shade(DIM),
                used as usize,
            );
            return;
        }
    }
}

/// `> an album` for one row of the albums column, written into `out`
/// rather than returned: the paint hands the same buffer to every row.
/// Clears `out` itself, so a caller cannot leak the previous row into
/// this one.
fn write_album_text(out: &mut String, item: &SearchItem, selected: bool) {
    out.clear();
    out.push_str(if selected { "> " } else { "  " });
    // albums are the only thing this column ever holds; anything else
    // is a row with no name to draw rather than a fallback label.
    if let SearchItem::Album { name, .. } = item {
        out.push_str(name);
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
    shown: Shown,
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
    let split = list_split(shown.len(), area.height as usize);
    let offset = scroll::offset_for(shown.row_of(selected), split.rows);
    // the last column goes to the `▾` when the list scrolls, so the
    // cue never lands on a title.
    let list = Rect {
        width: area.width.saturating_sub(u16::from(split.footer)),
        ..area
    };
    // the track number column is sized to the listing, so a 120-track
    // album does not run its numbers into the cursor's own column the
    // way a fixed two digits did.
    let gutter = digits(tracks.len()).max(2);
    // one pair of buffers for every row of the frame, where a label,
    // a `format!` and a `truncate` was three allocations per visible
    // row.
    let mut text = String::new();
    let mut fitted = String::new();
    let mut drawn = 0usize;
    for row in 0..split.rows {
        let Some(idx) = shown.get(offset + row) else {
            break;
        };
        let track = &tracks[idx];
        let is_selected = idx == selected;
        if is_selected {
            let level = anim::selection_bg(elapsed, focused && pane_focused);
            fill_row_bg(buf, area, area.y + row as u16, level);
        }
        write_track_text(
            &mut text,
            track,
            idx,
            is_selected,
            show_artist,
            is_starred(track, favorites, remote_starred),
            gutter,
        );
        draw_fitted(
            buf,
            list,
            area.y + row as u16,
            &text,
            if is_selected && focused {
                shade(BRIGHT)
            } else {
                shade(DIM)
            },
            &mut fitted,
        );
        drawn = row + 1;
    }
    scroll_feedback(
        buf,
        area,
        Rect {
            height: drawn as u16,
            ..area
        },
        split,
        offset,
        shown.len(),
        &mut text,
        &mut fitted,
    );
}

/// Decimal digits in a positive number, which is the track-number
/// column's width; `to_string().len()` is the same answer and an
/// allocation. A copy of `ui::queue`'s own, for the same reason
/// `list_split` is.
fn digits(n: usize) -> usize {
    n.max(1).ilog10() as usize + 1
}

/// `>  7. * a title · an artist` for one track row, written into `out`
/// rather than returned, same reason as `write_album_text`: the label
/// used to be a `String` of its own on top of the row's `format!`.
fn write_track_text(
    out: &mut String,
    track: &SearchItem,
    idx: usize,
    selected: bool,
    show_artist: bool,
    starred: bool,
    gutter: usize,
) {
    use std::fmt::Write;
    out.clear();
    out.push_str(if selected { "> " } else { "  " });
    // right-aligned by hand: `{:>gutter$}` would do it, but only
    // through a second `format!`.
    let number = idx + 1;
    for _ in digits(number)..gutter {
        out.push(' ');
    }
    // writing into a `String` cannot fail.
    let _ = write!(out, "{number}. ");
    if starred {
        out.push_str("* ");
    }
    // a track's artist only shows where the listing is not already one
    // artist's (`show_track_artist`); anything but a remote song is a
    // row with no label to draw rather than a fallback.
    if let SearchItem::Song { title, artist, .. } = track {
        out.push_str(title);
        if show_artist {
            out.push_str(" · ");
            out.push_str(artist);
        }
    }
}

fn draw_line(buf: &mut Buffer, area: Rect, y: u16, text: &str, color: ratatui::style::Color) {
    crate::ui::draw::line(buf, area, y, text, color);
}

/// Leads in a row that is context rather than something a keypress
/// acts on; the same glyph `ui::suggestions` and `ui::shellsong` mark
/// one with. All three of this pane's "nothing here" rows are that: a
/// bare "(no tracks)" at the left edge read as the first row of a
/// list, which is exactly what it is saying there is not.
const INFO_MARK: char = '\u{203a}';

fn draw_info(buf: &mut Buffer, area: Rect, y: u16, text: &str) {
    let mut out = String::with_capacity(text.len() + 4);
    out.push(' ');
    out.push(INFO_MARK);
    out.push(' ');
    out.push_str(text);
    let mut fitted = String::new();
    draw_fitted(buf, area, y, &out, shade(DIM), &mut fitted);
}

/// One row of `area`, clipped to its width with an ellipsis where
/// anything was cut. `fitted` is the caller's reused buffer.
fn draw_fitted(
    buf: &mut Buffer,
    area: Rect,
    y: u16,
    text: &str,
    color: ratatui::style::Color,
    fitted: &mut String,
) {
    truncate_into(fitted, text, area.width as usize);
    crate::ui::draw::line(buf, area, y, fitted, color);
}

/// What a frame of this pane costs, and whether it costs it per
/// *visible row* or per track in the whole listing. main.rs asks for
/// this pane's row map once a frame and then paints it, so anything in
/// here that scales with the track count is paid for twice at the
/// display's rate.
///
/// A measurement, not an assertion: the numbers move with the box and
/// a ceiling here would be a flake. Run it with
/// `cargo test --release --lib ui::browse::frame_budget -- --ignored --nocapture`;
/// the dev profile is `opt-level = 0` and reads about 10x the shipping
/// cost, which is why the build is the first thing printed.
#[cfg(test)]
mod frame_budget {
    use super::tests::{eager_album_text, eager_track_text, long_album};
    use super::*;
    use crate::ui::testbuf::buffer;
    use std::time::Instant;

    /// Alternating batches in one process, best of each. Sequential
    /// runs on this box differ 1.6-2.1x under load, so two separate
    /// measurements are not comparable and this is the only shape that
    /// is; same harness as `results_list::frame_budget`.
    fn best_each(
        rounds: usize,
        iters: usize,
        a: &mut dyn FnMut(),
        b: &mut dyn FnMut(),
    ) -> (f64, f64) {
        let mut best = (f64::MAX, f64::MAX);
        let time = |f: &mut dyn FnMut()| {
            let at = Instant::now();
            for _ in 0..iters {
                f();
            }
            at.elapsed().as_secs_f64() * 1e6 / iters as f64
        };
        for _ in 0..rounds {
            best.0 = best.0.min(time(a));
            best.1 = best.1.min(time(b));
        }
        best
    }

    /// The workspace pane on a tall terminal.
    const PANE: Rect = Rect {
        x: 0,
        y: 0,
        width: 70,
        height: 40,
    };

    fn frame(view: &BrowseView, buf: &mut Buffer) {
        let _ = rows(PANE, view, ArtMode::Off, 2.0);
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
        .render(PANE, buf);
    }

    /// One screenful of row geometry, which is what each of a frame's
    /// two derivations does: place the cursor, then ask for every row
    /// in the window.
    ///
    /// A function, and `screenful_materialized` below is another one,
    /// because an "old" shape written as a branch inside the timing
    /// loop gets inlined and specialized while the real path stays an
    /// ordinary call, and the A/B then lies.
    fn screenful(len: usize, selected: usize) {
        let shown = Shown::All(len);
        let offset = scroll::offset_for(shown.row_of(selected), PANE.height as usize);
        for row in 0..PANE.height as usize {
            std::hint::black_box(shown.get(offset + row));
        }
    }

    /// The same screenful off the `(0..len).collect()` index list that
    /// `Shown::All` replaced, plus the linear `position` scan for the
    /// cursor that went with it.
    fn screenful_materialized(len: usize, selected: usize) {
        let indices: Vec<usize> = (0..len).collect();
        let selected_row = indices.iter().position(|&i| i == selected).unwrap_or(0);
        let offset = scroll::offset_for(selected_row, PANE.height as usize);
        for row in 0..PANE.height as usize {
            std::hint::black_box(indices.get(offset + row).copied());
        }
    }

    #[test]
    #[ignore = "a measurement; run it explicitly"]
    fn what_a_frame_of_the_browse_pane_costs() {
        // 144hz is `anim::MAX_HZ`, the fastest the poll loop will ever
        // redraw at, so it is the tightest deadline this path has.
        let budget = 1e6 / crate::anim::MAX_HZ as f64;
        println!(
            "--- browse pane {}x{}, budget {budget:.0}us at {}hz, {} build",
            PANE.width,
            PANE.height,
            crate::anim::MAX_HZ,
            if cfg!(debug_assertions) {
                "DEBUG (not the shipping cost; re-run with --release)"
            } else {
                "release"
            }
        );
        // the short listing is the control: a pane's worth of tracks
        // and nothing more. Any gap between the two is work the frame
        // did on tracks that are not on screen.
        let small = long_album(PANE.height as usize, 0);
        let mut buf_a = buffer(PANE.width, PANE.height);
        let mut buf_b = buffer(PANE.width, PANE.height);
        for n in [2_000usize, 8_000, 15_000] {
            for (what, selected) in [("row 0", 0usize), ("the last row", n - 1)] {
                let long = long_album(n, selected);
                let (big, ctrl) = {
                    let mut on_long = || frame(&long, &mut buf_a);
                    let mut on_small = || frame(&small, &mut buf_b);
                    best_each(5, 20, &mut on_long, &mut on_small)
                };
                let (closed, materialized) =
                    best_each(5, 20, &mut || screenful(n, selected), &mut || {
                        screenful_materialized(n, selected)
                    });
                // and the whole frame either way. `frame` already does
                // the two closed-form derivations, so stacking the two
                // materialized ones on top is the old frame plus one
                // closed screenful of noise.
                let (was, now) = best_each(
                    5,
                    20,
                    &mut || {
                        screenful_materialized(n, selected);
                        screenful_materialized(n, selected);
                        frame(&long, &mut buf_a);
                    },
                    &mut || frame(&long, &mut buf_b),
                );
                println!(
                    "  {n:>6} tracks, cursor on {what:<12}: frame {big:>8.1}us ({:>5.1}% of budget)   {} on screen {ctrl:>7.1}us   {:>5.1}x\n{:24}one screenful of geometry: closed form {closed:>8.2}us   a Vec<usize> {materialized:>8.1}us   {:>6.1}x\n{:24}the whole frame {now:>8.1}us   with both derivations materialized {was:>8.1}us   {:>5.1}x",
                    big / budget * 100.0,
                    PANE.height,
                    big / ctrl,
                    "",
                    materialized / closed,
                    "",
                    was / now,
                );
            }
        }

        // the row text, which the paint builds once per visible row:
        // one reused buffer plus `truncate_into` against the label,
        // `format!` and `truncate_with_ellipsis` triple it replaced.
        let view = long_album(PANE.height as usize, 0);
        let mut text = String::new();
        let mut fitted = String::new();
        for (case, show_artist) in [("title only", false), ("title and artist", true)] {
            let mut reused = || {
                for (i, t) in view.tracks.iter().enumerate() {
                    write_track_text(&mut text, t, i, i == 0, show_artist, false, 2);
                    truncate_into(&mut fitted, &text, PANE.width as usize);
                    std::hint::black_box(&fitted);
                }
            };
            let mut formatted = || {
                for (i, t) in view.tracks.iter().enumerate() {
                    let line = eager_track_text(t, i, i == 0, show_artist, false, 2);
                    std::hint::black_box(crate::ui::textfit::truncate_with_ellipsis(
                        &line,
                        PANE.width as usize,
                    ));
                }
            };
            let (one_buffer, per_row) = best_each(5, 200, &mut reused, &mut formatted);
            println!(
                "  {} track rows, {case:<16}: two buffers {one_buffer:>7.2}us   a String per row {per_row:>7.2}us   {:.1}x",
                PANE.height,
                per_row / one_buffer,
            );
        }
        let albums = tests::test_albums(PANE.height as usize);
        let mut reused = || {
            for (i, a) in albums.iter().enumerate() {
                write_album_text(&mut text, a, i == 0);
                truncate_into(&mut fitted, &text, PANE.width as usize);
                std::hint::black_box(&fitted);
            }
        };
        let mut formatted = || {
            for (i, a) in albums.iter().enumerate() {
                let line = eager_album_text(a, i == 0);
                std::hint::black_box(crate::ui::textfit::truncate_with_ellipsis(
                    &line,
                    PANE.width as usize,
                ));
            }
        };
        let (one_buffer, per_row) = best_each(5, 200, &mut reused, &mut formatted);
        println!(
            "  {} album rows{:21}: two buffers {one_buffer:>7.2}us   a String per row {per_row:>7.2}us   {:.1}x",
            PANE.height,
            "",
            per_row / one_buffer,
        );

        // and the whole paint, which is what that sits inside.
        let mut pane = buffer(PANE.width, PANE.height);
        let mut paint = || frame(&view, &mut pane);
        let mut geometry = || {
            std::hint::black_box(rows(PANE, &view, ArtMode::Off, 2.0));
        };
        let (painted, geo) = best_each(5, 200, &mut paint, &mut geometry);
        println!(
            "  a whole frame {painted:>7.2}us ({:>4.1}% of budget)   the row map alone {geo:>7.2}us",
            painted / budget * 100.0,
        );
        // the floor, so this is a test and not just a printer: a
        // render that drew nothing would measure beautifully.
        let grid = crate::ui::testbuf::grid(&pane);
        assert!(grid.contains(">  1. song 0"), "nothing painted:\n{grid}");
        assert!(
            grid.lines().last().is_some_and(|r| r.contains(". song")),
            "the pane is not full to its last row:\n{grid}"
        );
    }
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
                " › (no tracks)..........",
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
                // "enter: …" named a key and no action, so the hints
                // drop and the title takes the row (ui::header).
                "Bladee · loading..............",
                "──────────────────────────────",
                "..............................",
                " › loading tracks.............",
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
                "┌─┤ search ├────┬────────────────────────────────────────",
                "│...............│Bladee │ enter: play from here, esc: cl…",
                "└───────────────┼───────┴────────────────────────────────",
                "................│........................................",
                "................├─── ALBUMS ──────────── SONGS ──────────",
                "................│ › (no albums)..........................",
                "................│..............┌─┤ filter songs ├───────┐",
                "................│..............│........................│",
                "................│..............└────────────────────────┘",
                "................│.............. › (no tracks)............",
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

    /// A finished local album listing paints its title, subtitle and
    /// every track at the sizes a pane really gets, with the selection
    /// on a real row.
    #[test]
    fn a_static_listing_page_draws_title_subtitle_and_every_track() {
        for (w, h) in [(40u16, 12u16), (80, 24), (120, 40)] {
            let mut loader = crate::browse::Loader::default();
            let tracks = test_songs(4);
            let view = loader.open_listing("Record Title", "Band Name", tracks.clone());
            assert!(view.selected < view.tracks.len(), "{w}x{h}");
            let area = Rect::new(0, 0, w, h);
            let buf = render_view(&view, area);
            let text: Vec<String> = (0..h).map(|y| row_text(&buf, y)).collect();
            let all = text.join("\n");
            assert!(all.contains("Record Title"), "{w}x{h}\n{all}");
            assert!(all.contains("Band Name"), "{w}x{h}\n{all}");
            for t in &tracks {
                let SearchItem::Song { title, .. } = t else {
                    unreachable!()
                };
                assert!(all.contains(title.as_str()), "{title} at {w}x{h}\n{all}");
            }
        }
    }

    pub(super) fn test_albums(n: usize) -> Vec<SearchItem> {
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

    /// An album listing of `n` tracks with the cursor on `selected`,
    /// which is the one view in here whose cost scales with the whole
    /// listing. Shared with `frame_budget`.
    pub(super) fn long_album(n: usize, selected: usize) -> BrowseView {
        let mut v = test_view(test_songs(n));
        v.selected = selected;
        v
    }

    /// What a track row's text used to cost: a `String` for the label
    /// and a `format!` for the row, per visible row per frame. Kept as
    /// the definition `write_track_text` has to agree with, and as the
    /// benchmark's other half.
    pub(super) fn eager_track_text(
        track: &SearchItem,
        idx: usize,
        selected: bool,
        show_artist: bool,
        starred: bool,
        gutter: usize,
    ) -> String {
        let marker = if selected { "> " } else { "  " };
        let label = match track {
            SearchItem::Song { title, artist, .. } if show_artist => format!("{title} · {artist}"),
            SearchItem::Song { title, .. } => title.clone(),
            _ => String::new(),
        };
        let star = if starred { "* " } else { "" };
        format!(
            "{marker}{:>gutter$}. {star}{label}",
            idx + 1,
            gutter = gutter
        )
    }

    /// Same for the albums column's rows.
    pub(super) fn eager_album_text(item: &SearchItem, selected: bool) -> String {
        let marker = if selected { "> " } else { "  " };
        let name = match item {
            SearchItem::Album { name, .. } => name.as_str(),
            _ => "",
        };
        format!("{marker}{name}")
    }

    /// Both one-buffer row writers have to spell the same row as the
    /// `format!`s they replaced, for every item variant and every
    /// combination of the flags; the track number is right-aligned by
    /// hand where `{:>2}` did it, so a number wider than two columns
    /// has to overflow the same way rather than be clipped.
    #[test]
    fn the_row_text_says_what_a_formatted_one_said() {
        let client = test_client();
        let items = [
            SearchItem::Song {
                id: "1".into(),
                title: "a title".into(),
                artist: "an artist".into(),
                album: "an album".into(),
                client: client.clone(),
            },
            SearchItem::Album {
                id: "2".into(),
                name: "an album".into(),
                artist: "an artist".into(),
                client: client.clone(),
            },
            // the variants neither list draws a label for, which both
            // writers have to leave blank rather than fall back on.
            SearchItem::Artist {
                id: "3".into(),
                name: "an artist".into(),
                client,
            },
            SearchItem::LocalVideo {
                path: "/v/c.mkv".into(),
                title: "a clip".into(),
            },
            SearchItem::Silence { secs: 930 },
        ];
        let mut out = String::new();
        for item in &items {
            for idx in [0usize, 8, 9, 98, 99, 100] {
                for selected in [false, true] {
                    for show_artist in [false, true] {
                        for starred in [false, true] {
                            // the same buffer across every case, so a
                            // missing `clear` shows up as the previous
                            // row's text still being in front.
                            for gutter in [1usize, 2, 3, 4] {
                                write_track_text(
                                    &mut out,
                                    item,
                                    idx,
                                    selected,
                                    show_artist,
                                    starred,
                                    gutter,
                                );
                                assert_eq!(
                                    out,
                                    eager_track_text(
                                        item, idx, selected, show_artist, starred, gutter
                                    ),
                                    "{item:?} idx {idx} gutter {gutter} selected {selected} artist {show_artist} starred {starred}"
                                );
                            }
                        }
                    }
                    write_album_text(&mut out, item, selected);
                    assert_eq!(
                        out,
                        eager_album_text(item, selected),
                        "album row for {item:?} selected {selected}"
                    );
                }
            }
        }
    }

    /// `Shown::All` answers rows and the cursor's row arithmetically
    /// where a `(0..len).collect()` plus a `position` scan did, so it
    /// has to agree with that list at every row, at a `selected` past
    /// the end (which falls back to row 0), and with what
    /// `visible_track_indices` hands back for an unfiltered view,
    /// which is the call `filtered_tracks` skips.
    #[test]
    fn the_unfiltered_row_source_answers_what_the_index_list_would() {
        for n in [0usize, 1, 3, 40, 137] {
            let view = long_album(n, 0);
            assert!(
                filtered_tracks(&view).is_none(),
                "an empty filter query materializes no index list"
            );
            let indices = view.visible_track_indices();
            assert_eq!(indices, (0..n).collect::<Vec<_>>(), "precondition, n {n}");
            let shown = Shown::new(&view, &None);
            assert_eq!(shown.len(), indices.len(), "n {n}");
            for row in 0..n + 3 {
                assert_eq!(
                    shown.get(row),
                    indices.get(row).copied(),
                    "row {row}, n {n}"
                );
            }
            for selected in [0usize, 1, n / 2, n.saturating_sub(1), n, n + 9] {
                assert_eq!(
                    shown.row_of(selected),
                    indices.iter().position(|&i| i == selected).unwrap_or(0),
                    "cursor row for selected {selected}, n {n}"
                );
            }
        }
    }

    /// And the filtered half, which still carries a real index list:
    /// `filtered_tracks` has to produce it and `Shown` has to read
    /// through it rather than treating the rows as their own indices.
    #[test]
    fn a_filter_query_still_materializes_the_tracks_it_matched() {
        let mut view = long_album(20, 0);
        view.is_artist = true;
        view.filter_query = "7".to_string();
        let filtered = filtered_tracks(&view).expect("a query materializes its matches");
        assert_eq!(filtered, view.visible_track_indices());
        assert!(filtered.len() > 1 && filtered.len() < 20, "{filtered:?}");
        let held = Some(filtered.clone());
        let shown = Shown::new(&view, &held);
        assert_eq!(shown.len(), filtered.len());
        assert_eq!(shown.get(0), Some(filtered[0]));
        assert_ne!(shown.get(1), Some(1), "row 1 is not track 1 under a filter");
        assert_eq!(shown.row_of(filtered[1]), 1);
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
                // a row the map names no track on is either blank or
                // the scroll footer, never a track it failed to name.
                None => assert!(
                    text.trim().is_empty() || text.contains("more below"),
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

    /// A finished static listing (a local album) draws its title,
    /// subtitle and every track at the sizes a pane really gets.
    #[test]
    fn a_static_listing_renders_title_subtitle_and_every_track() {
        for (w, h) in [(40u16, 12u16), (80, 24), (120, 40)] {
            let mut view = test_view(test_songs(4));
            view.title = "Record".to_string();
            view.subtitle = "Band".to_string();
            view.selected = 3;
            let area = Rect::new(0, 0, w, h);
            let buf = render_view(&view, area);
            let screen: String = (0..h)
                .map(|y| row_text(&buf, y))
                .collect::<Vec<_>>()
                .join("\n");
            assert!(screen.contains("Record"), "{w}x{h}:\n{screen}");
            assert!(screen.contains("Band"), "{w}x{h}:\n{screen}");
            let (_, song_rows) = rows(area, &view, ArtMode::Off, 2.0).tracks;
            for i in 0..4 {
                assert!(
                    screen.contains(&format!("song {i}")),
                    "{w}x{h} lacks track {i}:\n{screen}"
                );
            }
            let sel = song_rows
                .iter()
                .position(|r| *r == Some(3))
                .unwrap_or_else(|| panic!("{w}x{h}: selection row not mapped"));
            assert!(
                sel < h as usize,
                "{w}x{h}: selection row {sel} out of bounds"
            );
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

    /// The pane is `main_content`, which a short terminal or a grown
    /// bottom bar can squeeze to a few rows; everything below it
    /// belongs to the bottom bar, whose rule is drawn earlier and
    /// never repainted.
    #[test]
    fn the_pane_stays_inside_its_rect_at_every_squeezed_height() {
        for height in [0u16, 1, 2, 3, 4, 5, 6, 7] {
            for bio in [false, true] {
                let area = Rect::new(2, 1, 40, height);
                let mut buf = crate::ui::testbuf::buffer(46, 12);
                let mut v = test_view(test_songs(8));
                v.is_artist = true;
                v.albums = test_albums(8);
                if bio {
                    v.bio = Some(
                        "a long enough last.fm bio that it wraps across every \
                         row the pane reserved for it and then some"
                            .to_string(),
                    );
                }
                render(area, &v, &mut buf);
                crate::ui::testbuf::assert_outside_untouched(&buf, area);
            }
        }
    }
}

/// The feedback a windowed list owes: which way there is more, and
/// how much. Neither of this pane's lists had any, so eight rows of a
/// 120-track album read as the whole album.
#[cfg(test)]
mod scroll_feedback_tests {
    use super::tests::{long_album, test_albums};
    use super::*;
    use crate::browse::tests::{test_songs, test_view};
    use crate::ui::testbuf::{assert_outside_untouched, buffer, grid};

    fn painted(view: &BrowseView, w: u16, h: u16) -> String {
        let mut buf = buffer(w, h);
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
        .render(Rect::new(0, 0, w, h), &mut buf);
        grid(&buf)
    }

    /// A list that fits loses no row to a footer it has nothing to say
    /// on; one that does not gets the row and the count.
    #[test]
    fn only_a_list_that_does_not_fit_pays_for_a_footer() {
        assert_eq!(
            list_split(10, 10),
            Body {
                rows: 10,
                footer: false
            }
        );
        assert_eq!(
            list_split(11, 10),
            Body {
                rows: 9,
                footer: true
            }
        );
        let fits = painted(&test_view(test_songs(6)), 60, 10);
        assert!(
            !fits.contains("more below") && !fits.contains('\u{25be}'),
            "a list that fits says nothing about scrolling:\n{fits}"
        );
        assert!(
            fits.contains("6. song 5"),
            "every track is on screen:\n{fits}"
        );
    }

    /// A footer over one visible row is a column reporting on itself
    /// instead of showing anything, so a list this short keeps all of
    /// its rows.
    ///
    /// The heights are literals, not `0..FOOTER_FLOOR`: a loop bounded
    /// by the constant under test iterates nothing the moment that
    /// constant goes to zero, which is the whole subject of this test
    /// (see docs/testing.md).
    #[test]
    fn a_list_too_short_for_a_footer_keeps_every_row_for_its_items() {
        for height in [0usize, 1, 2] {
            assert_eq!(
                list_split(500, height),
                Body {
                    rows: height,
                    footer: false
                },
                "height {height}"
            );
        }
        // and the property behind those three, at every height: a
        // footer always leaves at least two rows of items under it,
        // and never claims more rows than the area has.
        for height in 0..=12usize {
            let split = list_split(500, height);
            assert!(
                !split.footer || split.rows >= 2,
                "height {height}: {split:?} is a footer over {} rows",
                split.rows
            );
            assert!(split.rows <= height, "height {height}: {split:?}");
        }
    }

    /// The `▴`/`▾` say which way, the footer says how much, and both
    /// have to agree with where the window actually is.
    #[test]
    fn the_cues_point_the_way_there_is_more_and_the_footer_counts_it() {
        let top = painted(&long_album(120, 0), 60, 12);
        assert!(!top.contains('\u{25b4}'), "nothing is above row 0:\n{top}");
        assert!(top.contains('\u{25be}'), "{top}");
        assert!(top.contains("113 more below"), "{top}");

        let middle = painted(&long_album(120, 60), 60, 12);
        assert!(
            middle.contains('\u{25b4}') && middle.contains('\u{25be}'),
            "{middle}"
        );
        assert!(middle.contains("59 more below"), "{middle}");

        let end = painted(&long_album(120, 119), 60, 12);
        assert!(end.contains('\u{25b4}'), "{end}");
        assert!(!end.contains('\u{25be}'), "that is the last track:\n{end}");
        // at the bottom the count is zero, and "0 more below" is a
        // number where the answer is that there is no more.
        assert!(!end.contains("0 more below"), "{end}");
        assert!(end.contains("end of the list"), "{end}");
    }

    /// Both of the artist page's columns are windowed and both scroll
    /// independently, so each owes its own pair.
    #[test]
    fn each_column_of_an_artist_page_reports_its_own_window() {
        let mut view = long_album(30, 25);
        view.is_artist = true;
        view.albums = test_albums(30);
        view.albums_selected = 20;
        let g = painted(&view, 70, 16);
        assert_eq!(
            g.matches('\u{25be}').count(),
            2,
            "one cue per scrolling column:\n{g}"
        );
        // the albums column is a third of the pane, too narrow for the
        // keys, so it keeps the count alone.
        assert!(g.contains("9 more below"), "the albums column:\n{g}");
        assert!(
            g.contains("4 more below \u{b7} j/k, wheel"),
            "the songs column:\n{g}"
        );
    }

    /// The count is the part of the footer that matters, so a column
    /// too narrow for the keys drops the keys rather than letting an
    /// ellipsis eat the number.
    #[test]
    fn a_narrow_footer_keeps_the_count_and_drops_the_keys() {
        let view = long_album(40, 0);
        let wide = painted(&view, 60, 10);
        assert!(wide.contains("35 more below \u{b7} j/k, wheel"), "{wide}");
        let narrow = painted(&view, 20, 8);
        assert!(narrow.contains("37 more below"), "{narrow}");
        assert!(!narrow.contains("j/k, wheel"), "{narrow}");
        let cramped = painted(&view, 12, 7);
        assert!(cramped.contains("+38"), "{cramped}");
    }

    /// The footer writes through `draw::at`, which `Buffer`
    /// bounds-checks and the rect does not: a row outside the rect is
    /// a row of some other pane. Not reachable through `render` (the
    /// split keeps the row inside), so it is exercised directly, same
    /// as `results_list::a_section_rule_outside_its_rect_paints_nothing`.
    #[test]
    fn a_footer_outside_its_rect_paints_nothing() {
        for (h, y) in [(0u16, 2u16), (0, 3), (1, 3), (2, 9), (3, 1), (3, 5)] {
            let area = Rect::new(1, 2, 18, h);
            let mut buf = buffer(22, 10);
            draw_footer(
                &mut buf,
                area,
                y,
                12,
                &mut String::new(),
                &mut String::new(),
            );
            assert_outside_untouched(&buf, area);
        }
        // and the row it must draw on, so this is not a test that
        // passes because the painter does nothing at all.
        let area = Rect::new(1, 2, 18, 2);
        let mut buf = buffer(22, 10);
        draw_footer(
            &mut buf,
            area,
            3,
            12,
            &mut String::new(),
            &mut String::new(),
        );
        assert!(grid(&buf).contains("12 more"), "{}", grid(&buf));
        assert_outside_untouched(&buf, area);
    }

    /// And nothing escapes at any size, cues and footers included.
    #[test]
    fn the_cues_and_footers_stay_inside_the_pane() {
        for (w, h) in [(1u16, 1u16), (2, 3), (6, 5), (20, 8), (70, 16), (120, 40)] {
            for artist in [false, true] {
                let area = Rect::new(2, 1, w, h);
                let mut buf = buffer(w + 5, h + 3);
                let mut view = long_album(120, 60);
                view.is_artist = artist;
                view.albums = test_albums(40);
                view.albums_selected = 30;
                BrowseListView {
                    view: &view,
                    favorites: &Favorites::default(),
                    remote_starred: &HashSet::new(),
                    art_mode: ArtMode::Off,
                    cell_aspect: 2.0,
                    cursor_visible: false,
                    focused: true,
                    elapsed: Duration::ZERO,
                }
                .render(area, &mut buf);
                assert_outside_untouched(&buf, area);
            }
        }
    }
}

/// The two columns this pane aligns by hand: the album names against
/// the songs column that starts in the very next cell, and the track
/// numbers against the cursor's own column.
#[cfg(test)]
mod column_alignment_tests {
    use super::tests::long_album;
    use super::*;
    use crate::ui::testbuf::{buffer, grid};

    /// A full-width album name ran straight into the song titles
    /// beside it, and once the cue column existed the `▾` did. Both
    /// cells stay clear of the songs column.
    #[test]
    fn the_albums_column_keeps_its_gutter_off_the_songs_column() {
        let mut view = long_album(3, 0);
        view.is_artist = true;
        view.albums = (0..30)
            .map(|i| SearchItem::Album {
                id: format!("a{i}"),
                name: format!("an album name far longer than a third of the pane {i}"),
                artist: "artist".to_string(),
                client: crate::browse::tests::test_client(),
            })
            .collect();
        view.albums_selected = 20;
        let area = Rect::new(0, 0, 70, 16);
        let mut buf = buffer(70, 16);
        BrowseListView {
            view: &view,
            favorites: &Favorites::default(),
            remote_starred: &HashSet::new(),
            art_mode: ArtMode::Off,
            cell_aspect: 2.0,
            cursor_visible: false,
            focused: true,
            elapsed: Duration::ZERO,
        }
        .render(area, &mut buf);
        let map = rows(area, &view, ArtMode::Off, 2.0);
        let (albums, album_rows) = map.albums.expect("an artist profile has an albums column");
        // precondition: the names really are too long to fit, so this
        // is about the gutter and not about short names.
        assert!(album_rows.iter().flatten().count() > 1, "{album_rows:?}");
        let right = albums.x + albums.width - 1;
        let g = grid(&buf);
        for (row, idx) in album_rows.iter().enumerate() {
            if idx.is_none() {
                continue;
            }
            let y = albums.y + row as u16;
            // the last cell is always air, whatever is in the one
            // before it.
            assert_eq!(
                buf.cell((right, y)).unwrap().symbol(),
                ".",
                "row {row} reaches the songs column:\n{g}"
            );
            // and the one before it is the cue column: a name never
            // gets there either.
            let cue = buf.cell((right - 1, y)).unwrap().symbol();
            assert!(
                matches!(cue, "." | "\u{25b4}" | "\u{25be}"),
                "row {row} put {cue:?} in the cue column:\n{g}"
            );
        }
        assert!(g.contains('\u{25be}'), "the column scrolls:\n{g}");
    }

    /// `{:>2}` ran a three-digit number into the cursor's own column,
    /// so a 120-track album had no gap between `>` and its number.
    /// The column is sized to the listing instead.
    #[test]
    fn the_track_number_column_is_sized_to_the_listing() {
        // the cursor is on track 1, so one row carries the marker and
        // the rest are the same columns with blanks in its place.
        for (n, cursor, plain) in [
            (9usize, ">  1. song 0", "   2. song 1"),
            (40, ">  1. song 0", "   2. song 1"),
            // three digits: the number used to eat the marker's cell.
            (120, ">   1. song 0", "    2. song 1"),
        ] {
            let mut buf = buffer(40, 20);
            let view = long_album(n, 0);
            BrowseListView {
                view: &view,
                favorites: &Favorites::default(),
                remote_starred: &HashSet::new(),
                art_mode: ArtMode::Off,
                cell_aspect: 2.0,
                cursor_visible: false,
                focused: true,
                elapsed: Duration::ZERO,
            }
            .render(Rect::new(0, 0, 40, 20), &mut buf);
            let g = grid(&buf);
            assert!(
                g.contains(cursor),
                "{n} tracks: no row reads {cursor:?}\n{g}"
            );
            assert!(g.contains(plain), "{n} tracks: no row reads {plain:?}\n{g}");
            // and the marker always keeps a cell of its own: a number
            // that filled the gutter used to butt straight onto it.
            for line in g.lines().filter(|l| l.contains(". song")) {
                assert!(
                    line.starts_with("> ") || line.starts_with("  "),
                    "{n} tracks: {line:?} has no room for the cursor"
                );
            }
        }
    }

    /// Longest-listing guard on the gutter itself, which the grids
    /// above only reach through the paint.
    #[test]
    fn the_gutter_width_is_the_decimal_length_of_the_count() {
        for n in [0usize, 1, 9, 10, 99, 100, 101, 999, 15_000] {
            assert_eq!(digits(n), n.max(1).to_string().len(), "{n}");
        }
    }
}

#[cfg(test)]
mod eyeball {
    use super::*;
    use crate::browse::tests::{test_client, test_songs, test_view};
    use crate::ui::testbuf::{buffer, grid};

    fn artist(bio: bool) -> BrowseView {
        let mut v = test_view(test_songs(6));
        v.is_artist = true;
        v.albums = (0..6)
            .map(|i| SearchItem::Album {
                id: format!("album-{i}"),
                name: format!("album {i}"),
                artist: "artist".to_string(),
                client: test_client(),
            })
            .collect();
        if bio {
            v.bio = Some(
                "swedish rapper and singer, one of the most influential \
                 voices in the drain gang collective"
                    .to_string(),
            );
        }
        v
    }

    fn render_into(view: &BrowseView, area: Rect, buf: &mut Buffer) {
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

    fn print(label: &str, view: &BrowseView, w: u16, h: u16) {
        // three rows taller than the rect: those rows are the bottom
        // bar's, and this is where stray paint shows up.
        let mut buf = buffer(w, h + 3);
        render_into(view, Rect::new(0, 0, w, h), &mut buf);
        println!(
            "--- {label}: rect {w}x{h} in a {w}x{} buffer\n{}",
            h + 3,
            grid(&buf)
        );
    }

    /// The heights the host actually hands this pane on a short
    /// terminal (8 rows, default 4-row bottom bar) or a grown bottom
    /// bar (10 rows on a 14-row terminal), both of which leave 4:
    /// `cargo test --lib ui::browse::eyeball -- --nocapture`.
    /// Asserts the floor too, so it is a test and not just a printer.
    #[test]
    fn print_the_pane_at_the_squeezed_heights() {
        print("artist page", &artist(false), 40, 4);
        print("artist page with a bio", &artist(true), 40, 6);
        print("artist page with a bio", &artist(true), 40, 5);
        print("album", &test_view(test_songs(3)), 40, 0);
        print("album", &test_view(test_songs(3)), 40, 1);

        let mut buf = buffer(40, 8);
        render_into(&test_view(test_songs(3)), Rect::new(0, 0, 40, 6), &mut buf);
        assert!(grid(&buf).contains("album"), "{}", grid(&buf));
    }

    /// The states the squeezed heights do not show: empty, one item, a
    /// listing exactly as long as its pane and one far longer, both of
    /// the artist page's columns scrolling at once, album names longer
    /// than their column, and the two widths the footer changes form
    /// at. Read it:
    /// `cargo test --lib ui::browse::eyeball -- --nocapture`.
    #[test]
    fn print_the_pane_at_every_length_and_width() {
        use super::tests::{long_album, test_albums};

        print("album, empty", &test_view(vec![]), 60, 10);
        print("album, one track", &test_view(test_songs(1)), 60, 10);
        // six tracks in the six rows a 10-row pane leaves: the case
        // either side of which a footer appears.
        print("album, exactly fills the pane", &long_album(6, 2), 60, 10);
        print("album, one row over", &long_album(7, 2), 60, 10);
        print("album, 120 tracks at the top", &long_album(120, 0), 60, 12);
        print(
            "album, 120 tracks at the end",
            &long_album(120, 119),
            60,
            12,
        );
        print("album, cramped", &long_album(40, 0), 20, 8);
        print("album, very cramped", &long_album(40, 0), 12, 7);
        let mut artists = long_album(4, 0);
        artists.show_track_artist = true;
        print("album, very wide with artists", &artists, 110, 9);

        let mut both = long_album(30, 25);
        both.is_artist = true;
        both.albums = test_albums(30);
        both.albums_selected = 20;
        print("artist, both columns scrolling", &both, 70, 16);
        let mut empty = test_view(vec![]);
        empty.is_artist = true;
        print("artist, empty", &empty, 70, 14);
        let mut one = long_album(1, 0);
        one.is_artist = true;
        one.albums = test_albums(1);
        print("artist, one of each", &one, 70, 14);
        let mut wide_names = long_album(3, 0);
        wide_names.is_artist = true;
        wide_names.albums = (0..9)
            .map(|i| SearchItem::Album {
                id: format!("a{i}"),
                name: format!("an album name far longer than a third of the pane {i}"),
                artist: "artist".to_string(),
                client: test_client(),
            })
            .collect();
        print("artist, names longer than the column", &wide_names, 70, 14);

        // wide glyphs: two cells each, and the two width models the
        // cut and the cell advance use are different ones.
        let mut wide = test_view(vec![
            SearchItem::Song {
                id: "1".into(),
                title: "日本語のタイトル".into(),
                artist: "アーティスト".into(),
                album: "a".into(),
                client: test_client(),
            },
            SearchItem::Song {
                id: "2".into(),
                title: "🎵🎶 emoji".into(),
                artist: "🎤".into(),
                album: "a".into(),
                client: test_client(),
            },
        ]);
        wide.show_track_artist = true;
        print("album, cjk and emoji", &wide, 34, 8);

        // the floor: the longest case says how much is hidden, which
        // is the whole point of the footer.
        let mut buf = buffer(60, 12);
        render_into(&long_album(120, 0), Rect::new(0, 0, 60, 12), &mut buf);
        assert!(grid(&buf).contains("more below"), "{}", grid(&buf));
    }

    /// Every cell's brightness as a digit, foreground over background.
    ///
    /// The printer that found the sibling pane's two legibility bugs,
    /// neither of which was visible in the chars: a surface drawn at
    /// one flat level, and a feature drawn at the level of the frame
    /// around it. This pane came out clean (names and numbers at
    /// `DIM`, the cursor's row at `BRIGHT`, the rules at `NEAR_BLACK`),
    /// and it is kept so the next change here can be looked at the
    /// same way:
    /// `cargo test --lib ui::browse::eyeball -- --nocapture`.
    #[test]
    fn print_the_shade_map() {
        use super::tests::{long_album, test_albums};

        let mut view = long_album(30, 25);
        view.is_artist = true;
        view.albums = test_albums(30);
        view.albums_selected = 20;
        let mut buf = buffer(70, 16);
        render_into(&view, Rect::new(0, 0, 70, 16), &mut buf);
        let digit = |c: ratatui::style::Color| match c {
            ratatui::style::Color::Rgb(r, _, _) => {
                char::from_digit((r as u32 * 9 + 127) / 255, 10).unwrap_or('9')
            }
            // `Reset` is the terminal's own background, i.e. nothing
            // was painted into this cell at all.
            _ => '.',
        };
        println!(
            "--- 70x16 artist page, chars then fg | bg shades\n{}",
            grid(&buf)
        );
        for y in 0..buf.area.height {
            let fg: String = (0..buf.area.width)
                .map(|x| {
                    let cell = buf.cell((x, y)).unwrap();
                    match cell.symbol() {
                        "." | " " => ' ',
                        _ => digit(cell.fg),
                    }
                })
                .collect();
            let bg: String = (0..buf.area.width)
                .map(|x| digit(buf.cell((x, y)).unwrap().bg))
                .collect();
            println!("{fg} | {bg}");
        }
        // the floor: more than one brightness is in use, which is what
        // "a whole surface at one flat level" fails.
        let levels: std::collections::HashSet<char> = (0..buf.area.height)
            .flat_map(|y| (0..buf.area.width).map(move |x| (x, y)))
            .filter(|(x, y)| !matches!(buf.cell((*x, *y)).unwrap().symbol(), "." | " "))
            .map(|(x, y)| digit(buf.cell((x, y)).unwrap().fg))
            .collect();
        assert!(
            levels.len() >= 3,
            "the pane reads as one flat shade: {levels:?}"
        );
    }
}
