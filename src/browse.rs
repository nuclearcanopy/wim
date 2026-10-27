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

/// Rows `draw_header` + the one blank gap row after it always take up
/// (title+rule line, one blank row); fixed because the header's own
/// content is fixed, not computed dynamically per open view.
const HEADER_HEIGHT: u16 = 3;

/// Artist bios get up to 3 wrapped lines plus one gap row; albums/
/// playlists never have one so this just doesn't apply to them.
const BIO_LINES: usize = 3;
const BIO_HEIGHT: u16 = BIO_LINES as u16 + 1;

/// Single source of truth for where the art panel and the track list
/// each land, so the widget (which draws into them) and main.rs
/// (which overlays a real image on the same spot in `Real` mode) can
/// never disagree about where that spot actually is.
pub fn layout(area: Rect, has_art: bool, has_bio: bool) -> (Option<Rect>, Rect) {
    let bio_height = if has_bio { BIO_HEIGHT } else { 0 };
    let y = area.y + HEADER_HEIGHT + bio_height;
    let body_height = (area.y + area.height).saturating_sub(y);
    // roughly square given ~2:1 terminal cell aspect.
    let art_width = if has_art && body_height > 0 {
        (body_height * 2).min(area.width / 3).max(1)
    } else {
        0
    };
    let art_rect = (art_width > 0).then_some(Rect {
        x: area.x,
        y,
        width: art_width,
        height: body_height,
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
    pub fn next(&mut self) {
        if self.selected + 1 < self.tracks.len() {
            self.selected += 1;
        }
    }

    pub fn prev(&mut self) {
        self.selected = self.selected.saturating_sub(1);
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
