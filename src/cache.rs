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

/// How many distinct play days are kept per track. Only the count
/// matters, but keeping the days themselves means raising the
/// threshold later still has history to judge against; 64 is far past
/// any threshold worth configuring and keeps `index.toml` from growing
/// without bound for a track played every day for years.
const MAX_TRACKED_DAYS: usize = 64;

/// Per-track bookkeeping, keyed by the track's stable navidrome id
/// (`SearchItem::Song::id`).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CacheEntry {
    #[serde(default)]
    pub play_count: u32,
    /// Distinct calendar days (`YYYY-MM-DD`, see `day_of`) this track
    /// was played on, oldest first. What the auto-download threshold
    /// actually counts: playing the same song twice in one sitting
    /// says nothing about wanting it cached, coming back to it on
    /// another day does.
    #[serde(default)]
    pub play_days: Vec<String>,
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

/// `YYYY-MM-DD` for a unix timestamp, UTC. UTC rather than local time
/// keeps this free of a date crate or libc FFI; the only cost is that
/// two plays within a few hours of UTC midnight can land on different
/// day labels than the user's own calendar would give them, which for
/// a "played it again another day" signal is noise, not a defect.
pub fn day_of(unix_secs: u64) -> String {
    // days-to-civil, the standard shift-the-epoch-to-March-1st
    // algorithm; March-based years put the leap day last, which is
    // what removes every special case from the month arithmetic.
    let days = (unix_secs / 86_400) as i64 + 719_468;
    let era = if days >= 0 { days } else { days - 146_096 } / 146_097;
    let doe = (days - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe as i64 + era * 400 + i64::from(month <= 2);
    format!("{year:04}-{month:02}-{day:02}")
}

/// Today's `day_of`. The one wall-clock read in the day-threshold
/// path; everything that decides anything takes the day as an
/// argument instead, so the policy stays testable against fixed days.
pub fn today() -> String {
    day_of(now_secs())
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

    /// Records a play that happened on `today` (`YYYY-MM-DD`), bumping
    /// the play count and the set of distinct play days, and returns
    /// how many distinct days this track has now been played on so the
    /// caller can tell whether that just crossed the auto-download
    /// threshold. `today` is a parameter rather than read off the
    /// clock here so the policy is testable against fixed days.
    pub fn record_play(&mut self, id: &str, today: &str) -> u32 {
        let entry = self.entries.entry(id.to_string()).or_default();
        entry.play_count += 1;
        // migration for an index written before play days existed: the
        // one earlier day we can still recover is whatever
        // `last_played_secs` points at, so a track played yesterday
        // and played again now counts as the two days it really was,
        // instead of silently restarting from zero.
        if entry.play_days.is_empty() {
            if let Some(prev) = entry.last_played_secs {
                entry.play_days.push(day_of(prev));
            }
        }
        if entry.play_days.last().map(String::as_str) != Some(today) {
            entry.play_days.retain(|d| d != today);
            entry.play_days.push(today.to_string());
        }
        if entry.play_days.len() > MAX_TRACKED_DAYS {
            let excess = entry.play_days.len() - MAX_TRACKED_DAYS;
            entry.play_days.drain(..excess);
        }
        entry.last_played_secs = Some(now_secs());
        entry.play_days.len() as u32
    }

    /// Distinct days `id` has been played on; what the auto-download
    /// threshold is measured against. The live path reads
    /// `record_play`'s return value instead of coming back for this,
    /// so like `play_count` it's here as the public read API for
    /// whatever UI surface later wants to show it.
    #[allow(dead_code)]
    pub fn days_played(&self, id: &str) -> u32 {
        self.entries
            .get(id)
            .map(|e| e.play_days.len() as u32)
            .unwrap_or(0)
    }

    /// The whole entry for `id`, read-only. The per-field getters
    /// above answer one question each; the suggestion engine needs
    /// frequency, distinct days, and recency together for every track
    /// it scores, so it reads the record instead of three lookups.
    pub fn entry(&self, id: &str) -> Option<&CacheEntry> {
        self.entries.get(id)
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
    fn record_play_increments_and_returns_distinct_days() {
        let mut idx = CacheIndex::default();
        assert_eq!(idx.record_play("t1", "2026-01-01"), 1);
        assert_eq!(idx.record_play("t1", "2026-01-02"), 2);
        assert_eq!(idx.record_play("t2", "2026-01-01"), 1);
        assert_eq!(idx.play_count("t1"), 2);
        assert_eq!(idx.play_count("unknown"), 0);
        assert_eq!(idx.days_played("unknown"), 0);
    }

    #[test]
    fn playing_the_same_song_twice_in_one_day_is_still_one_day() {
        // the actual complaint: pressing enter on a song twice in one
        // sitting used to be enough to trigger a download.
        let mut idx = CacheIndex::default();
        assert_eq!(idx.record_play("t1", "2026-01-01"), 1);
        assert_eq!(idx.record_play("t1", "2026-01-01"), 1);
        assert_eq!(idx.record_play("t1", "2026-01-01"), 1);
        assert_eq!(idx.play_count("t1"), 3, "plays are still counted");
        assert_eq!(idx.days_played("t1"), 1);
    }

    #[test]
    fn two_days_in_a_row_counts_as_two_days() {
        let mut idx = CacheIndex::default();
        idx.record_play("t1", "2026-01-01");
        idx.record_play("t1", "2026-01-01");
        assert_eq!(idx.record_play("t1", "2026-01-02"), 2);
        assert_eq!(idx.days_played("t1"), 2);
    }

    #[test]
    fn a_day_already_recorded_out_of_order_is_not_counted_twice() {
        let mut idx = CacheIndex::default();
        idx.record_play("t1", "2026-01-01");
        idx.record_play("t1", "2026-01-02");
        // a clock that stepped back (or a play straddling midnight
        // reported late) must not inflate the day count.
        assert_eq!(idx.record_play("t1", "2026-01-01"), 2);
        assert_eq!(idx.days_played("t1"), 2);
    }

    #[test]
    fn tracked_days_stay_bounded_for_a_track_played_every_day() {
        let mut idx = CacheIndex::default();
        for day in 0..MAX_TRACKED_DAYS + 20 {
            idx.record_play("t1", &day_of(day as u64 * 86_400));
        }
        assert_eq!(idx.days_played("t1"), MAX_TRACKED_DAYS as u32);
    }

    #[test]
    fn an_index_from_before_play_days_recovers_its_last_played_day() {
        // what loading an older index.toml looks like: a play count and
        // a last-played timestamp, no day list.
        let mut idx = CacheIndex::default();
        idx.entries.insert(
            "t1".to_string(),
            CacheEntry {
                play_count: 9,
                last_played_secs: Some(0),
                ..Default::default()
            },
        );
        // played again on a different day: that's two days, not one,
        // since the old timestamp still tells us about the first.
        assert_eq!(idx.record_play("t1", "2026-01-01"), 2);
        assert_eq!(idx.days_played("t1"), 2);
    }

    #[test]
    fn an_old_index_last_played_today_is_still_only_one_day() {
        let mut idx = CacheIndex::default();
        idx.entries.insert(
            "t1".to_string(),
            CacheEntry {
                play_count: 4,
                last_played_secs: Some(0),
                ..Default::default()
            },
        );
        assert_eq!(idx.record_play("t1", &day_of(0)), 1);
    }

    #[test]
    fn an_old_index_file_still_loads_without_its_new_field() {
        let raw = "[entries.t1]\nplay_count = 3\nlast_played_secs = 0\n";
        let idx: CacheIndex = toml::from_str(raw).expect("old index must still parse");
        assert_eq!(idx.play_count("t1"), 3);
        assert_eq!(idx.days_played("t1"), 0);
    }

    #[test]
    fn day_of_formats_known_unix_timestamps() {
        assert_eq!(day_of(0), "1970-01-01");
        assert_eq!(day_of(86_399), "1970-01-01");
        assert_eq!(day_of(86_400), "1970-01-02");
        // a leap day, and the day after it.
        assert_eq!(day_of(1_709_164_800), "2024-02-29");
        assert_eq!(day_of(1_709_251_200), "2024-03-01");
        assert_eq!(day_of(1_767_225_600), "2026-01-01");
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

        idx.record_play("t1", "2026-01-01");
        idx.record_play("t1", "2026-01-02");
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

        idx.record_play("t1", "2026-01-01");
        idx.mark_cached("t1", path.clone());

        let evicted = idx.sweep_stale(7 * 24 * 3600);
        assert_eq!(evicted, 0);
        assert!(path.exists());
        fs::remove_file(&path).unwrap();
    }
}
