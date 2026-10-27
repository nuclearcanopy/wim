//! Track-listing view opened on an album or playlist. Opening is a
//! separate action from playing: picking an album/playlist from search
//! shows its tracks here first ("let me scroll through the songs"),
//! rather than jumping straight into playback. Closing (esc) always
//! returns `main_content` to whatever it'd otherwise show (now-playing,
//! lyrics, or the idle banner/logo).

use std::sync::Arc;

use ratatui::layout::Rect;

use crate::search::SearchItem;
use crate::subsonic::Client;

/// Ascii is the "pure-TUI purity" default: a grayscale block
/// rasterization that stays strictly monochrome and needs nothing
/// from the terminal. Real asks `ratatui-image` to use whatever
/// graphics protocol the terminal actually supports (sixel/kitty/
/// iterm2), falling back to colored halfblocks if none is detected;
/// that fallback is explicitly not monochrome, which is the tradeoff
/// of asking for real images at all. Off skips the art panel
/// entirely, giving the track list the full width instead.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArtMode {
    Ascii,
    Real,
    Off,
}

impl ArtMode {
    pub fn toggle(self) -> Self {
        match self {
            ArtMode::Ascii => ArtMode::Real,
            ArtMode::Real => ArtMode::Off,
            ArtMode::Off => ArtMode::Ascii,
        }
    }
}

/// A plain rule, `draw_header`'s own title+rule line, and the one
/// blank gap row after it; fixed because the header's own content is
/// fixed, not computed dynamically per open view. The leading plain
/// rule exists so this view's very first row is a continuation of
/// the sidebar's own search-box top border (see `main.rs`'s vdivider
/// junction at that row) rather than title text sharing a row with
/// that border and reading as crammed against it.
const HEADER_HEIGHT: u16 = 4;

/// Artist bios get up to 3 wrapped lines plus one gap row; albums/
/// playlists never have one so this just doesn't apply to them.
const BIO_LINES: usize = 3;
const BIO_HEIGHT: u16 = BIO_LINES as u16 + 1;

/// Single source of truth for where the art panel and the track list
/// each land, so the widget (which draws into them) and main.rs
/// (which overlays a real image on the same spot in `Real` mode) can
/// never disagree about where that spot actually is. `cell_aspect` is
/// the terminal's actual cell height:width ratio (see
/// `main::cell_aspect`), not a hardcoded guess: using a fixed "2" here
/// while the widget sampled with a different fixed "2" happened to
/// agree with *each other*, but not necessarily with the terminal's
/// real font metrics, which is what actually made the art look
/// slightly too tall at some sizes.
pub fn layout(area: Rect, has_art: bool, has_bio: bool, cell_aspect: f32) -> (Option<Rect>, Rect) {
    let bio_height = if has_bio { BIO_HEIGHT } else { 0 };
    let y = area.y + HEADER_HEIGHT + bio_height;
    let body_height = (area.y + area.height).saturating_sub(y);
    // width wants to be `cell_aspect`x height in cells to look square.
    // Capping width against `area.width / 3` without also shrinking
    // height the same way left the art rect itself non-square (full
    // body_height, clipped width); the rect handed to the widget has
    // to be square first, so height is always re-derived from
    // whatever width actually ends up being, never kept independently.
    let art_width = if has_art && body_height > 0 {
        ((body_height as f32 * cell_aspect).round() as u16)
            .min(area.width / 3)
            .max(1)
    } else {
        0
    };
    let art_height = ((art_width as f32 / cell_aspect).round() as u16)
        .max(1)
        .min(body_height);
    let art_rect = (art_width > 0).then_some(Rect {
        x: area.x,
        y,
        width: art_width,
        height: art_height,
    });
    let list_x = area.x + art_width + if art_width > 0 { 1 } else { 0 };
    let list_width = (area.x + area.width).saturating_sub(list_x);
    let list_rect = Rect {
        x: list_x,
        y,
        width: list_width,
        height: body_height,
    };
    (art_rect, list_rect)
}

pub struct BrowseView {
    pub title: String,
    /// Artist name for an album, empty for a playlist (playlists have
    /// no single artist).
    pub subtitle: String,
    /// Release year; album only, `getAlbumList2` doesn't carry it but
    /// `getAlbum` does. Always `None` for playlists.
    pub year: Option<u32>,
    pub tracks: Vec<SearchItem>,
    pub selected: usize,
    /// Decoded cover art, fetched once on open; `None` for playlists
    /// (no single piece of art) or if the fetch/decode failed, in
    /// which case the art panel just shows nothing rather than erroring
    /// the whole view.
    pub art: Option<image::DynamicImage>,
    /// Artist bio (navidrome's `getArtistInfo2`, proxied from
    /// last.fm); only ever set when this view was opened on an artist,
    /// `None` for albums/playlists.
    pub bio: Option<String>,
    /// Only set for artist views: every album credited to them,
    /// shown as its own column alongside the (filterable) song list
    /// rather than mixed into one flat list. Empty for album/playlist
    /// views, which keep the single-list layout they always had.
    pub is_artist: bool,
    pub albums: Vec<SearchItem>,
    pub albums_selected: usize,
    /// left/right switches which column navigation (j/k/enter/*)
    /// applies to; meaningless for album/playlist views (`is_artist`
    /// is false, there's only ever one column).
    pub albums_focused: bool,
    /// Narrows `tracks` down to whatever matches (plain case-
    /// insensitive substring on the row label, not the sidebar's full
    /// fuzzy search: this is "find a song in one artist's already-
    /// small catalog", not "search everything"). Empty matches all.
    pub filter_query: String,
    pub filtering: bool,
}

