//! Local filesystem music/video scanning. Tags come from `lofty` where
//! available; anything that fails to parse (or video, which we don't tag)
//! falls back to a filename-derived title so it still shows up in search.
//!
//! A scan is the single biggest startup cost in the app (one blocking
//! `Probe::read` per music file), so it never runs on the main thread:
//! see `spawn_scan` for the streaming/parallel path and `TagIndex` for
//! the mtime cache that makes a warm start a pure `stat` pass.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc;
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

pub fn classify(ext: &str) -> Option<MediaKind> {
    let ext = ext.to_ascii_lowercase();
    if MUSIC_EXTENSIONS.contains(&ext.as_str()) {
        Some(MediaKind::Music)
    } else if VIDEO_EXTENSIONS.contains(&ext.as_str()) {
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
        let path = index_path()?;
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let json = serde_json::to_vec(self)?;
        fs::write(&path, json).with_context(|| format!("writing {}", path.display()))?;
        Ok(())
    }

    /// The cached tags for `path`, or `None` on a miss or whenever the
    /// file has been touched since (in which case the caller re-reads).
    fn lookup(&self, path: &Path, mtime_nanos: u64) -> Option<LocalTrack> {
        let tags = self.entries.get(path.to_string_lossy().as_ref())?;
        if tags.mtime_nanos != mtime_nanos {
            return None;
        }
        Some(LocalTrack {
            path: path.to_path_buf(),
            title: tags.title.clone(),
            artist: tags.artist.clone(),
            album: tags.album.clone(),
            duration_secs: tags.duration_secs,
            kind: MediaKind::Music,
        })
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

    /// Drops entries for files this scan didn't see, so a deleted or
    /// moved library doesn't grow the index forever; true when it
    /// actually dropped something (the caller only rewrites the file
    /// when the index changed).
    fn retain_seen(&mut self, seen: &HashSet<String>) -> bool {
        let before = self.entries.len();
        self.entries.retain(|path, _| seen.contains(path));
        self.entries.len() != before
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

/// Scans on its own thread, streaming batches over `tx`; the sender is
/// dropped when the scan finishes, which is how the main thread knows
/// (a disconnected channel) to stop reporting "scanning". Never blocks
/// the caller, which is the whole point: this used to run inline before
/// the first frame.
pub fn spawn_scan(paths: Vec<String>, tx: mpsc::Sender<ScanBatch>) {
    thread::spawn(move || {
        trace::mark("library scan: started");
        scan_into(&paths, &tx);
        trace::mark("library scan: finished");
    });
}

/// Recursively scans each configured library path, returning every music
/// and video file found. Unreadable tags are not fatal; the file still
/// gets indexed with a filename-derived title. Falls back to
/// `default_library_paths()` when `paths` is empty. Synchronous; only
/// for callers that have nothing to show until it's done (a library
/// path changing in onboarding), never startup.
pub fn scan(paths: &[String]) -> Vec<LocalTrack> {
    let (tx, rx) = mpsc::channel();
    scan_into(paths, &tx);
    drop(tx);
    rx.into_iter().flatten().collect()
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
        let mtime = entry.metadata().ok().as_ref().and_then(mtime_nanos);
        found.push((entry.path().to_path_buf(), kind, mtime));
    }
    found
}

/// The one real scan path, shared by `scan` and `spawn_scan`: walk,
/// answer everything the mtime index already knows plus every video
/// (never tagged) in one immediate batch, then read the rest in
/// parallel and write the index back.
fn scan_into(paths: &[String], tx: &mpsc::Sender<ScanBatch>) {
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
    let seen: HashSet<String> = found
        .iter()
        .map(|(p, _, _)| p.to_string_lossy().into_owned())
        .collect();
    trace::mark("library scan: walked");

    let mut known = Vec::new();
    let mut to_read = Vec::new();
    for (path, kind, mtime) in found {
        match kind {
            MediaKind::Video => known.push(video_track(&path)),
            MediaKind::Music => match mtime.and_then(|m| index.lookup(&path, m)) {
                Some(cached) => known.push(cached),
                None => to_read.push((path, mtime)),
            },
        }
    }
    trace::mark("library scan: index resolved");
    if !known.is_empty() && tx.send(known).is_err() {
        return; // main thread is gone (shutting down)
    }

    let fresh = read_tags_parallel(&to_read, tx);
    for (track, mtime) in &fresh {
        index.record(track, *mtime);
    }
    // a warm start that read nothing new and dropped nothing has an
    // identical index already on disk; rewriting it would be the only
    // io the whole scan did.
    if !fresh.is_empty() || index.retain_seen(&seen) {
        let _ = index.save();
        trace::mark("library scan: index saved");
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
            .lookup(Path::new("/music/a.flac"), 42)
            .expect("same mtime should hit");
        assert_eq!(hit.title, "Tagged Title");
        assert_eq!(hit.artist.as_deref(), Some("Tagged Artist"));
        assert_eq!(hit.album.as_deref(), Some("Tagged Album"));
        assert_eq!(hit.duration_secs, Some(201));
        assert_eq!(hit.path, PathBuf::from("/music/a.flac"));
    }

    #[test]
    fn index_miss_on_a_path_never_scanned() {
        let mut index = TagIndex::default();
        index.record(&track("/music/a.flac"), 42);
        assert!(index.lookup(Path::new("/music/b.flac"), 42).is_none());
    }

    #[test]
    fn index_miss_once_the_file_has_been_touched() {
        let mut index = TagIndex::default();
        index.record(&track("/music/a.flac"), 42);
        assert!(
            index.lookup(Path::new("/music/a.flac"), 43).is_none(),
            "a changed mtime must force a re-read, not serve stale tags"
        );
    }

    #[test]
    fn retain_seen_drops_entries_for_files_that_are_gone() {
        let mut index = TagIndex::default();
        index.record(&track("/music/a.flac"), 1);
        index.record(&track("/music/gone.flac"), 1);

        let seen = HashSet::from(["/music/a.flac".to_string()]);
        assert!(index.retain_seen(&seen), "dropping an entry is a change");
        assert!(!index.retain_seen(&seen), "nothing left to drop");

        assert!(index.lookup(Path::new("/music/a.flac"), 1).is_some());
        assert!(index.lookup(Path::new("/music/gone.flac"), 1).is_none());
    }

    #[test]
    fn index_survives_a_json_round_trip() {
        let mut index = TagIndex::default();
        index.record(&track("/music/a.flac"), 7);
        let raw = serde_json::to_vec(&index).unwrap();
        let back: TagIndex = serde_json::from_slice(&raw).unwrap();
        assert_eq!(back.entries, index.entries);
    }
}
