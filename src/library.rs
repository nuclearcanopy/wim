//! Local filesystem music/video scanning. Tags come from `lofty` where
//! available; anything that fails to parse (or video, which we don't tag)
//! falls back to a filename-derived title so it still shows up in search.
//!
//! A scan is the single biggest startup cost in the app (one blocking
//! `Probe::read` per music file), so it never runs on the main thread:
//! see `spawn_scan` for the streaming/parallel path and `TagIndex` for
//! the mtime cache that makes a warm start a pure `stat` pass.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::UNIX_EPOCH;

use anyhow::{Context, Result};
use lofty::file::{AudioFile, TaggedFileExt};
use lofty::prelude::*;
use lofty::probe::Probe;
use serde::{Deserialize, Serialize};
use walkdir::WalkDir;

use crate::cache;
use crate::trace;

const MUSIC_EXTENSIONS: &[&str] = &["mp3", "flac", "ogg", "opus", "m4a", "wav", "aac", "wma"];
const VIDEO_EXTENSIONS: &[&str] = &["mp4", "mkv", "webm", "avi", "mov", "m4v", "ts"];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MediaKind {
    Music,
    Video,
}

#[derive(Debug, Clone)]
pub struct LocalTrack {
    pub path: PathBuf,
    pub title: String,
    pub artist: Option<String>,
    pub album: Option<String>,
    // captured from the tags but not read anywhere yet: SearchItem
    // doesn't carry duration (see lyrics.rs), so local lookups always
    // fall through to lrclib's fuzzy /search rather than the exact
    // /get endpoint. documented, deliberate gap, not blocking.
    #[allow(dead_code)]
    pub duration_secs: Option<u32>,
    pub kind: MediaKind,
}

/// Case-insensitively, and without allocating: this runs once per file
/// the walk sees, including every non-media one, so the lowercased
/// copy it used to make was a `String` per file in a library's worth
/// of cover art and `.nfo`s.
pub fn classify(ext: &str) -> Option<MediaKind> {
    let same = |known: &&str| known.eq_ignore_ascii_case(ext);
    if MUSIC_EXTENSIONS.iter().any(same) {
        Some(MediaKind::Music)
    } else if VIDEO_EXTENSIONS.iter().any(same) {
        Some(MediaKind::Video)
    } else {
        None
    }
}

pub(crate) fn filename_title(path: &Path) -> String {
    path.file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.to_string_lossy().into_owned())
}

fn read_music_tags(path: &Path) -> LocalTrack {
    let fallback_title = filename_title(path);

    let tagged = Probe::open(path).ok().and_then(|p| p.read().ok());
    let Some(tagged) = tagged else {
        return LocalTrack {
            path: path.to_path_buf(),
            title: fallback_title,
            artist: None,
            album: None,
            duration_secs: None,
            kind: MediaKind::Music,
        };
    };

    let duration_secs = Some(tagged.properties().duration().as_secs() as u32);
    let tag = tagged.primary_tag().or_else(|| tagged.first_tag());

    let title = tag
        .and_then(|t| t.title())
        .map(|c| c.into_owned())
        .filter(|s| !s.is_empty())
        .unwrap_or(fallback_title);
    let artist = tag.and_then(|t| t.artist()).map(|c| c.into_owned());
    let album = tag.and_then(|t| t.album()).map(|c| c.into_owned());

    LocalTrack {
        path: path.to_path_buf(),
        title,
        artist,
        album,
        duration_secs,
        kind: MediaKind::Music,
    }
}

fn video_track(path: &Path) -> LocalTrack {
    LocalTrack {
        path: path.to_path_buf(),
        title: filename_title(path),
        artist: None,
        album: None,
        duration_secs: None,
        kind: MediaKind::Video,
    }
}

/// Standard XDG user dirs (`~/Music`, `~/Videos`), used when the config
/// doesn't list any explicit `library_paths`; most people keep their
/// media in the usual home-folder layout and shouldn't have to configure
/// paths just to get started.
pub fn default_library_paths() -> Vec<String> {
    [dirs::audio_dir(), dirs::video_dir()]
        .into_iter()
        .flatten()
        .filter(|p| p.is_dir())
        .map(|p| p.to_string_lossy().into_owned())
        .collect()
}

/// One file's tags as last read, only trusted while its mtime still
/// matches; see `TagIndex`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct IndexedTags {
    mtime_nanos: u64,
    title: String,
    artist: Option<String>,
    album: Option<String>,
    duration_secs: Option<u32>,
}

/// `path -> last-read tags`, so a warm start only has to `stat` every
/// file instead of parsing it: the walk is cheap, `Probe::read` is the
/// entire cost of a scan. Lives next to the download cache (disposable,
/// reconstructable; same reasoning as `cache.rs`) but as json rather
/// than toml, because parsing the index has to stay far cheaper than
/// the reads it saves and a toml map of tens of thousands of entries
/// isn't.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TagIndex {
    #[serde(default)]
    entries: HashMap<String, IndexedTags>,
}

