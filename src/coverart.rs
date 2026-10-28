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
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;
use std::time::{Duration, SystemTime};

use image::DynamicImage;
use md5::{Digest, Md5};

use crate::search::SearchItem;

/// How many covers to keep decoded in memory. Bounded by
/// `MAX_DECODED_EDGE` rather than by whatever the server sent, so this
/// is a predictable ceiling (~1.2 MB each at the limit) instead of
/// "whatever 24 multi-megapixel jpegs decode to"; past it the oldest
/// insertion goes. Eviction is cheap now that the encoded bytes are on
/// disk: coming back to an evicted album is a file read and a decode,
/// not a round trip.
const MAX_ENTRIES: usize = 24;

/// Longest edge kept for a decoded cover. The half-block rasterizer
/// samples at most `cols x 2 rows` (a 400x200 cell grid, i.e. the font
/// zoomed way out, is 400x400 samples) and the `ratatui-image` path
/// draws into an art panel a few hundred pixels across, so this is
/// comfortably above both while a full-resolution cover is far past
/// it: 24 untouched 1500px covers retain ~162 MB of rgb8, 24 of these
/// ~30 MB. See docs/raster.md.
const MAX_DECODED_EDGE: u32 = 640;

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

impl Key {
    /// Whatever cover belongs to `item` is filed under this, whether
    /// or not it can be fetched. Items with no fetchable cover still
    /// get a key: it is what the on-disk store and the protocol cache
    /// are named by, and both need an answer for everything.
    pub fn of(item: &SearchItem) -> Key {
        if let SearchItem::Song { artist, album, .. } = item {
            if !artist.trim().is_empty() && !album.trim().is_empty() {
                return Key::Album {
                    artist: artist.trim().to_lowercase(),
                    album: album.trim().to_lowercase(),
                };
            }
        }
        // no album to group under (an untagged track) or nothing
        // fetchable at all (a local file, a playlist): its own slot,
        // including the item kind so an id that collides across kinds
        // still cannot collide here.
        let (kind, id) = item.identity();
        Key::Track(format!("{kind}:{id}"))
    }

    /// The one canonical name for this key: the in-memory map, the
    /// `ratatui-image` protocol cache and the file on disk all derive
    /// from this single definition, so a cover cannot be written under
    /// one name and read back under another. The `a:`/`t:` prefixes
    /// keep an album key and a track key in separate namespaces, and
    /// U+0001 is the separator because it cannot occur in a tag.
    pub fn name(&self) -> String {
        match self {
            Key::Album { artist, album } => format!("a:{artist}\u{1}{album}"),
            Key::Track(id) => format!("t:{id}"),
        }
    }
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
        SearchItem::Song { id, .. } => Some((Key::of(item), id.clone())),
        _ => None,
    }
}

/// A stable string naming whatever cover belongs to `item`, for
/// deciding when a cached `ratatui-image` protocol has gone stale.
/// Same album means same cover means no rebuild.
pub fn art_key(item: &SearchItem) -> String {
    Key::of(item).name()
}

/// Decodes fetched cover bytes into what the renderers actually want:
/// rgb8 (the rasterizer's contiguous fast path, and what
/// `ratatui-image` resizes from anyway), bounded to
/// `MAX_DECODED_EDGE`. Full-resolution pixels exist for exactly as
/// long as this function runs, and it only ever runs on a fetch
/// thread, never on the render path.
pub fn decode_cover(bytes: &[u8]) -> Option<DynamicImage> {
    let img = image::load_from_memory(bytes).ok()?;
    let (w, h) = (img.width(), img.height());
    if w == 0 || h == 0 {
        return None;
    }
    let img = if w.max(h) > MAX_DECODED_EDGE {
        // `resize` fits inside the box and keeps the aspect ratio;
        // triangle is the cheapest filter that does not alias, and
        // this runs once per cover, not once per frame.
        img.resize(
            MAX_DECODED_EDGE,
            MAX_DECODED_EDGE,
            image::imageops::FilterType::Triangle,
        )
    } else {
        img
    };
    Some(DynamicImage::ImageRgb8(img.into_rgb8()))
}

/// Encoded covers on disk, so a restart (or an eviction from the
/// in-memory bound) costs a file read instead of a round trip.
///
/// Conventions come from `cache`: under `cache::cache_dir()`, purely
/// disposable, and swept by the same "untouched for
/// `config::cache_eviction_days`" rule the track cache uses. No index
/// file, unlike `cache::index.toml`, because there is nothing to
/// record beyond last use: the file's own mtime is that stamp,
/// refreshed on every hit. That keeps the fetch threads free of a
/// shared lock, and writes land on a temp name and are renamed into
/// place so a half-written file can never be read back as an image.
///
/// Negative results are deliberately *not* persisted: "this album has
/// no cover" is cheap to re-ask once per session, and remembering it
/// across restarts would mean art added to the server later never
/// showing up.
mod store {
    use super::*;

