//! Cover art for whatever is playing, fetched in the background.
//!
//! The browse view already carries the art for an album you opened,
//! and the player borrows it when the playing track belongs to that
//! view. That covers "open album, play, maximize" but nothing else: a
//! track played straight out of search, or out of a view since
//! closed, had no art at all.
//!
//! This fetches it on its own thread and hands it back over a channel,
//! so nothing on the render or input path ever blocks on the network.
//! Keyed per album rather than per track, since every track on one
//! album shares a cover and fetching it ten times would be ten round
//! trips for one image.

use std::collections::HashMap;
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;

use image::DynamicImage;

use crate::search::SearchItem;

/// How many covers to keep decoded in memory. A decoded cover is a
/// few MB, so this is a real ceiling and not a formality; past it the
/// oldest insertion goes. Albums listened to in one sitting are few,
/// and a re-fetch is one request.
const MAX_ENTRIES: usize = 24;

/// What a cover is filed under. Album name plus artist, not an id,
/// because `SearchItem::Song` carries neither an `album_id` nor an
/// `artist_id` (see ROADMAP's name-based-identity item). For *art*
/// that is a safe trade in a way it is not for play affinity: two
/// same-titled albums by one artist are the same record in practice,
/// and the cost of being wrong is a sibling album's cover rather than
/// a mis-attributed listen.
///
/// Falls back to the track's own identity when either name is blank,
/// so an untagged file still gets its own slot instead of colliding
/// with every other untagged file.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Key {
    Album { artist: String, album: String },
    Track(String),
}

/// The art id to ask the server for, with the key to file the answer
/// under. `None` for anything with no fetchable cover: local files
/// (their art is embedded, which is a different job) and remote items
/// that are not songs.
///
/// Subsonic's `getCoverArt` takes a song, album or artist id, so the
/// song's own id is enough here; nothing needs the album id this
/// codebase does not carry.
pub fn fetch_plan(item: &SearchItem) -> Option<(Key, String)> {
    match item {
        SearchItem::Song {
            id, artist, album, ..
        } => {
            let key = if artist.trim().is_empty() || album.trim().is_empty() {
                Key::Track(item.identity().1.into_owned())
            } else {
                Key::Album {
                    artist: artist.trim().to_lowercase(),
                    album: album.trim().to_lowercase(),
                }
            };
            Some((key, id.clone()))
        }
        _ => None,
    }
}

/// A stable string naming whatever cover belongs to `item`, for
/// deciding when a cached `ratatui-image` protocol has gone stale.
/// Same album means same cover means no rebuild.
pub fn art_key(item: &SearchItem) -> String {
    match fetch_plan(item) {
        Some((Key::Album { artist, album }, _)) => format!("a:{artist}\u{1}{album}"),
        // a track with no album to group under, or no fetchable cover
        // at all, is its own key.
        _ => {
            let (kind, id) = item.identity();
            format!("t:{kind}:{id}")
        }
    }
}

struct Fetched {
    key: Key,
    image: Option<DynamicImage>,
}

/// Background cover fetches plus their results, including the failures.
pub struct Cache {
    /// `None` means "asked, and the answer was no". Kept so a cover
    /// the server does not have is not re-requested on every frame
    /// for as long as the track plays.
    entries: HashMap<Key, Option<DynamicImage>>,
    /// Insertion order, for evicting at `MAX_ENTRIES`.
    order: Vec<Key>,
    in_flight: Vec<Key>,
    tx: Sender<Fetched>,
    rx: Receiver<Fetched>,
}

impl Default for Cache {
    fn default() -> Self {
        let (tx, rx) = mpsc::channel();
        Cache {
            entries: HashMap::new(),
            order: Vec::new(),
            in_flight: Vec::new(),
            tx,
            rx,
        }
    }
}

impl Cache {
    /// Takes delivery of anything that finished since the last call.
    /// Returns whether something arrived, so the caller can tell a
    /// frame that gained art from one that did not.
    pub fn poll(&mut self) -> bool {
        let mut got = false;
        while let Ok(Fetched { key, image }) = self.rx.try_recv() {
            self.in_flight.retain(|k| *k != key);
            self.insert(key, image);
            got = true;
        }
        got
    }