fn index_path() -> Result<PathBuf> {
    Ok(cache::cache_dir()?.join("library-index.json"))
}

/// Nanoseconds, not seconds: a tag edit within the same second as the
/// last scan would otherwise keep serving stale tags, and every
/// filesystem we care about stores better than second granularity.
/// `None` for anything that can't be stat'd, which is never cached.
fn mtime_nanos(meta: &fs::Metadata) -> Option<u64> {
    meta.modified()
        .ok()?
        .duration_since(UNIX_EPOCH)
        .ok()
        .map(|d| d.as_nanos() as u64)
}

impl TagIndex {
    /// Best-effort: a missing/corrupt index just means a cold scan,
    /// same leniency as `CacheIndex::load`.
    pub fn load() -> Self {
        index_path()
            .ok()
            .and_then(|p| fs::read(p).ok())
            .and_then(|raw| serde_json::from_slice(&raw).ok())
            .unwrap_or_default()
    }

    pub fn save(&self) -> Result<()> {
        self.save_to(&index_path()?)
    }

    fn save_to(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let json = serde_json::to_vec(self)?;
        fs::write(path, json).with_context(|| format!("writing {}", path.display()))?;
        Ok(())
    }

    /// The cached tags for `path`, **moved out** of the index, or
    /// `None` on a miss or whenever the file has been touched since
    /// (in which case the caller re-reads).
    ///
    /// The entry goes either way, and that is the point twice over.
    /// A hit's three strings move straight into the `LocalTrack`
    /// instead of being cloned beside tags about to be dropped, which
    /// on a warm start is every file in the library. And whatever is
    /// left in the index afterwards is then exactly the entries for
    /// files this walk did not find, which is what `retain_seen`
    /// needed a `HashSet` of every scanned path to work out.
    fn take(&mut self, path: &Path, mtime_nanos: Option<u64>) -> Option<IndexedTags> {
        let tags = self.entries.remove(path.to_string_lossy().as_ref())?;
        (Some(tags.mtime_nanos) == mtime_nanos).then_some(tags)
    }

    fn record(&mut self, track: &LocalTrack, mtime_nanos: u64) {
        self.entries.insert(
            track.path.to_string_lossy().into_owned(),
            IndexedTags {
                mtime_nanos,
                title: track.title.clone(),
                artist: track.artist.clone(),
                album: track.album.clone(),
                duration_secs: track.duration_secs,
            },
        );
    }
}

/// One chunk of scanned tracks, handed over as it's read rather than
/// all at once at the end, so results fill the ui while the rest of
/// the library is still being parsed.
pub type ScanBatch = Vec<LocalTrack>;

/// Tracks per channel send; small enough that the first results show up
/// almost immediately, large enough that a huge library isn't one
/// channel send (and one `resync_results`) per file.
const SCAN_BATCH: usize = 128;

/// The one thing a scan has to say back that isn't a track: why it
/// couldn't write its tag index, which costs the *next* start a full
/// re-read of every file. `ScanBatch` is a bare `Vec<LocalTrack>` that
/// three other modules destructure, so this rides beside the channel
/// rather than in it; same idiom as `graphics::ArtProbe::poll`.
///
/// Shared with the scan thread, so it's an `Arc<Mutex<_>>`; the lock is
/// held for one `Option` swap and never across io.
#[derive(Clone, Default)]
pub struct ScanReport(Arc<Mutex<Option<String>>>);

impl ScanReport {
    /// The warning the host owes the user, taken rather than read, so
    /// polling it every frame pushes it once.
    pub fn poll(&self) -> Option<String> {
        self.0.lock().ok()?.take()
    }

    fn warn(&self, message: String) {
        if let Ok(mut slot) = self.0.lock() {
            *slot = Some(message);
        }
    }
}

/// Restarts the scan against `paths`, replacing `rx` with the new
/// channel's receiver.
///
/// **Replacing the receiver is the load-bearing part.** A scan signs
/// off by dropping its sender, so an old receiver cannot be reused;
/// worse, the previous scan's remaining batches are still in flight
/// and would drain into whatever corpus is current now, listing every
/// local track twice for the rest of the session. `Op::Rescan` got
/// this right and the onboarding library-paths step did not, which is
/// why the rule lives here rather than in each caller.
///
/// The caller still owns the corpus side: drop the local half (not the
/// whole corpus, or the navidrome catalog goes with it until the next
/// sync) and resync the results.
pub fn restart_scan(paths: Vec<String>, rx: &mut mpsc::Receiver<ScanBatch>) -> ScanReport {
    let (tx, fresh) = mpsc::channel::<ScanBatch>();
    let report = spawn_scan(paths, tx);
    *rx = fresh;
    report
}