    pub fn dir() -> Option<PathBuf> {
        crate::cache::cache_dir().ok().map(|d| d.join("covers"))
    }

    /// Named by a digest of `Key::name`, so the file name cannot
    /// collide across albums and an album title containing a path
    /// separator cannot escape the directory.
    pub fn path(dir: &Path, key: &Key) -> PathBuf {
        let mut hasher = Md5::new();
        hasher.update(key.name().as_bytes());
        dir.join(format!("{:x}", hasher.finalize()))
    }

    pub fn read(dir: &Path, key: &Key) -> Option<Vec<u8>> {
        let path = path(dir, key);
        let bytes = fs::read(&path).ok()?;
        if bytes.is_empty() {
            return None;
        }
        // the mtime is the last-used stamp, so an album still in
        // rotation survives the sweep. best effort: a filesystem that
        // refuses the touch only means this cover ages out early.
        if let Ok(f) = fs::File::options().write(true).open(&path) {
            let _ = f.set_modified(SystemTime::now());
        }
        Some(bytes)
    }

    pub fn write(dir: &Path, key: &Key, bytes: &[u8]) {
        if bytes.is_empty() || fs::create_dir_all(dir).is_err() {
            return;
        }
        let path = path(dir, key);
        let tmp = path.with_extension(format!("{}.tmp", std::process::id()));
        if fs::write(&tmp, bytes).is_ok() && fs::rename(&tmp, &path).is_err() {
            let _ = fs::remove_file(&tmp);
        }
    }

    pub fn remove(dir: &Path, key: &Key) {
        let _ = fs::remove_file(path(dir, key));
    }

    /// Deletes every cover untouched for `max_age`, the same rule and
    /// the same configured day count as `cache::sweep_stale`.
    /// Best-effort per file: one failed delete does not stop the
    /// sweep. Returns how many went.
    pub fn sweep(dir: &Path, max_age: Duration) -> usize {
        let Ok(entries) = fs::read_dir(dir) else {
            return 0;
        };
        let now = SystemTime::now();
        let mut evicted = 0;
        for entry in entries.flatten() {
            let Ok(meta) = entry.metadata() else { continue };
            if !meta.is_file() {
                continue;
            }
            let stale = meta
                .modified()
                .ok()
                .and_then(|m| now.duration_since(m).ok())
                .map(|age| age > max_age)
                // a file with no readable mtime (or one dated in the
                // future) is left alone rather than guessed at; a
                // leftover temp file from a killed write is not.
                .unwrap_or_else(|| entry.file_name().to_string_lossy().ends_with(".tmp"));
            if stale && fs::remove_file(entry.path()).is_ok() {
                evicted += 1;
            }
        }
        evicted
    }
}

/// One cover, from disk if it is there and from the server otherwise.
/// Runs on a fetch thread, never the render path: the decode (tens of
/// ms on a big jpeg) and the downscale both happen here.
///
/// Only bytes that actually decoded are kept, on either side: a disk
/// file that will not decode is deleted and the fetch falls through to
/// the network, and a server response that will not decode is never
/// written, so a bad byte string cannot wedge an album's cover
/// permanently.
fn fetch(
    client: &crate::subsonic::Client,
    art_id: &str,
    key: &Key,
    store: Option<&Path>,
) -> Option<DynamicImage> {
    if let Some(dir) = store {
        if let Some(bytes) = store::read(dir, key) {
            match decode_cover(&bytes) {
                Some(img) => return Some(img),
                None => store::remove(dir, key),
            }
        }
    }
    let bytes = client.get_cover_art_bytes(art_id).ok()?;
    let img = decode_cover(&bytes)?;
    if let Some(dir) = store {
        store::write(dir, key, &bytes);
    }
    Some(img)
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
    /// The on-disk cover store, or `None` when there is to be no disk
    /// at all. `Default` leaves it `None`, which is what makes a unit
    /// test unable to touch a real `~/.cache`; only `new` turns it on.
    store: Option<PathBuf>,
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
            store: None,
            tx,
            rx,
        }
    }
}

