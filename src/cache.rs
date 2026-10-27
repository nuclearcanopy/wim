//! Local cache of frequently-played remote tracks: play counts, last-
//! played timestamps, and whether/where a track is downloaded to disk.
//! Persisted next to the actual cached audio files under the XDG cache
//! dir (not state/data, see `persistence.rs` for that equivalent): this
//! is disposable, reconstructable data, exactly what `dirs::cache_dir()`
//! is for. Local files (`SearchItem::LocalMusic`/`LocalVideo`) are never
//! entered here at all; they're already local, there's nothing to cache.
//!
//! Shared between the main thread and background download threads as
//! `Arc<Mutex<CacheIndex>>`; every lock held here is brief (in-memory
//! map lookups plus an occasional small file write), never held across
//! network I/O.

use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

/// Per-track bookkeeping, keyed by the track's stable navidrome id
/// (`SearchItem::Song::id`).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CacheEntry {
    #[serde(default)]
    pub play_count: u32,
    /// Unix seconds; `None` if never played this session or across a
    /// restart before this field existed.
    #[serde(default)]
    pub last_played_secs: Option<u64>,
    /// Absolute path under `tracks_dir()`. `None` means "not on disk
    /// right now" even if `play_count` is well past the download
    /// threshold: a download may still be in flight, have failed, or
    /// have been evicted again since.
    #[serde(default)]
    pub cached_path: Option<String>,
    #[serde(default)]
    pub cached_at_secs: Option<u64>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CacheIndex {
    #[serde(default)]
    entries: HashMap<String, CacheEntry>,
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// `~/.cache/wim`; the actual downloaded audio lives under its own
/// `tracks/` subdirectory (see `tracks_dir`), the index file sits right
/// alongside it.
pub fn cache_dir() -> Result<PathBuf> {
    let dir = dirs::cache_dir().context("no cache dir found for this platform")?;
    Ok(dir.join("wim"))
}

pub fn tracks_dir() -> Result<PathBuf> {
    Ok(cache_dir()?.join("tracks"))
}

fn index_path() -> Result<PathBuf> {
    Ok(cache_dir()?.join("index.toml"))
}

/// Where a given track id would land on disk once downloaded, with the
/// given extension (no leading dot).
pub fn track_path(id: &str, ext: &str) -> Result<PathBuf> {
    Ok(tracks_dir()?.join(format!("{id}.{ext}")))
}

impl CacheIndex {
    /// Best-effort load; a missing/corrupt index just means starting
    /// fresh (nothing known to be cached yet), not an error worth
    /// surfacing, same leniency as `persistence::load`.
    pub fn load() -> Self {
        index_path()
            .ok()
            .and_then(|p| fs::read_to_string(p).ok())
            .and_then(|raw| toml::from_str(&raw).ok())
            .unwrap_or_default()
    }

    pub fn save(&self) -> Result<()> {
        let path = index_path()?;
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let toml = toml::to_string_pretty(self)?;
        fs::write(&path, toml).with_context(|| format!("writing {}", path.display()))?;
        Ok(())
    }

    /// Bumps play count and last-played for `id`, returning the new
    /// play count so the caller can tell whether this play just
    /// crossed the auto-download threshold.
    pub fn record_play(&mut self, id: &str) -> u32 {
        let entry = self.entries.entry(id.to_string()).or_default();
        entry.play_count += 1;
        entry.last_played_secs = Some(now_secs());
        entry.play_count
    }

    // not read anywhere outside this module's own tests yet; kept as
    // part of the public read API for whatever UI surface later wants
    // to show "played N times" (same documented gap as
    // `subsonic::Song::duration`).
    #[allow(dead_code)]
    pub fn play_count(&self, id: &str) -> u32 {
        self.entries.get(id).map(|e| e.play_count).unwrap_or(0)
    }

    /// A still-valid cached path for `id`: recorded in the index *and*
    /// still actually present on disk. A sweep (or someone clearing
    /// the cache dir by hand) must not leave a dangling reference that
    /// playback would otherwise trust.
    pub fn cached_path(&self, id: &str) -> Option<PathBuf> {
        let entry = self.entries.get(id)?;
        let path = PathBuf::from(entry.cached_path.as_ref()?);
        path.is_file().then_some(path)
    }

    pub fn mark_cached(&mut self, id: &str, path: PathBuf) {
        let entry = self.entries.entry(id.to_string()).or_default();
        entry.cached_path = Some(path.to_string_lossy().into_owned());
        entry.cached_at_secs = Some(now_secs());
    }

    /// Deletes every cached file whose track hasn't been played in
    /// `max_age_secs`, freeing the disk space. Entries themselves (play
    /// count/last-played) are kept; only `cached_path`/`cached_at`
    /// clear, so a later re-download still remembers the track's play
    /// history rather than starting the threshold over from zero.
    /// Best-effort per file: one failed delete doesn't stop the rest
    /// of the sweep. Returns how many files were evicted.
    pub fn sweep_stale(&mut self, max_age_secs: u64) -> usize {
        let now = now_secs();
        let mut evicted = 0;
        for entry in self.entries.values_mut() {
            let Some(path) = &entry.cached_path else {
                continue;
            };
            let stale = entry
                .last_played_secs
                .map(|t| now.saturating_sub(t) > max_age_secs)
                .unwrap_or(true);
            if stale {
                let _ = fs::remove_file(path);
                entry.cached_path = None;
                entry.cached_at_secs = None;
                evicted += 1;
            }
        }
        evicted
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn record_play_increments_and_returns_running_count() {
        let mut idx = CacheIndex::default();
        assert_eq!(idx.record_play("t1"), 1);
        assert_eq!(idx.record_play("t1"), 2);
        assert_eq!(idx.record_play("t2"), 1);
        assert_eq!(idx.play_count("t1"), 2);
        assert_eq!(idx.play_count("unknown"), 0);
    }

    #[test]
    fn cached_path_ignores_entries_whose_file_no_longer_exists() {
        let mut idx = CacheIndex::default();
        idx.mark_cached("t1", PathBuf::from("/definitely/does/not/exist.mp3"));
        assert!(idx.cached_path("t1").is_none());
    }

    #[test]
    fn cached_path_returns_the_path_when_the_file_is_really_there() {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!("wim-cache-test-{nanos}.mp3"));
        fs::write(&path, b"not real audio, just needs to exist").unwrap();

        let mut idx = CacheIndex::default();
        idx.mark_cached("t1", path.clone());
        assert_eq!(idx.cached_path("t1"), Some(path.clone()));

        fs::remove_file(&path).unwrap();
    }

    #[test]
    fn sweep_stale_evicts_old_plays_but_keeps_play_count() {
        let mut idx = CacheIndex::default();
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!("wim-cache-sweep-{nanos}.mp3"));
        fs::write(&path, b"fake").unwrap();

        idx.record_play("t1");
        idx.record_play("t1");
        idx.mark_cached("t1", path.clone());
        // backdate last_played well past any reasonable threshold.
        idx.entries.get_mut("t1").unwrap().last_played_secs = Some(0);

        let evicted = idx.sweep_stale(7 * 24 * 3600);
        assert_eq!(evicted, 1);
        assert!(!path.exists(), "stale cached file should be deleted");
        assert!(idx.cached_path("t1").is_none());
        assert_eq!(
            idx.play_count("t1"),
            2,
            "play history must survive eviction, only the file goes"
        );
    }

    #[test]
    fn sweep_stale_leaves_recently_played_tracks_alone() {
        let mut idx = CacheIndex::default();
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!("wim-cache-fresh-{nanos}.mp3"));
        fs::write(&path, b"fake").unwrap();

        idx.record_play("t1");
        idx.mark_cached("t1", path.clone());

        let evicted = idx.sweep_stale(7 * 24 * 3600);
        assert_eq!(evicted, 0);
        assert!(path.exists());
        fs::remove_file(&path).unwrap();
    }
}