/// Scans on its own thread, streaming batches over `tx`; the sender is
/// dropped when the scan finishes, which is how the main thread knows
/// (a disconnected channel) to stop reporting "scanning". Never blocks
/// the caller, which is the whole point: this used to run inline before
/// the first frame. Poll the returned `ScanReport` next to the batch
/// drain.
pub fn spawn_scan(paths: Vec<String>, tx: mpsc::Sender<ScanBatch>) -> ScanReport {
    let report = ScanReport::default();
    let mine = report.clone();
    thread::spawn(move || {
        trace::mark("library scan: started");
        scan_into(&paths, &tx, &mine);
        trace::mark("library scan: finished");
    });
    report
}

/// Recursively scans each configured library path, returning every music
/// and video file found. Unreadable tags are not fatal; the file still
/// gets indexed with a filename-derived title. Falls back to
/// `default_library_paths()` when `paths` is empty. Synchronous; only
/// for callers that have nothing to show until it's done (a library
/// path changing in onboarding), never startup.
pub fn scan(paths: &[String]) -> Vec<LocalTrack> {
    scan_reporting(paths).0
}

/// `scan`, plus whatever it could not persist. Same split and the same
/// reason as `config::load_reporting` and
/// `persistence::load_reporting`: prefer this at a call site with a
/// notification area to push into.
pub fn scan_reporting(paths: &[String]) -> (Vec<LocalTrack>, Option<String>) {
    let (tx, rx) = mpsc::channel();
    let report = ScanReport::default();
    scan_into(paths, &tx, &report);
    drop(tx);
    (rx.into_iter().flatten().collect(), report.poll())
}

/// One file the walk found: its path, kind, and the mtime from the
/// same `stat` the walk already did.
type FoundMedia = (PathBuf, MediaKind, Option<u64>);

/// Every music/video file under `paths`, with the mtime from the same
/// `stat` the walk already did (`None` when it couldn't be read, which
/// just means the file is never tag-cached). Roots are independent
/// trees, so they walk concurrently (the usual `~/Music` + `~/Videos`
/// pair is two stat storms that have nothing to say to each other);
/// results stay in root order, which keeps a scan's batches
/// reproducible.
fn walk_media(paths: &[String]) -> Vec<FoundMedia> {
    match paths {
        [] => Vec::new(),
        [root] => walk_root(root),
        roots => {
            let workers = thread::available_parallelism()
                .map(|n| n.get())
                .unwrap_or(4)
                .min(roots.len());
            let next = AtomicUsize::new(0);
            let mut walked: Vec<Vec<FoundMedia>> = roots.iter().map(|_| Vec::new()).collect();
            let done: Vec<Vec<(usize, Vec<FoundMedia>)>> = thread::scope(|scope| {
                (0..workers)
                    .map(|_| {
                        let next = &next;
                        scope.spawn(move || {
                            let mut mine = Vec::new();
                            loop {
                                let i = next.fetch_add(1, Ordering::Relaxed);
                                let Some(root) = roots.get(i) else {
                                    return mine;
                                };
                                mine.push((i, walk_root(root)));
                            }
                        })
                    })
                    .collect::<Vec<_>>()
                    .into_iter()
                    .map(|handle| handle.join().unwrap_or_default())
                    .collect()
            });
            for (i, found) in done.into_iter().flatten() {
                walked[i] = found;
            }
            walked.into_iter().flatten().collect()
        }
    }
}

fn walk_root(root: &str) -> Vec<FoundMedia> {
    let mut found = Vec::new();
    for entry in WalkDir::new(root)
        .follow_links(true)
        .into_iter()
        .filter_map(|e| e.ok())
    {
        if !entry.file_type().is_file() {
            continue;
        }
        let Some(kind) = entry
            .path()
            .extension()
            .and_then(|e| e.to_str())
            .and_then(classify)
        else {
            continue;
        };
        // one `lstat` per media file and no more: walkdir's own
        // `file_type` comes from the directory read, and a file whose
        // extension is not media is never stat'd (or tagged) at all.
        let mtime = entry.metadata().ok().as_ref().and_then(mtime_nanos);
        // `into_path` moves walkdir's own `PathBuf` rather than
        // copying the bytes into a second one.
        found.push((entry.into_path(), kind, mtime));
    }
    found
}

