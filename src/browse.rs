//! Track-listing view opened on an album or playlist. Opening is a
//! separate action from playing: picking an album/playlist from search
//! shows its tracks here first ("let me scroll through the songs"),
//! rather than jumping straight into playback. Closing (esc) always
//! returns `main_content` to whatever it'd otherwise show (now-playing,
//! lyrics, or the idle banner/logo).

use std::sync::Arc;

use crate::search::SearchItem;
use crate::subsonic::Client;

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