    /// Starts a fetch for `item` unless it is already cached, already
    /// in flight, or has no fetchable cover. Cheap enough to call every
    /// frame; the work happens at most once per album.
    pub fn request(&mut self, item: &SearchItem) {
        let Some((key, art_id)) = fetch_plan(item) else {
            return;
        };
        if self.entries.contains_key(&key) || self.in_flight.contains(&key) {
            return;
        }
        let SearchItem::Song { client, .. } = item else {
            return;
        };
        self.in_flight.push(key.clone());
        let client = client.clone();
        let tx = self.tx.clone();
        thread::spawn(move || {
            let image = client
                .get_cover_art_bytes(&art_id)
                .ok()
                .and_then(|bytes| image::load_from_memory(&bytes).ok());
            // a dead receiver just means the app is shutting down.
            let _ = tx.send(Fetched { key, image });
        });
    }

    /// The cover for `item`, if it has arrived. `None` covers all of
    /// "not fetched yet", "being fetched" and "the server has none",
    /// which the caller treats identically: draw no art.
    pub fn get(&self, item: &SearchItem) -> Option<&DynamicImage> {
        let (key, _) = fetch_plan(item)?;
        self.entries.get(&key)?.as_ref()
    }

    fn insert(&mut self, key: Key, image: Option<DynamicImage>) {
        if !self.entries.contains_key(&key) {
            self.order.push(key.clone());
        }
        self.entries.insert(key, image);
        while self.order.len() > MAX_ENTRIES {
            let oldest = self.order.remove(0);
            self.entries.remove(&oldest);
        }
    }