/// The one real scan path, shared by `scan` and `spawn_scan`: walk,
/// answer everything the mtime index already knows plus every video
/// (never tagged) in one immediate batch, then read the rest in
/// parallel and write the index back.
fn scan_into(paths: &[String], tx: &mpsc::Sender<ScanBatch>, report: &ScanReport) {
    let owned_defaults;
    let paths = if paths.is_empty() {
        owned_defaults = default_library_paths();
        &owned_defaults
    } else {
        paths
    };

    // the walk is a stat storm and the index is a multi-megabyte json
    // parse; neither needs the other, so they overlap instead of
    // costing their sum before the first track can be answered.
    let (found, mut index) = thread::scope(|scope| {
        let index = scope.spawn(TagIndex::load);
        let found = walk_media(paths);
        (found, index.join().unwrap_or_default())
    });
    trace::mark("library scan: walked");
    let Resolved {
        known,
        to_read,
        hits,
        dropped,
    } = resolve_known(found, &mut index);
    trace::mark("library scan: index resolved");

    // a warm start that read nothing new and dropped nothing has an
    // identical index already on disk; rewriting it would be the only
    // io the whole scan did. known before the send, because the send
    // gives `known` away.
    if dropped || !to_read.is_empty() {
        rerecord_hits(&mut index, &known, &hits);
    }

    if !known.is_empty() && tx.send(known).is_err() {
        return; // main thread is gone (shutting down)
    }

    let fresh = read_tags_parallel(&to_read, tx);
    for (track, mtime) in &fresh {
        index.record(track, *mtime);
    }
    if !fresh.is_empty() || dropped {
        save_or_warn(&index, report);
        trace::mark("library scan: index saved");
    }
}

/// Puts back what `resolve_known` moved out of the index. Load-
/// bearing whenever the index is going to be written: without it a
/// scan that re-read a single touched file would persist an index
/// describing only that file, and every warm start after would be a
/// full cold scan.
fn rerecord_hits(index: &mut TagIndex, known: &[LocalTrack], hits: &[(u32, u64)]) {
    for (at, mtime) in hits {
        if let Some(track) = known.get(*at as usize) {
            index.record(track, *mtime);
        }
    }
}

/// Persists the tag index, saying so when it cannot. Same stance and
/// the same name as `config::save_or_warn`: a read-only or full cache
/// dir costs the *next* start a full re-read of every file in the
/// library, which used to happen with nothing said about it.
fn save_or_warn(index: &TagIndex, report: &ScanReport) {
    match index_path() {
        Ok(path) => save_or_warn_to(index, &path, report),
        Err(e) => report.warn(format!("couldn't find where to cache library tags: {e}")),
    }
}

/// `save_or_warn` against an explicit path, so a test never has to
/// write to the real `~/.cache/wim`; same injection as
/// `cache::save_catalog_in`.
fn save_or_warn_to(index: &TagIndex, path: &Path, report: &ScanReport) {
    if let Err(e) = index.save_to(path) {
        report.warn(format!(
            "couldn't cache this library's tags, so the next start has to \
             re-read every file: {e}"
        ));
    }
}

/// The walk's output split against the index: what is already known
/// (cached tags plus every video, which is never tagged) and what
/// still has to be parsed.
struct Resolved {
    known: Vec<LocalTrack>,
    to_read: Vec<(PathBuf, Option<u64>)>,
    /// Which of `known` came out of the index, as `(position, mtime)`.
    /// Only read when the index is going to be rewritten, which is
    /// the one case where the tags `take` moved out have to go back;
    /// two integers a hit rather than a second copy of its strings.
    hits: Vec<(u32, u64)>,
    /// The index held entries for files this walk did not find, so it
    /// is no longer what is on disk.
    dropped: bool,
}

/// Empties `index` of everything this walk accounts for: see
/// `TagIndex::take`. The leftovers are the stale entries, which is
/// why this takes the index by `&mut` and why `scan_into` has to put
/// the hits back before saving.
fn resolve_known(found: Vec<FoundMedia>, index: &mut TagIndex) -> Resolved {
    let mut known = Vec::with_capacity(found.len());
    let mut to_read = Vec::new();
    let mut hits = Vec::new();
    for (path, kind, mtime) in found {
        match kind {
            // never tagged, so never indexed and never looked up.
            MediaKind::Video => known.push(video_track(&path)),
            MediaKind::Music => match index.take(&path, mtime) {
                Some(tags) => {
                    hits.push((known.len() as u32, tags.mtime_nanos));
                    known.push(LocalTrack {
                        path,
                        title: tags.title,
                        artist: tags.artist,
                        album: tags.album,
                        duration_secs: tags.duration_secs,
                        kind: MediaKind::Music,
                    });
                }
                None => to_read.push((path, mtime)),
            },
        }
    }
    let dropped = !index.entries.is_empty();
    index.entries.clear();
    Resolved {
        known,
        to_read,
        hits,
        dropped,
    }
}