/// The bio's own rect, directly below the header and above whatever
/// `layout` gives the art/track list; `None` when there's no bio to
/// show. Kept separate from `layout` itself since the bio doesn't
/// share a row with the art panel the way the track list does.
pub fn bio_rect(area: Rect, has_bio: bool) -> Option<Rect> {
    has_bio.then(|| Rect {
        x: area.x,
        y: area.y + HEADER_HEIGHT,
        width: area.width,
        height: BIO_HEIGHT - 1,
    })
}

/// Naive greedy word-wrap, no hyphenation; the last line gets an
/// ellipsis if there was more text than `max_lines` could hold, same
/// "truncate rather than overflow" rule as everywhere else in the ui.
pub fn wrap_bio(text: &str, width: usize, max_lines: usize) -> Vec<String> {
    let width = width.max(1);
    let mut lines: Vec<String> = Vec::new();
    let mut current = String::new();
    let mut truncated = false;

    for word in text.split_whitespace() {
        if lines.len() >= max_lines {
            truncated = true;
            break;
        }
        let would_be = if current.is_empty() {
            word.len()
        } else {
            current.len() + 1 + word.len()
        };
        if would_be > width && !current.is_empty() {
            lines.push(std::mem::take(&mut current));
            if lines.len() >= max_lines {
                truncated = true;
                break;
            }
        }
        if !current.is_empty() {
            current.push(' ');
        }
        current.push_str(word);
    }
    if !truncated && !current.is_empty() {
        lines.push(current);
    }

    if truncated {
        if let Some(last) = lines.last_mut() {
            let max_chars = width.saturating_sub(1);
            if last.chars().count() > max_chars {
                *last = last.chars().take(max_chars).collect();
            }
            last.push('…');
        }
    }
    lines
}

impl BrowseView {
    /// Indices into `tracks` that match `filter_query`: a plain
    /// case-insensitive substring match on the same label the row
    /// itself shows. Empty query matches everything, so this is
    /// always safe to use as "the currently visible tracks."
    pub fn visible_track_indices(&self) -> Vec<usize> {
        if self.filter_query.is_empty() {
            return (0..self.tracks.len()).collect();
        }
        let needle = self.filter_query.to_lowercase();
        self.tracks
            .iter()
            .enumerate()
            .filter(|(_, item)| {
                crate::ui::results_list::label(item)
                    .to_lowercase()
                    .contains(&needle)
            })
            .map(|(i, _)| i)
            .collect()
    }

    /// Snaps `selected` onto the nearest still-visible track after
    /// the filter changes, rather than leaving it pointed at a row
    /// that's no longer shown at all.
    pub fn clamp_selection_to_filter(&mut self) {
        let visible = self.visible_track_indices();
        if !visible.contains(&self.selected) {
            self.selected = visible.first().copied().unwrap_or(0);
        }
    }

    pub fn next(&mut self) {
        if self.albums_focused {
            if self.albums_selected + 1 < self.albums.len() {
                self.albums_selected += 1;
            }
            return;
        }
        let visible = self.visible_track_indices();
        match visible.iter().position(|&i| i == self.selected) {
            Some(pos) => {
                if let Some(&next_idx) = visible.get(pos + 1) {
                    self.selected = next_idx;
                }
            }
            None => self.selected = visible.first().copied().unwrap_or(0),
        }
    }

    pub fn prev(&mut self) {
        if self.albums_focused {
            self.albums_selected = self.albums_selected.saturating_sub(1);
            return;
        }
        let visible = self.visible_track_indices();
        match visible.iter().position(|&i| i == self.selected) {
            Some(pos) if pos > 0 => self.selected = visible[pos - 1],
            Some(_) => {}
            None => self.selected = visible.first().copied().unwrap_or(0),
        }
    }
}

pub fn songs_to_items(songs: Vec<crate::subsonic::Song>, client: &Arc<Client>) -> Vec<SearchItem> {
    songs
        .into_iter()
        .map(|s| SearchItem::Song {
            id: s.id,
            title: s.title,
            artist: s.artist,
            album: s.album,
            client: client.clone(),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wrap_bio_fits_short_text_on_one_line() {
        let lines = wrap_bio("a short bio", 40, 3);
        assert_eq!(lines, vec!["a short bio".to_string()]);
    }

    #[test]
    fn wrap_bio_truncates_with_ellipsis_past_max_lines() {
        let text = "one two three four five six seven eight nine ten eleven twelve";
        let lines = wrap_bio(text, 10, 2);
        assert_eq!(lines.len(), 2);
        assert!(
            lines[1].ends_with('…'),
            "last line should end truncated: {:?}",
            lines[1]
        );
    }

    #[test]
    fn wrap_bio_handles_empty_text() {
        assert!(wrap_bio("", 40, 3).is_empty());
    }
}
