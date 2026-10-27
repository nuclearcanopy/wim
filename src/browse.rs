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
/// of asking for real images at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArtMode {
    Ascii,
    Real,
}

impl ArtMode {
    pub fn toggle(self) -> Self {
        match self {
            ArtMode::Ascii => ArtMode::Real,
            ArtMode::Real => ArtMode::Ascii,
        }
    }
}

/// Rows `draw_header` + the one blank gap row after it always take up
/// (title+rule line, one blank row); fixed because the header's own
/// content is fixed, not computed dynamically per open view.
const HEADER_HEIGHT: u16 = 3;

/// Single source of truth for where the art panel and the track list
/// each land, so the widget (which draws into them) and main.rs
/// (which overlays a real image on the same spot in `Real` mode) can
/// never disagree about where that spot actually is.
pub fn layout(area: Rect, has_art: bool) -> (Option<Rect>, Rect) {
    let y = area.y + HEADER_HEIGHT;
    let body_height = (area.y + area.height).saturating_sub(y);
    // roughly square given ~2:1 terminal cell aspect.
    let art_width = if has_art && body_height > 0 {
        (body_height * 2).min(area.width / 3).max(1)
    } else {
        0
    };
    let art_rect = (art_width > 0).then(|| Rect { x: area.x, y, width: art_width, height: body_height });
    let list_x = area.x + art_width + if art_width > 0 { 1 } else { 0 };
    let list_width = (area.x + area.width).saturating_sub(list_x);
    let list_rect = Rect { x: list_x, y, width: list_width, height: body_height };
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
        .map(|s| SearchItem::Song { id: s.id, title: s.title, artist: s.artist, album: s.album, client: client.clone() })
        .collect()
}