/// Tag reads are independent blocking parses, so they run across a pool
/// sized to the machine rather than one after another; each worker
/// sends its own batches straight on and hands back what it read for
/// the index.
fn read_tags_parallel(
    files: &[(PathBuf, Option<u64>)],
    tx: &mpsc::Sender<ScanBatch>,
) -> Vec<(LocalTrack, u64)> {
    if files.is_empty() {
        return Vec::new();
    }
    let next = AtomicUsize::new(0);
    let workers = thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4)
        .min(files.len());
    thread::scope(|scope| {
        (0..workers)
            .map(|_| {
                let next = &next;
                let tx = tx.clone();
                scope.spawn(move || {
                    let mut cacheable = Vec::new();
                    let mut batch: ScanBatch = Vec::new();
                    while let Some((path, mtime)) = files.get(next.fetch_add(1, Ordering::Relaxed))
                    {
                        let track = read_music_tags(path);
                        if let Some(mtime) = mtime {
                            cacheable.push((track.clone(), *mtime));
                        }
                        batch.push(track);
                        if batch.len() >= SCAN_BATCH && tx.send(std::mem::take(&mut batch)).is_err()
                        {
                            return cacheable;
                        }
                    }
                    if !batch.is_empty() {
                        let _ = tx.send(batch);
                    }
                    cacheable
                })
            })
            .collect::<Vec<_>>()
            .into_iter()
            .filter_map(|h| h.join().ok())
            .flatten()
            .collect()
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    fn track(path: &str) -> LocalTrack {
        LocalTrack {
            path: PathBuf::from(path),
            title: "Tagged Title".to_string(),
            artist: Some("Tagged Artist".to_string()),
            album: Some("Tagged Album".to_string()),
            duration_secs: Some(201),
            kind: MediaKind::Music,
        }
    }

    #[test]
    fn classifies_by_extension_case_insensitively() {
        assert_eq!(classify("MP3"), Some(MediaKind::Music));
        assert_eq!(classify("mkv"), Some(MediaKind::Video));
        assert_eq!(classify("txt"), None);
    }

    #[test]
    fn filename_title_strips_extension() {
        let p = Path::new("/music/Artist/Album/01 - Song Title.flac");
        assert_eq!(filename_title(p), "01 - Song Title");
    }

    #[test]
    fn index_hit_returns_the_cached_tags_without_touching_the_file() {
        let mut index = TagIndex::default();
        index.record(&track("/music/a.flac"), 42);

        let hit = index
            .take(Path::new("/music/a.flac"), Some(42))
            .expect("same mtime should hit");
        assert_eq!(hit.title, "Tagged Title");
        assert_eq!(hit.artist.as_deref(), Some("Tagged Artist"));
        assert_eq!(hit.album.as_deref(), Some("Tagged Album"));
        assert_eq!(hit.duration_secs, Some(201));
        assert_eq!(hit.mtime_nanos, 42);
        assert!(
            index.entries.is_empty(),
            "a hit is moved out, not copied out"
        );
    }

    #[test]
    fn index_miss_on_a_path_never_scanned() {
        let mut index = TagIndex::default();
        index.record(&track("/music/a.flac"), 42);
        assert!(index.take(Path::new("/music/b.flac"), Some(42)).is_none());
        assert_eq!(
            index.entries.len(),
            1,
            "a miss on another path must not disturb the entry that is there"
        );
    }

    #[test]
    fn index_miss_once_the_file_has_been_touched() {
        let mut index = TagIndex::default();
        index.record(&track("/music/a.flac"), 42);
        assert!(
            index.take(Path::new("/music/a.flac"), Some(43)).is_none(),
            "a changed mtime must force a re-read, not serve stale tags"
        );
        assert!(
            index.take(Path::new("/music/a.flac"), None).is_none(),
            "nor may an unreadable mtime hit"
        );
    }

    fn found(path: &str, kind: MediaKind, mtime: Option<u64>) -> FoundMedia {
        (PathBuf::from(path), kind, mtime)
    }

    /// What `retain_seen` and its `HashSet` of every scanned path used
    /// to answer, now a by-product of taking each hit out: whatever
    /// the walk did not account for is what has to go.
    #[test]
    fn resolving_drops_entries_for_files_that_are_gone() {
        let mut index = TagIndex::default();
        index.record(&track("/music/a.flac"), 1);
        index.record(&track("/music/gone.flac"), 1);

        let out = resolve_known(
            vec![found("/music/a.flac", MediaKind::Music, Some(1))],
            &mut index,
        );
        assert_eq!(out.known.len(), 1);
        assert_eq!(out.known[0].title, "Tagged Title");
        assert_eq!(out.known[0].path, PathBuf::from("/music/a.flac"));
        assert_eq!(
            out.hits,
            [(0, 1)],
            "the hit, and the mtime to re-record it with"
        );
        assert!(
            out.dropped,
            "an entry for a file the walk never saw must be dropped"
        );
        assert!(index.entries.is_empty());
    }

    /// The other half: a walk that accounts for everything the index
    /// holds has not dropped anything, which is what keeps a warm
    /// start from rewriting an identical file.
    #[test]
    fn a_warm_walk_that_accounts_for_everything_drops_nothing() {
        let mut index = TagIndex::default();
        index.record(&track("/music/a.flac"), 1);

        let out = resolve_known(
            vec![
                found("/music/a.flac", MediaKind::Music, Some(1)),
                found("/videos/v.mkv", MediaKind::Video, Some(9)),
            ],
            &mut index,
        );
        assert_eq!(out.known.len(), 2);
        assert!(out.to_read.is_empty());
        assert!(
            !out.dropped,
            "nothing is missing, so the index on disk is still correct"
        );
        // video is never indexed, so it is not a hit to re-record.
        assert_eq!(out.hits, [(0, 1)]);
        assert_eq!(out.known[1].kind, MediaKind::Video);
        assert_eq!(out.known[1].title, "v");
    }

    /// The invariant that pays for `take` moving tags out instead of
    /// cloning them: on the one path that writes the index, every hit
    /// has to be back in it. Persisting an index that described only
    /// the files this scan re-read would make every later start a
    /// full cold scan.
    #[test]
    fn an_index_about_to_be_written_still_describes_every_cached_file() {
        let mut index = TagIndex::default();
        index.record(&track("/music/warm.flac"), 1);
        index.record(&track("/music/touched.flac"), 1);

        let out = resolve_known(
            vec![
                found("/music/warm.flac", MediaKind::Music, Some(1)),
                found("/music/touched.flac", MediaKind::Music, Some(2)),
            ],
            &mut index,
        );
        assert_eq!(out.to_read.len(), 1, "precondition: one file was touched");
        assert!(
            index.entries.is_empty(),
            "precondition: resolving emptied the index"
        );

        rerecord_hits(&mut index, &out.known, &out.hits);
        let back = index
            .take(Path::new("/music/warm.flac"), Some(1))
            .expect("the warm file's tags must be back in the index");
        assert_eq!(back.title, "Tagged Title");
        assert_eq!(back.artist.as_deref(), Some("Tagged Artist"));
        assert_eq!(back.album.as_deref(), Some("Tagged Album"));
        assert_eq!(back.duration_secs, Some(201));
    }

    /// A tag index that cannot be written costs the *next* start a
    /// full re-read of the whole library, and that used to happen in
    /// silence. Nothing here touches the real `~/.cache/wim`.
    #[test]
    fn an_index_that_cannot_be_written_is_reported_not_swallowed() {
        let nanos = std::time::SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let blocker = std::env::temp_dir().join(format!("wim-idxfail-{nanos}"));
        fs::write(&blocker, b"a file, not a directory").unwrap();

        let mut index = TagIndex::default();
        index.record(&track("/music/a.flac"), 1);
        let report = ScanReport::default();
        // the index's parent is a regular file, so `create_dir_all`
        // cannot make room for it.
        save_or_warn_to(&index, &blocker.join("library-index.json"), &report);

        let warning = report.poll().expect("a lost tag cache must be reported");
        assert!(
            warning.contains("re-read every file"),
            "the message has to say what it costs: {warning}"
        );
        assert!(
            report.poll().is_none(),
            "taken, not read: polling every frame must push it once"
        );
        fs::remove_file(&blocker).ok();
    }

    #[test]
    fn an_index_that_writes_fine_says_nothing() {
        let nanos = std::time::SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("wim-idxok-{nanos}"));
        let report = ScanReport::default();
        let mut index = TagIndex::default();
        index.record(&track("/music/a.flac"), 1);

        save_or_warn_to(&index, &dir.join("library-index.json"), &report);
        assert!(report.poll().is_none());
        assert!(dir.join("library-index.json").is_file());
        fs::remove_dir_all(&dir).ok();
    }

    /// A touched file is re-read, and must not read as a *drop*: it is
    /// accounted for, and `read_tags_parallel` records it again.
    #[test]
    fn a_touched_file_is_reread_rather_than_counted_as_gone() {
        let mut index = TagIndex::default();
        index.record(&track("/music/a.flac"), 1);

        let out = resolve_known(
            vec![found("/music/a.flac", MediaKind::Music, Some(2))],
            &mut index,
        );
        assert!(out.known.is_empty());
        assert_eq!(out.to_read, [(PathBuf::from("/music/a.flac"), Some(2))]);
        assert!(out.hits.is_empty());
        assert!(
            !out.dropped,
            "a file that is still there is not a dropped entry"
        );
    }

    #[test]
    fn index_survives_a_json_round_trip() {
        let mut index = TagIndex::default();
        index.record(&track("/music/a.flac"), 7);
        let raw = serde_json::to_vec(&index).unwrap();
        let back: TagIndex = serde_json::from_slice(&raw).unwrap();
        assert_eq!(back.entries, index.entries);
    }

    /// Cpu time, not wall time, for the reason `search::tests::cpu_now`
    /// and `cache::tests::best_cpu` exist: this machine's other cores
    /// are busy during a bench run, so wall clock moves 2-3x between
    /// runs while the work done does not.
    fn best_cpu<T>(iters: u32, mut f: impl FnMut() -> T) -> (std::time::Duration, T) {
        let cpu_now = || {
            let mut ts = libc::timespec {
                tv_sec: 0,
                tv_nsec: 0,
            };
            // SAFETY: clock_gettime only writes the timespec we hand it.
            unsafe { libc::clock_gettime(libc::CLOCK_PROCESS_CPUTIME_ID, &mut ts) };
            std::time::Duration::new(
                u64::try_from(ts.tv_sec).unwrap_or(0),
                u32::try_from(ts.tv_nsec).unwrap_or(0),
            )
        };
        let mut best = std::time::Duration::MAX;
        let mut last = f();
        for _ in 0..iters {
            let start = cpu_now();
            last = f();
            best = best.min(cpu_now().saturating_sub(start));
        }
        (best, last)
    }

    fn best<T>(iters: u32, mut f: impl FnMut() -> T) -> (std::time::Duration, T) {
        let mut best = std::time::Duration::MAX;
        let mut last = f();
        for _ in 0..iters {
            let start = std::time::Instant::now();
            last = f();
            best = best.min(start.elapsed());
        }
        (best, last)
    }

    /// A real on-disk library tree of `files` music files in an
    /// artist/album/track layout, so the walk measures a real stat
    /// storm rather than one flat directory. Its own temp dir; nothing
    /// here touches the user's media or `~/.cache/wim`.
    fn media_tree(label: &str, files: usize) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!("wim-scanbench-{label}-{nanos}"));
        let per_album = 17;
        let per_artist = 10;
        let albums = files.div_ceil(per_album);
        for album in 0..albums {
            let dir = root
                .join(format!("artist {}", album / per_artist))
                .join(format!("album {album}"));
            fs::create_dir_all(&dir).unwrap();
            for t in 0..per_album {
                if album * per_album + t >= files {
                    break;
                }
                fs::write(dir.join(format!("{t:02} - some song title.flac")), b"").unwrap();
            }
        }
        root
    }

    /// An index that hits on every file the walk found, which is the
    /// warm-start case: nothing is re-parsed, so the whole scan is the
    /// walk plus this index's parse plus the resolve.
    fn index_covering(found: &[FoundMedia]) -> TagIndex {
        let mut index = TagIndex::default();
        for (path, kind, mtime) in found {
            if *kind != MediaKind::Music {
                continue;
            }
            let Some(mtime) = mtime else { continue };
            index.record(
                &LocalTrack {
                    path: path.clone(),
                    title: "Some Song Title".to_string(),
                    artist: Some("Some Artist Name".to_string()),
                    album: Some("Some Album Name".to_string()),
                    duration_secs: Some(214),
                    kind: MediaKind::Music,
                },
                *mtime,
            );
        }
        index
    }

    /// What `resolve_known` used to be, kept only as the baseline the
    /// measurement below is against: a `HashSet<String>` of every
    /// scanned path, a `lookup` that cloned a hit's three strings and
    /// its path out of the index, and a `retain_seen` pass over the
    /// whole index afterwards.
    /// `known`, `to_read`, and the set of every scanned path that
    /// `retain_seen` needed.
    type ResolvedCloning = (
        Vec<LocalTrack>,
        Vec<(PathBuf, Option<u64>)>,
        HashSet<String>,
    );

    fn resolve_known_cloning(found: Vec<FoundMedia>, index: &TagIndex) -> ResolvedCloning {
        let seen: HashSet<String> = found
            .iter()
            .map(|(p, _, _)| p.to_string_lossy().into_owned())
            .collect();
        let mut known = Vec::new();
        let mut to_read = Vec::new();
        for (path, kind, mtime) in found {
            match kind {
                MediaKind::Video => known.push(video_track(&path)),
                MediaKind::Music => {
                    let hit = mtime.and_then(|m| {
                        let tags = index.entries.get(path.to_string_lossy().as_ref())?;
                        (tags.mtime_nanos == m).then(|| LocalTrack {
                            path: path.to_path_buf(),
                            title: tags.title.clone(),
                            artist: tags.artist.clone(),
                            album: tags.album.clone(),
                            duration_secs: tags.duration_secs,
                            kind: MediaKind::Music,
                        })
                    });
                    match hit {
                        Some(cached) => known.push(cached),
                        None => to_read.push((path, mtime)),
                    }
                }
            }
        }
        (known, to_read, seen)
    }

    fn retain_seen(index: &mut TagIndex, seen: &HashSet<String>) -> bool {
        let before = index.entries.len();
        index.entries.retain(|path, _| seen.contains(path));
        index.entries.len() != before
    }

    /// The three legs of a warm scan, separately: the walk (a stat per
    /// file), the index's json parse, and the resolve that splits the
    /// walk against it. The walk and the parse overlap in `scan_into`,
    /// so the floor is `max(walk, parse) + resolve`; which of the two
    /// leads is the whole question, and the answer is the walk, which
    /// is syscall-bound and the one leg nothing here shortens. The
    /// resolve is printed against `resolve_known_cloning`, the shape
    /// it replaced. `cargo test --release -- --ignored --nocapture
    /// warm_scan_phases`.
    #[test]
    #[ignore = "measurement, run explicitly"]
    fn warm_scan_phases() {
        for files in [5_000usize, 34_000] {
            let root = media_tree("warm", files);
            let roots = vec![root.to_string_lossy().into_owned()];
            let (walk_wall, found) = best(3, || walk_media(&roots));
            let (walk_cpu, _) = best_cpu(3, || walk_media(&roots));
            assert_eq!(found.len(), files);

            let index = index_covering(&found);
            let raw = serde_json::to_vec(&index).unwrap();
            let (parse, parsed) = best_cpu(5, || {
                serde_json::from_slice::<TagIndex>(&raw).unwrap_or_default()
            });
            assert_eq!(parsed.entries.len(), files);

            // both resolves clone `found`, and only one of them can be
            // handed the index to empty, so the clones are charged to
            // both and priced separately.
            let (clone_found, _) = best_cpu(5, || found.clone().len());
            let (was, old) = best_cpu(5, || resolve_known_cloning(found.clone(), &index));
            let (retain, _) = best_cpu(5, || retain_seen(&mut index.clone(), &old.2));
            let mut spares: Vec<TagIndex> = (0..7).map(|_| index.clone()).collect();
            let (now, new) = best_cpu(5, || {
                resolve_known(
                    found.clone(),
                    &mut spares.pop().expect("a spare per iteration"),
                )
            });
            assert_eq!(old.0.len(), files, "a warm index must hit every file");
            assert!(old.1.is_empty());
            assert_eq!(new.known.len(), files);
            assert!(new.to_read.is_empty() && !new.dropped);

            println!(
                "{files:>6} files  walk cpu {walk_cpu:>10.3?} wall {walk_wall:>10.3?}  \
                 index {:>8}B parse {parse:>10.3?}\n         resolve: cloning {:>10.3?} \
                 (+ retain_seen {:>10.3?}) = {:>10.3?}  taking {:>10.3?}   \
                 (the bench's own found.clone() of {clone_found:>10.3?} is out of all three)",
                raw.len(),
                was.saturating_sub(clone_found),
                retain,
                (was + retain).saturating_sub(clone_found),
                now.saturating_sub(clone_found),
            );
            fs::remove_dir_all(&root).ok();
        }
    }
}