impl Cache {
    /// The live constructor: fetches are backed by the on-disk store,
    /// which is swept once here by the same configured day count the
    /// track cache uses (`config::cache_eviction_days`). The sweep
    /// runs on its own thread so startup never waits on a directory
    /// walk; a cover read or written while it runs has a fresh mtime
    /// and so is not a candidate anyway.
    pub fn new(eviction_days: u64) -> Self {
        let store = store::dir();
        if let Some(dir) = store.clone() {
            let max_age = Duration::from_secs(eviction_days.saturating_mul(24 * 3600));
            thread::spawn(move || {
                store::sweep(&dir, max_age);
            });
        }
        Cache {
            store,
            ..Cache::default()
        }
    }

    /// One call per frame: takes delivery of whatever landed, asks for
    /// the playing track's cover, and warms the next queued track's so
    /// maximizing the player the moment it starts is never a blank
    /// wait. Returns whether something arrived, so the caller can tell
    /// a frame that gained art from one that did not. Everything in
    /// here is a no-op once the covers are in hand, and `next` is
    /// usually the same album as `playing`, in which case it is free.
    pub fn tick(&mut self, playing: Option<&SearchItem>, next: Option<&SearchItem>) -> bool {
        let got = self.poll();
        if let Some(item) = playing {
            self.request(item);
        }
        if let Some(item) = next {
            self.request(item);
        }
        got
    }

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
        let store = self.store.clone();
        thread::spawn(move || {
            let image = fetch(&client, &art_id, &key, store.as_deref());
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

    #[test]
    fn the_disk_name_and_the_protocol_key_come_from_one_definition() {
        // the whole point of `Key::name`: a cover written under one
        // name and read back under another is how the wrong cover
        // gets shown.
        let item = song("1", "Bladee", "Crest");
        let (key, _) = fetch_plan(&item).unwrap();
        assert_eq!(key.name(), art_key(&item));
    }

    #[test]
    fn two_albums_never_share_a_file_on_disk() {
        let dir = PathBuf::from("/tmp");
        let a = Key::of(&song("1", "Bladee", "Crest"));
        let b = Key::of(&song("2", "Bladee", "Icedancer"));
        let c = Key::of(&song("3", "Ecco2k", "Crest"));
        assert_ne!(store::path(&dir, &a), store::path(&dir, &b));
        assert_ne!(store::path(&dir, &a), store::path(&dir, &c));
        assert_eq!(store::path(&dir, &a), store::path(&dir, &a));
    }

    #[test]
    fn an_album_title_cannot_escape_the_cover_directory() {
        let dir = PathBuf::from("/tmp/covers");
        let nasty = Key::of(&song("1", "../../..", "../../../etc/passwd"));
        let path = store::path(&dir, &nasty);
        assert_eq!(path.parent(), Some(dir.as_path()));
        assert!(!path.to_string_lossy().contains(".."));
    }

    /// A temp dir of this test's own; nothing here may read or write
    /// the real `~/.cache/wim`.
    fn temp_dir(tag: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("wim-covers-{tag}-{nanos}"));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// A real, decodable png, so the round trip exercises the actual
    /// decode rather than a stand-in.
    fn encoded_png(w: u32, h: u32) -> Vec<u8> {
        let img = DynamicImage::ImageRgb8(image::RgbImage::from_pixel(w, h, image::Rgb([7, 7, 7])));
        let mut out = std::io::Cursor::new(Vec::new());
        img.write_to(&mut out, image::ImageFormat::Png).unwrap();
        out.into_inner()
    }

    #[test]
    fn encoded_bytes_survive_a_write_and_read_round_trip() {
        let dir = temp_dir("roundtrip");
        let key = Key::of(&song("1", "Bladee", "Crest"));
        assert!(store::read(&dir, &key).is_none(), "nothing cached yet");
        let bytes = encoded_png(8, 8);
        store::write(&dir, &key, &bytes);
        assert_eq!(store::read(&dir, &key).as_deref(), Some(bytes.as_slice()));
        // and no temp file left behind by the atomic write
        let leftovers: Vec<_> = fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().contains(".tmp"))
            .collect();
        assert!(leftovers.is_empty(), "atomic write leaked a temp file");
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_hit_refreshes_the_file_so_an_album_in_rotation_is_not_swept() {
        let dir = temp_dir("refresh");
        let key = Key::of(&song("1", "Bladee", "Crest"));
        store::write(&dir, &key, &encoded_png(4, 4));
        // backdate it well past any threshold, then read it: the read
        // is what has to save it from the next sweep.
        let path = store::path(&dir, &key);
        let old = SystemTime::now() - Duration::from_secs(60 * 86_400);
        fs::File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_modified(old)
            .unwrap();
        assert!(store::read(&dir, &key).is_some());
        assert_eq!(store::sweep(&dir, Duration::from_secs(86_400)), 0);
        assert!(path.exists(), "a swept-too-early cover");

        // and without the read, it goes.
        fs::File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_modified(old)
            .unwrap();
        assert_eq!(store::sweep(&dir, Duration::from_secs(86_400)), 1);
        assert!(!path.exists());
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_sweep_leaves_a_recently_used_cover_alone() {
        let dir = temp_dir("fresh");
        let key = Key::of(&song("1", "Bladee", "Crest"));
        store::write(&dir, &key, &encoded_png(4, 4));
        assert_eq!(store::sweep(&dir, Duration::from_secs(86_400)), 0);
        assert!(store::read(&dir, &key).is_some());
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn an_empty_or_truncated_cover_file_reads_as_a_miss() {
        let dir = temp_dir("empty");
        let key = Key::of(&song("1", "Bladee", "Crest"));
        fs::write(store::path(&dir, &key), b"").unwrap();
        assert!(store::read(&dir, &key).is_none());
        // non-empty garbage reads back fine but must not decode, so
        // `fetch` deletes it and falls through to the network.
        fs::write(store::path(&dir, &key), b"not an image").unwrap();
        assert!(store::read(&dir, &key).is_some());
        assert!(decode_cover(b"not an image").is_none());
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_sweep_with_no_cover_directory_at_all_is_a_no_op() {
        let dir = std::env::temp_dir().join("wim-covers-does-not-exist");
        assert_eq!(store::sweep(&dir, Duration::from_secs(1)), 0);
    }

    #[test]
    fn decode_bounds_a_big_cover_and_keeps_its_aspect() {
        let img = decode_cover(&encoded_png(1500, 1000)).expect("a png must decode");
        assert_eq!(img.width().max(img.height()), MAX_DECODED_EDGE);
        assert_eq!(img.width(), MAX_DECODED_EDGE);
        assert!(
            img.height().abs_diff(MAX_DECODED_EDGE * 2 / 3) <= 1,
            "the downscale distorted a 3:2 cover: {}x{}",
            img.width(),
            img.height()
        );
        assert!(
            matches!(img, DynamicImage::ImageRgb8(_)),
            "the rasterizer's contiguous fast path wants rgb8"
        );
    }

    #[test]
    fn decode_leaves_a_cover_already_within_the_bound_alone() {
        let img = decode_cover(&encoded_png(300, 300)).unwrap();
        assert_eq!((img.width(), img.height()), (300, 300));
    }

    #[test]
    fn a_default_cache_touches_no_disk_at_all() {
        // the test constructor: whatever a unit test does with it, it
        // cannot reach the real `~/.cache/wim/covers`.
        assert!(Cache::default().store.is_none());
    }

    #[test]
    fn tick_warms_the_next_queued_track_as_well_as_the_playing_one() {
        let mut cache = Cache::default();
        let playing = song("1", "Bladee", "Crest");
        let next = song("2", "Ecco2k", "E");
        cache.tick(Some(&playing), Some(&next));
        assert_eq!(
            cache.in_flight.len(),
            2,
            "the next track's cover should already be on its way"
        );
        assert!(cache.in_flight.contains(&Key::of(&playing)));
        assert!(cache.in_flight.contains(&Key::of(&next)));
    }

    #[test]
    fn tick_on_the_next_track_of_the_same_album_is_one_fetch() {
        let mut cache = Cache::default();
        cache.tick(
            Some(&song("1", "Bladee", "Crest")),
            Some(&song("2", "Bladee", "Crest")),
        );
        assert_eq!(cache.in_flight.len(), 1);
    }

    /// Numbers for the report, not a gate. `cargo test --release --
    /// --ignored --nocapture cover_memory` prints them.
    #[test]
    #[ignore = "timing, run explicitly"]
    fn cover_memory() {
        use std::time::Instant;

        fn rgb_bytes(img: &DynamicImage) -> usize {
            img.width() as usize * img.height() as usize * 3
        }

        for edge in [600u32, 1000, 1500, 3000] {
            let bytes = encoded_png(edge, edge);
            let t = Instant::now();
            let raw = image::load_from_memory(&bytes).unwrap();
            let plain = t.elapsed();
            let t = Instant::now();
            let bounded = decode_cover(&bytes).unwrap();
            let with_downscale = t.elapsed();
            println!(
                "{edge}px cover: decode {plain:?} -> {} MB each, {} MB for {MAX_ENTRIES}; \
                 decode+downscale {with_downscale:?} -> {} MB each, {} MB for {MAX_ENTRIES}",
                rgb_bytes(&raw) / 1_048_576,
                rgb_bytes(&raw) * MAX_ENTRIES / 1_048_576,
                rgb_bytes(&bounded) / 1_048_576,
                rgb_bytes(&bounded) * MAX_ENTRIES / 1_048_576,
            );
        }
    }
}