    #[cfg(test)]
    fn deliver(&mut self, key: Key, image: Option<DynamicImage>) {
        self.in_flight.retain(|k| *k != key);
        self.insert(key, image);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn song(id: &str, artist: &str, album: &str) -> SearchItem {
        SearchItem::Song {
            id: id.to_string(),
            title: format!("track {id}"),
            artist: artist.to_string(),
            album: album.to_string(),
            client: crate::browse::tests::test_client(),
        }
    }

    fn local(title: &str) -> SearchItem {
        SearchItem::LocalMusic {
            path: PathBuf::from(format!("/m/{title}.flac")),
            title: title.to_string(),
            artist: None,
            album: None,
        }
    }

    fn img() -> DynamicImage {
        DynamicImage::new_rgb8(2, 2)
    }

    #[test]
    fn two_tracks_from_one_album_share_a_key() {
        let (a, _) = fetch_plan(&song("1", "Bladee", "Crest")).unwrap();
        let (b, _) = fetch_plan(&song("2", "Bladee", "Crest")).unwrap();
        assert_eq!(a, b, "one album must not be fetched once per track");
    }

    #[test]
    fn the_key_ignores_case_and_surrounding_space() {
        let (a, _) = fetch_plan(&song("1", "Bladee", "Crest")).unwrap();
        let (b, _) = fetch_plan(&song("2", " bladee ", "CREST")).unwrap();
        assert_eq!(a, b);
    }

    #[test]
    fn different_albums_do_not_share_a_key() {
        let (a, _) = fetch_plan(&song("1", "Bladee", "Crest")).unwrap();
        let (b, _) = fetch_plan(&song("2", "Bladee", "Icedancer")).unwrap();
        assert_ne!(a, b);
    }

    #[test]
    fn the_art_id_asked_for_is_the_songs_own() {
        let (_, id) = fetch_plan(&song("song-42", "Bladee", "Crest")).unwrap();
        assert_eq!(
            id, "song-42",
            "getCoverArt takes a song id, which is what we have"
        );
    }

    #[test]
    fn an_untagged_track_gets_its_own_slot_rather_than_colliding() {
        let (a, _) = fetch_plan(&song("1", "", "")).unwrap();
        let (b, _) = fetch_plan(&song("2", "", "")).unwrap();
        assert_ne!(a, b, "blank names must not collapse every untagged track");
        assert!(matches!(a, Key::Track(_)));
    }

    #[test]
    fn local_files_have_no_fetch_plan() {
        assert!(
            fetch_plan(&local("x")).is_none(),
            "embedded art is a different job"
        );
    }

    #[test]
    fn a_delivered_cover_is_returned_for_every_track_on_that_album() {
        let mut cache = Cache::default();
        let (key, _) = fetch_plan(&song("1", "Bladee", "Crest")).unwrap();
        cache.deliver(key, Some(img()));
        assert!(cache.get(&song("1", "Bladee", "Crest")).is_some());
        assert!(
            cache.get(&song("2", "Bladee", "Crest")).is_some(),
            "a sibling track should hit the same entry"
        );
    }

    #[test]
    fn a_server_with_no_cover_is_remembered_as_a_no() {
        let mut cache = Cache::default();
        let item = song("1", "Bladee", "Crest");
        let (key, _) = fetch_plan(&item).unwrap();
        cache.deliver(key.clone(), None);
        assert!(cache.get(&item).is_none());
        // the point of caching the negative: request must now be a
        // no-op rather than spawning a thread every frame.
        cache.request(&item);
        assert!(
            cache.in_flight.is_empty(),
            "a known-missing cover was re-requested"
        );
    }

    #[test]
    fn an_in_flight_fetch_is_not_started_twice() {
        let mut cache = Cache::default();
        let (key, _) = fetch_plan(&song("1", "Bladee", "Crest")).unwrap();
        cache.in_flight.push(key);
        cache.request(&song("2", "Bladee", "Crest"));
        assert_eq!(cache.in_flight.len(), 1, "same album, same fetch");
    }

    #[test]
    fn the_cache_is_bounded_and_drops_the_oldest() {
        let mut cache = Cache::default();
        for i in 0..MAX_ENTRIES + 5 {
            let (key, _) = fetch_plan(&song("1", "A", &format!("album {i}"))).unwrap();
            cache.deliver(key, Some(img()));
        }
        assert_eq!(cache.entries.len(), MAX_ENTRIES);
        assert_eq!(cache.order.len(), MAX_ENTRIES);
        // the first album in is the first out
        assert!(cache.get(&song("1", "A", "album 0")).is_none());
        assert!(cache.get(&song("1", "A", "album 28")).is_some());
    }

    #[test]
    fn poll_reports_whether_anything_landed() {
        let mut cache = Cache::default();
        assert!(!cache.poll());
        let (key, _) = fetch_plan(&song("1", "Bladee", "Crest")).unwrap();
        cache
            .tx
            .send(Fetched {
                key,
                image: Some(img()),
            })
            .unwrap();
        assert!(cache.poll());
        assert!(cache.get(&song("1", "Bladee", "Crest")).is_some());
    }

    #[test]
    fn art_key_is_shared_across_an_albums_tracks() {
        assert_eq!(
            art_key(&song("1", "Bladee", "Crest")),
            art_key(&song("2", "Bladee", "Crest")),
            "a protocol must not be rebuilt moving between tracks of one album"
        );
    }

    #[test]
    fn art_key_changes_with_the_album() {
        assert_ne!(
            art_key(&song("1", "Bladee", "Crest")),
            art_key(&song("1", "Bladee", "Icedancer"))
        );
    }

    #[test]
    fn art_key_separates_an_album_from_a_track_of_the_same_name() {
        // "a:" and "t:" prefixes, so an untagged track whose identity
        // happens to read like an album key cannot collide with it.
        let untagged = art_key(&song("x", "", ""));
        assert!(untagged.starts_with("t:"));
        assert!(art_key(&song("x", "A", "B")).starts_with("a:"));
    }

    #[test]
    fn art_key_falls_back_for_items_with_no_cover_to_fetch() {
        let k = art_key(&local("song"));
        assert!(k.starts_with("t:"), "local files still need a stable key");
    }
}