/// `restart_scan`'s one real rule, which is the receiver and not the
/// thread.
#[cfg(test)]
mod restart_tests {
    use super::*;

    /// The previous scan's in-flight batches must not land in the
    /// corpus the restart is building.
    ///
    /// This is the bug the onboarding library-paths step had: it built
    /// a fresh corpus and left `scan_rx` alone, so the startup scan's
    /// remaining batches drained straight into it and **every local
    /// track was listed twice for the rest of the session**. Swapping
    /// the receiver is what makes them go nowhere; dropping the old one
    /// is the entire mechanism, so it is worth a test that would catch
    /// someone "simplifying" it back to reusing the channel.
    #[test]
    fn a_restart_leaves_the_old_scans_batches_nowhere_to_land() {
        let (old_tx, mut rx) = mpsc::channel::<ScanBatch>();

        // a batch already queued on the old channel, i.e. a scan that
        // has not finished signing off yet.
        old_tx.send(Vec::new()).expect("the old receiver is still up");

        // nothing to scan, so the new thread finishes immediately and
        // the test does not depend on the filesystem.
        let _report = restart_scan(Vec::new(), &mut rx);

        // the queued batch went with the receiver it was queued on.
        // whatever the new scan does, it cannot be that batch.
        let mut from_old = 0;
        while let Ok(_batch) = rx.try_recv() {
            from_old += 1;
        }
        assert_eq!(
            from_old, 0,
            "a batch from the previous scan reached the new receiver, \
             which is how every local track got listed twice"
        );

        // and the old sender now talks to nobody, rather than to the
        // channel the main loop is reading.
        assert!(
            old_tx.send(Vec::new()).is_err(),
            "the old sender still reaches a live receiver"
        );
    }
}
