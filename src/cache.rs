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
//!
//! The second thing cached here is each server's indexed catalog
//! (`ServerCatalog`), so a cold start paints the whole library off the
//! disk instead of waiting on the network; see docs/startup-cache.md.

use std::borrow::Cow;
use std::collections::HashMap;
use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use md5::{Digest, Md5};
use serde::{Deserialize, Serialize};

use crate::config::NavidromeConfig;
use crate::subsonic::{self, Album, Artist, Client, Playlist};

/// How many distinct play days are kept per track. Only the count
/// matters, but keeping the days themselves means raising the
/// threshold later still has history to judge against; 64 is far past
/// any threshold worth configuring and keeps the play index from
/// growing without bound for a track played every day for years.
const MAX_TRACKED_DAYS: usize = 64;

/// Per-track bookkeeping, keyed by the track's stable navidrome id
/// (`SearchItem::Song::id`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CacheEntry {
    #[serde(default)]
    pub play_count: u32,
    /// Distinct calendar days this track was played on, in the order
    /// they were recorded, as day numbers (days since the unix epoch;
    /// `day_of` renders one, `day_number` reads one back). What the
    /// auto-download threshold actually counts: playing the same song
    /// twice in one sitting says nothing about wanting it cached,
    /// coming back to it on another day does.
    ///
    /// Packed integers rather than the `Vec<String>` of `YYYY-MM-DD`
    /// this used to be, because both ends of it were measured: at
    /// 20000 tracks x 64 days the day strings were nine tenths of the
    /// whole index parse, on the main thread before the first frame
    /// (`play_index_parse_legs`), and re-parsing the first one per
    /// track was over half the suggestion engine's affinity stage
    /// (`suggest::cost::what_the_history_lookups_cost`). Either shape
    /// still loads off disk; see `play_days_from_disk`.
    #[serde(default, deserialize_with = "play_days_from_disk")]
    pub play_days: Vec<i32>,
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
    /// Plaintext, credential-free source identity used to offer cache rebuilds
    /// after changing the lossy transcode profile. Never a stream URL.
    #[serde(default)]
    pub download_source: Option<DownloadSource>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DownloadSource {
    pub server_url: String,
    pub username: String,
    pub title: String,
    pub artist: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CacheIndex {
    #[serde(default)]
    entries: HashMap<String, CacheEntry>,
}

/// Wall clock in unix seconds. The one clock read in the crate;
/// everything that scores off time takes it as a parameter so it
/// stays reproducible in a test.
pub fn now_secs() -> u64 {
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

/// `YYYY-MM-DD` back to a day number, the exact inverse of `day_of`.
/// Same March-based-year trick, which is what removes every month
/// special case. `None` for anything unparseable, so a hand-edited
/// index cannot panic anything that reads it.
pub fn day_number(ymd: &str) -> Option<i64> {
    let mut parts = ymd.split('-');
    let year: i64 = parts.next()?.parse().ok()?;
    let month: i64 = parts.next()?.parse().ok()?;
    let day: i64 = parts.next()?.parse().ok()?;
    if parts.next().is_some() || !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }
    // the arithmetic below overflows for absurd years from a hand-edited index
    if !(0..=9999).contains(&year) {
        return None;
    }
    let year = if month <= 2 { year - 1 } else { year };
    let era = if year >= 0 { year } else { year - 399 } / 400;
    let yoe = year - era * 400;
    let mp = if month > 2 { month - 3 } else { month + 9 };
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    Some(era * 146_097 + doe - 719_468)
}

/// The day number a timestamp falls on, i.e. `day_number(&day_of(s))`
/// without the string in the middle; `day_of` is a pure render of
/// this, which is what makes the two agree by construction.
fn day_number_of(unix_secs: u64) -> i32 {
    (unix_secs / 86_400) as i32
}

/// Today's `day_of`. The one wall-clock read in the day-threshold
/// path; everything that decides anything takes the day as an
/// argument instead, so the policy stays testable against fixed days.
pub fn today() -> String {
    day_of(now_secs())
}

/// One recorded play day as it may sit on disk: an integer day number
/// (what is written now) or a `YYYY-MM-DD` label (what every index
/// written before the packing holds, including the legacy
/// `index.toml`). An unreadable label is dropped rather than failing
/// the row: the ndjson index costs one track per bad line, but the
/// legacy toml is read as one document, and a single hand-edited date
/// must not reset someone's whole play history.
///
/// Hand-written rather than `#[serde(untagged)]`, which cannot
/// deserialize in one pass and would buffer every day into its own
/// `Content` tree; that is the cost `CatalogRow` exists to avoid, and
/// this field is the one that was measured as the parse.
fn play_days_from_disk<'de, D>(deserializer: D) -> Result<Vec<i32>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use serde::de::{Error, SeqAccess, Visitor};
    use std::fmt;

    struct Day(Option<i32>);

    impl<'de> Deserialize<'de> for Day {
        fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Day, D::Error> {
            struct V;
            impl Visitor<'_> for V {
                type Value = Day;
                fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                    f.write_str("a day number or a YYYY-MM-DD date")
                }
                fn visit_i64<E: Error>(self, v: i64) -> Result<Day, E> {
                    Ok(Day(i32::try_from(v).ok()))
                }
                fn visit_u64<E: Error>(self, v: u64) -> Result<Day, E> {
                    Ok(Day(i32::try_from(v).ok()))
                }
                fn visit_str<E: Error>(self, v: &str) -> Result<Day, E> {
                    Ok(Day(day_number(v).and_then(|d| i32::try_from(d).ok())))
                }
            }
            d.deserialize_any(V)
        }
    }

    struct Days;
    impl<'de> Visitor<'de> for Days {
        type Value = Vec<i32>;
        fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
            f.write_str("a list of day numbers or YYYY-MM-DD dates")
        }
        fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Vec<i32>, A::Error> {
            let mut out = Vec::with_capacity(seq.size_hint().unwrap_or(0).min(MAX_TRACKED_DAYS));
            while let Some(Day(day)) = seq.next_element::<Day>()? {
                out.extend(day);
            }
            Ok(out)
        }
    }
    deserializer.deserialize_seq(Days)
}

/// `~/.cache/wim`; the actual downloaded audio lives under its own
/// `tracks/` subdirectory (see `tracks_dir`), the index file sits right
/// alongside it.
///
/// **Under `cfg(test)` this refuses a path outside the temp dir**, and
/// that is not paranoia. A `cargo test -- --ignored` run reached here
/// through `note_playback_started` -> `CacheIndex::save` from a harness
/// that starts at `CacheIndex::default()`, and overwrote a real
/// install's play index with the fixture's one row. The already
/// downloaded files under `tracks/` survived but were no longer
/// referenced, so they would be re-streamed and re-downloaded and
/// `sweep_stale` no longer knew about them.
///
/// Honouring `XDG_CACHE_HOME` was never the problem; *not noticing*
/// when a test had failed to set it was. A test that wants this has to
/// point it somewhere disposable, and gets told so rather than
/// silently writing to the user's install.
pub fn cache_dir() -> Result<PathBuf> {
    let dir = dirs::cache_dir().context("no cache dir found for this platform")?;
    let dir = dir.join("wim");
    #[cfg(test)]
    {
        let tmp = std::env::temp_dir();
        anyhow::ensure!(
            dir.starts_with(&tmp),
            "a test asked for the real cache dir ({}); point XDG_CACHE_HOME at a \
             directory under {} first, or this writes the user's play index",
            dir.display(),
            tmp.display()
        );
    }
    Ok(dir)
}

pub fn tracks_dir() -> Result<PathBuf> {
    Ok(cache_dir()?.join("tracks"))
}

/// One json line per tracked track. Same reasoning as `CatalogRow`
/// and the same measurement behind it: the write happens on every track
/// change, while startup reads it on a worker. At the size a
/// long-running install reaches, toml was ~5x slower to parse and
/// ~20x slower to render than this; see docs/startup-cache.md. One
/// corrupt line also costs one track rather than the whole history,
/// and it stays greppable.
const INDEX_FILE: &str = "index.ndjson";

/// The toml index `INDEX_FILE` replaced. Read once, when there is no
/// ndjson yet, so an existing install keeps its play history; never
/// written.
const LEGACY_INDEX_FILE: &str = "index.toml";

/// One `CacheEntry` as a row. Flat, with `Cow` fields, so a line with
/// no escapes borrows straight out of it and only the strings actually
/// kept are allocated.
#[derive(Debug, Serialize, Deserialize)]
struct PlayRow<'a> {
    #[serde(borrow)]
    id: Cow<'a, str>,
    #[serde(default)]
    play_count: u32,
    /// Day numbers out, either shape in; see `play_days_from_disk`.
    #[serde(default, deserialize_with = "play_days_from_disk")]
    play_days: Vec<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    last_played_secs: Option<u64>,
    #[serde(default, borrow, skip_serializing_if = "Option::is_none")]
    cached_path: Option<Cow<'a, str>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    cached_at_secs: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    download_source: Option<DownloadSource>,
}

fn index_body(index: &CacheIndex) -> String {
    let mut body = String::new();
    for (id, entry) in &index.entries {
        let row = PlayRow {
            id: Cow::Borrowed(id),
            play_count: entry.play_count,
            play_days: entry.play_days.clone(),
            last_played_secs: entry.last_played_secs,
            cached_path: entry.cached_path.as_deref().map(Cow::Borrowed),
            cached_at_secs: entry.cached_at_secs,
            download_source: entry.download_source.clone(),
        };
        if let Ok(line) = serde_json::to_string(&row) {
            body.push_str(&line);
            body.push('\n');
        }
    }
    body
}

/// One row as an entry, or `None` for a line that will not parse,
/// which costs that track and nothing else.
fn play_entry(line: &str) -> Option<(String, CacheEntry)> {
    let row: PlayRow = serde_json::from_str(line).ok()?;
    Some((
        row.id.into_owned(),
        CacheEntry {
            play_count: row.play_count,
            play_days: row.play_days,
            last_played_secs: row.last_played_secs,
            cached_path: row.cached_path.map(Cow::into_owned),
            cached_at_secs: row.cached_at_secs,
            download_source: row.download_source,
        },
    ))
}

/// The map is pre-sized off the line count rather than grown: a map
/// grown from empty rehashes every id it already holds a dozen times
/// on the way up. Startup parses it on a worker.
/// Worth 1.07-1.16x of the parse, measured by `play_index_parse_legs`.
///
/// Serial on purpose. Chunking the rows over a pool was measured and
/// rejected; `play_index_parse_legs` holds the candidate and the
/// numbers.
fn parse_index_body(raw: &str) -> CacheIndex {
    let lines: Vec<&str> = raw.lines().collect();
    let mut entries = HashMap::with_capacity(lines.len());
    entries.extend(lines.into_iter().filter_map(play_entry));
    CacheIndex { entries }
}

/// Where a given track id would land on disk once downloaded, with the
/// given extension (no leading dot).
pub fn track_path(id: &str, ext: &str) -> Result<PathBuf> {
    Ok(tracks_dir()?.join(format!("{}.{ext}", safe_file_stem(id))))
}

/// The playback directory sits in shared `/tmp` under a predictable name:
/// refuse one another user created, and keep ours unreadable by others.
fn secure_private_dir(dir: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        let meta = fs::symlink_metadata(dir).context("cannot inspect playback cache")?;
        // SAFETY: geteuid has no preconditions.
        let me = unsafe { libc::geteuid() };
        anyhow::ensure!(
            meta.is_dir() && meta.uid() == me,
            "temporary playback cache is not ours"
        );
        fs::set_permissions(dir, fs::Permissions::from_mode(0o700))
            .context("cannot restrict temporary playback cache")?;
    }
    #[cfg(not(unix))]
    let _ = dir;
    Ok(())
}

/// A server-supplied id is a file name here, so one holding a path
/// separator, NUL or a dot-only name must not be able to leave
/// `tracks/`. Ordinary ids pass through unchanged, so existing cache
/// files keep their names.
fn safe_file_stem(id: &str) -> String {
    let plain = !id.is_empty() && !id.chars().all(|c| c == '.') && !id.contains(['/', '\\', '\0']);
    if plain {
        return id.to_string();
    }
    use md5::{Digest, Md5};
    let digest = Md5::digest(id.as_bytes());
    let hex: String = digest.iter().map(|b| format!("{b:02x}")).collect();
    format!("id-{hex}")
}

/// The transient, uncompressed file handed to mpv for a compressed cache
/// entry. mpv needs a seekable ordinary media file; the durable copy remains
/// zstd-compressed in `tracks/`. These files live in the system temp area,
/// are scoped to this process, and are removed when the player exits.
pub fn playback_path(compressed: &Path) -> Result<PathBuf> {
    playback_path_with_budget(compressed, active_budget())
}

fn playback_path_with_budget(
    compressed: &Path,
    budget: Option<Arc<CacheBudget>>,
) -> Result<PathBuf> {
    if compressed.extension().is_none_or(|ext| ext != "zst") {
        return Ok(compressed.to_path_buf());
    }
    static NEXT_PART: AtomicU64 = AtomicU64::new(0);
    let file_name = compressed
        .file_name()
        .context("compressed cache path has no filename")?
        .to_string_lossy();
    let media_name = file_name
        .strip_suffix(".zst")
        .context("compressed cache path has an invalid suffix")?;
    let id = format!("wim-playback-{}", std::process::id());
    let dir = std::env::temp_dir().join(id);
    fs::create_dir_all(&dir).context("cannot create temporary playback cache")?;
    secure_private_dir(&dir)?;
    let dest = dir.join(media_name);
    if dest.is_file() {
        return Ok(dest);
    }
    let part = dir.join(format!(
        ".{}.{}.part",
        NEXT_PART.fetch_add(1, Ordering::Relaxed),
        media_name
    ));
    let mut reserved = 0u64;
    let result = (|| -> Result<bool> {
        let input = fs::File::open(compressed).context("cannot open compressed cache file")?;
        let mut decoder = zstd::stream::read::Decoder::new(input)
            .context("cannot decode compressed cache file")?;
        let mut output = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&part)
            .context("cannot create temporary playback file")?;
        let mut buf = [0u8; 64 * 1024];
        loop {
            let n = decoder
                .read(&mut buf)
                .context("cannot decompress cached audio for playback")?;
            if n == 0 {
                break;
            }
            if budget
                .as_ref()
                .is_some_and(|budget| !budget.reserve(n as u64))
            {
                anyhow::bail!("playback expansion exceeds the configured cache budget");
            }
            reserved = reserved.saturating_add(n as u64);
            output
                .write_all(&buf[..n])
                .context("cannot write temporary playback file")?;
        }
        output
            .flush()
            .context("cannot flush temporary playback file")?;
        drop(output);
        match fs::rename(&part, &dest) {
            Ok(()) => Ok(true),
            Err(_error) if dest.is_file() => Ok(false),
            Err(error) => Err(error).context("cannot finalize temporary playback file"),
        }
    })();
    let retain_reservation = matches!(&result, Ok(true))
        || fs::remove_file(&part).is_err_and(|error| error.kind() != std::io::ErrorKind::NotFound);
    if !retain_reservation {
        if let Some(budget) = &budget {
            budget.release(reserved);
        }
    }
    result?;
    Ok(dest)
}

/// Removes this process' transient, uncompressed playback copies. Called by
/// both Wim frontends during normal shutdown; a later startup also cleans up
/// stale process directories left by a forced termination.
pub fn cleanup_playback_files() {
    let dir = std::env::temp_dir().join(format!("wim-playback-{}", std::process::id()));
    let bytes = playback_tree_bytes(&dir);
    if fs::remove_dir_all(dir).is_ok() {
        if let Some(budget) = active_budget() {
            budget.release(bytes);
        }
    }
}

fn playback_tree_bytes(dir: &Path) -> u64 {
    let Ok(entries) = fs::read_dir(dir) else {
        return 0;
    };
    entries.flatten().fold(0u64, |sum, entry| {
        let Ok(kind) = entry.file_type() else {
            return sum;
        };
        if kind.is_dir() {
            sum.saturating_add(playback_tree_bytes(&entry.path()))
        } else if kind.is_file() && entry.path().extension().is_none_or(|ext| ext != "part") {
            sum.saturating_add(entry.metadata().map(|meta| meta.len()).unwrap_or(0))
        } else {
            sum
        }
    })
}

/// RAII lifetime for transient files handed to mpv. Startup cleanup runs
/// alongside the other cache work in `spawn_startup`; drop still handles this
/// process's files even when initialization or playback fails.
pub struct PlaybackFiles;

impl PlaybackFiles {
    pub fn new() -> Self {
        Self
    }
}

impl Default for PlaybackFiles {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for PlaybackFiles {
    fn drop(&mut self) {
        cleanup_playback_files();
    }
}

fn cleanup_stale_playback_files() {
    let root = std::env::temp_dir();
    let Ok(entries) = fs::read_dir(root) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(pid) = name
            .to_string_lossy()
            .strip_prefix("wim-playback-")
            .and_then(|pid| pid.parse::<u32>().ok())
        else {
            continue;
        };
        #[cfg(unix)]
        let alive = {
            // A PID that answers with EPERM still exists; only ESRCH is stale.
            let Ok(pid) = libc::pid_t::try_from(pid) else {
                continue;
            };
            if pid <= 0 {
                continue;
            }
            // SAFETY: signal 0 only checks that the pid exists; a positive pid.
            let status = unsafe { libc::kill(pid, 0) };
            status == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
        };
        #[cfg(not(unix))]
        let alive = pid == std::process::id();
        if !alive {
            let _ = fs::remove_dir_all(entry.path());
        }
    }
}

/// Load history and account for disk usage off the terminal thread. Callers
/// must wait for this result before playback or downloads: cleanup, eviction,
/// and the budget snapshot have to finish before any writer can reserve space.
pub fn spawn_startup(
    max_bytes: u64,
    eviction_days: Option<u64>,
) -> std::sync::mpsc::Receiver<CacheIndex> {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        cleanup_stale_playback_files();
        cleanup_playback_files();
        let mut index = CacheIndex::load();
        if eviction_days.is_some_and(|days| index.sweep_stale(days.saturating_mul(24 * 3600)) > 0) {
            let _ = index.save();
        }
        initialize_budget(max_bytes);
        let _ = tx.send(index);
    });
    rx
}

/// Bumped whenever the row shapes below change; a catalog written by
/// an older wim is dropped whole rather than half-parsed.
const CATALOG_FORMAT: u32 = 1;
static NEXT_INDEX_TEMP: AtomicU64 = AtomicU64::new(0);

/// Process-wide accounting for bytes in Wim-owned cache and playback files.
/// The budget is initialized once from disk at startup; writers reserve each
/// output chunk before it reaches disk, so overlapping batches share the same
/// headroom instead of each assuming the cache is empty.
pub struct CacheBudget {
    max_bytes: AtomicU64,
    used_bytes: AtomicU64,
}

impl CacheBudget {
    pub(crate) fn new(max_bytes: u64, used_bytes: u64) -> Self {
        Self {
            max_bytes: AtomicU64::new(max_bytes),
            used_bytes: AtomicU64::new(used_bytes),
        }
    }

    pub(crate) fn reserve(&self, bytes: u64) -> bool {
        let mut used = self.used_bytes.load(Ordering::Acquire);
        loop {
            let Some(next) = used
                .checked_add(bytes)
                .filter(|next| *next <= self.max_bytes.load(Ordering::Acquire))
            else {
                return false;
            };
            match self.used_bytes.compare_exchange_weak(
                used,
                next,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => return true,
                Err(actual) => used = actual,
            }
        }
    }

    pub(crate) fn release(&self, bytes: u64) {
        let mut used = self.used_bytes.load(Ordering::Acquire);
        loop {
            let next = used.saturating_sub(bytes);
            match self.used_bytes.compare_exchange_weak(
                used,
                next,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => return,
                Err(actual) => used = actual,
            }
        }
    }

    pub(crate) fn set_limit(&self, max_bytes: u64) {
        self.max_bytes.store(max_bytes, Ordering::Release);
    }

    #[cfg(test)]
    pub(crate) fn used_bytes(&self) -> u64 {
        self.used_bytes.load(Ordering::Acquire)
    }
}

static ACTIVE_BUDGET: OnceLock<Arc<CacheBudget>> = OnceLock::new();

/// Install the configured budget after stale playback directories have been
/// cleaned. Existing managed files count against the same limit as new writes.
pub fn initialize_budget(max_bytes: u64) {
    let budget =
        ACTIVE_BUDGET.get_or_init(|| Arc::new(CacheBudget::new(max_bytes, managed_disk_bytes())));
    budget.set_limit(max_bytes);
}

fn active_budget() -> Option<Arc<CacheBudget>> {
    ACTIVE_BUDGET.get().cloned()
}

pub(crate) fn budget_for_download(max_bytes: u64) -> Arc<CacheBudget> {
    if let Some(budget) = ACTIVE_BUDGET.get() {
        budget.set_limit(max_bytes);
        return budget.clone();
    }
    Arc::new(CacheBudget::new(max_bytes, managed_disk_bytes()))
}

fn managed_disk_bytes() -> u64 {
    fn tree_bytes(path: &Path) -> u64 {
        let Ok(entries) = fs::read_dir(path) else {
            return 0;
        };
        entries.flatten().fold(0u64, |sum, entry| {
            let Ok(meta) = entry.file_type() else {
                return sum;
            };
            if meta.is_dir() {
                sum.saturating_add(tree_bytes(&entry.path()))
            } else if meta.is_file() {
                sum.saturating_add(entry.metadata().map(|metadata| metadata.len()).unwrap_or(0))
            } else {
                sum
            }
        })
    }

    let mut used = tracks_dir().map(|dir| tree_bytes(&dir)).unwrap_or(0);
    if let Ok(entries) = fs::read_dir(std::env::temp_dir()) {
        for entry in entries.flatten() {
            if entry
                .file_name()
                .to_string_lossy()
                .starts_with("wim-playback-")
            {
                used = used.saturating_add(tree_bytes(&entry.path()));
            }
        }
    }
    used
}

/// One server's eagerly indexed catalog, exactly what `getArtists`,
/// `getAlbumList2` and `getPlaylists` last answered. Songs are
/// deliberately absent: nothing indexes them up front (see
/// `subsonic::search3`), so there is no authoritative list to persist.
#[derive(Debug, Default, Clone)]
pub struct ServerCatalog {
    pub artists: Vec<Artist>,
    pub albums: Vec<Album>,
    pub playlists: Vec<Playlist>,
}

impl ServerCatalog {
    pub fn is_empty(&self) -> bool {
        self.artists.is_empty() && self.albums.is_empty() && self.playlists.is_empty()
    }
}

/// First line of a catalog file. `id` is a second, independently
/// salted digest of the server key, so a digest collision in the
/// filename still can't serve one server's catalog as another's
/// without putting the url or username in the file; `sum` lets a sync
/// whose catalog hasn't changed skip rewriting the whole thing.
#[derive(Debug, Serialize, Deserialize)]
struct CatalogHeader {
    v: u32,
    id: String,
    sum: String,
    at: u64,
}

/// Which entity a row describes. An unknown value fails the row's
/// deserialization, which costs that row and nothing else.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
enum RowKind {
    Artist,
    Album,
    Playlist,
}

/// One catalog row. Newline-delimited json rather than one json
/// document: it parses a line at a time with no whole-file value tree,
/// a single corrupt line costs one entry instead of the catalog, and
/// it stays greppable. See docs/startup-cache.md for the format
/// comparison that picked it.
///
/// One flat struct with a `k` discriminant rather than the obvious
/// `#[serde(tag = "k")]` enum: identical bytes on disk, a fifth off
/// the parse. An internally-tagged enum cannot be deserialized in one
/// pass, so serde buffers every row into its own `Content` tree
/// (allocating per field) before it can know which variant to build.
/// `Cow` fields then let a row with no escapes borrow straight out of
/// the line, so only the strings actually kept are allocated. See
/// `catalog_parse_scaling` for the numbers and
/// docs/startup-cache.md for why this is where the parse cost was
/// taken from rather than fanning it out over threads.
#[derive(Debug, Serialize, Deserialize)]
struct CatalogRow<'a> {
    k: RowKind,
    #[serde(borrow)]
    id: Cow<'a, str>,
    #[serde(borrow)]
    name: Cow<'a, str>,
    /// Album rows only. Absent on an artist or playlist row rather
    /// than written empty, which is what the format always did.
    #[serde(default, borrow, skip_serializing_if = "Option::is_none")]
    artist: Option<Cow<'a, str>>,
    /// The credited artist's own id, when the server told us
    /// (`subsonic::IdIndex`). Optional and skipped when absent, so a
    /// catalog written before this field existed still loads and one
    /// written by a server that reports no artist ids is byte-
    /// identical to what it used to be.
    #[serde(default, borrow, skip_serializing_if = "Option::is_none")]
    artist_id: Option<Cow<'a, str>>,
}

fn catalogs_dir() -> Result<PathBuf> {
    Ok(cache_dir()?.join("catalogs"))
}

/// Which server a catalog belongs to: normalized base url plus
/// username, since two accounts on one server see different playlists.
fn catalog_key(base_url: &str, username: &str) -> String {
    format!("{username}@{base_url}")
}

/// Hex md5, used only to turn a server key into a fixed-length
/// filesystem-safe name; nothing here depends on it being a secure
/// digest (the key itself is stored in the file's header).
fn digest_hex(data: &str) -> String {
    let mut hasher = Md5::new();
    hasher.update(data.as_bytes());
    format!("{:x}", hasher.finalize())
}

fn catalog_path(dir: &Path, key: &str) -> PathBuf {
    dir.join(format!("{}.ndjson", digest_hex(key)))
}

/// The header's own check digest. Salted differently from the
/// filename's so the two can't collide together, and a digest rather
/// than the key itself because a cache file has no business holding a
/// server url and username in cleartext.
fn catalog_id(key: &str) -> String {
    digest_hex(&format!("wim-catalog-id\n{key}"))
}

/// The body half of a catalog file, one json row per line. Artists
/// first, then albums, then playlists; the order is just the write
/// order, nothing reading it depends on it.
///
/// `artist_of_album` resolves each album's credited artist id, which
/// `subsonic::Album` does not carry; the live caller reads it off the
/// client's id index.
fn catalog_body(
    artists: &[Artist],
    albums: &[Album],
    playlists: &[Playlist],
    artist_of_album: &dyn Fn(&str) -> Option<String>,
) -> String {
    let rows = artists
        .iter()
        .map(|a| CatalogRow {
            k: RowKind::Artist,
            id: Cow::Borrowed(&a.id),
            name: Cow::Borrowed(&a.name),
            artist: None,
            artist_id: None,
        })
        .chain(albums.iter().map(|a| CatalogRow {
            k: RowKind::Album,
            id: Cow::Borrowed(&a.id),
            name: Cow::Borrowed(&a.name),
            artist: Some(Cow::Borrowed(&a.artist)),
            artist_id: artist_of_album(&a.id).map(Cow::Owned),
        }))
        .chain(playlists.iter().map(|p| CatalogRow {
            k: RowKind::Playlist,
            id: Cow::Borrowed(&p.id),
            name: Cow::Borrowed(&p.name),
            artist: None,
            artist_id: None,
        }));
    let mut body = String::new();
    for row in rows {
        if let Ok(line) = serde_json::to_string(&row) {
            body.push_str(&line);
            body.push('\n');
        }
    }
    body
}

/// The `sum` an existing catalog file was written with, reading only
/// its first line; what makes "the catalog hasn't changed" cost one
/// small read instead of a multi-megabyte rewrite every sync.
fn stored_sum(path: &Path) -> Option<String> {
    let file = fs::File::open(path).ok()?;
    let mut line = String::new();
    BufReader::new(file).read_line(&mut line).ok()?;
    let header: CatalogHeader = serde_json::from_str(line.trim_end()).ok()?;
    (header.v == CATALOG_FORMAT).then_some(header.sum)
}

/// Persists one server's catalog under the XDG cache dir; see
/// `save_catalog_in` for what the write itself guarantees. Nothing
/// identifying goes in the file: no url, no username, and obviously no
/// password, just the library rows and a check digest.
pub fn save_catalog(
    client: &Client,
    artists: &[Artist],
    albums: &[Album],
    playlists: &[Playlist],
) -> Result<()> {
    save_catalog_in(
        &catalogs_dir()?,
        &catalog_key(client.base_url(), client.username()),
        artists,
        albums,
        playlists,
        &|album_id| client.artist_of_album(album_id),
    )?;
    Ok(())
}

/// Best-effort load for one configured server, same leniency as
/// `CacheIndex::load`: a missing, truncated, foreign or outdated file
/// just means this server starts cold and the first background sync
/// fills it in.
///
/// The second half is the album-to-artist links the file carries,
/// which belong in the client's id index rather than on the rows
/// (`subsonic::Album` cannot be widened; see docs/identity.md). Empty
/// for a catalog written before the field existed, which is exactly
/// the name-fallback case.
pub fn load_catalog(nav: &NavidromeConfig) -> Option<(ServerCatalog, Vec<(String, String)>)> {
    load_catalog_in(&catalogs_dir().ok()?, &server_key(nav))
}

/// Deletes every persisted catalog that doesn't belong to one of
/// `active`, which is how a removed server's (or a renamed url's)
/// catalog stops taking up disk. Deliberately not age-based: a
/// configured server's catalog is exactly what makes a cold start
/// instant, including after a week offline. Returns how many files
/// went.
pub fn sweep_foreign_catalogs(active: &[NavidromeConfig]) -> usize {
    let keep: Vec<String> = active
        .iter()
        .map(|nav| digest_hex(&server_key(nav)))
        .collect();
    catalogs_dir()
        .map(|dir| sweep_foreign_catalogs_in(&dir, &keep))
        .unwrap_or(0)
}

/// The catalog key for a configured server. Goes through
/// `subsonic::normalize_url` so the key a load computes from config
/// matches the one a save computed from the live client's base url.
fn server_key(nav: &NavidromeConfig) -> String {
    catalog_key(&subsonic::normalize_url(&nav.url), &nav.username)
}

/// Skips the write entirely when what's on disk already matches
/// (reported as `false`): an auto-sync whose catalog is identical,
/// which is almost all of them, would otherwise rewrite megabytes
/// every 45 seconds. Written to a temp file and renamed, so an
/// interrupted write can't leave a truncated catalog behind, and
/// created 0600: it holds no credentials, but it does map out the
/// user's library and which server it came from.
fn save_catalog_in(
    dir: &Path,
    key: &str,
    artists: &[Artist],
    albums: &[Album],
    playlists: &[Playlist],
    artist_of_album: &dyn Fn(&str) -> Option<String>,
) -> Result<bool> {
    let path = catalog_path(dir, key);
    let body = catalog_body(artists, albums, playlists, artist_of_album);
    let sum = digest_hex(&body);
    if stored_sum(&path).as_deref() == Some(sum.as_str()) {
        return Ok(false);
    }
    fs::create_dir_all(dir)?;
    restrict_dir_to_owner(dir)?;
    let header = serde_json::to_string(&CatalogHeader {
        v: CATALOG_FORMAT,
        id: catalog_id(key),
        sum,
        at: now_secs(),
    })?;
    let tmp = path.with_extension("tmp");
    write_owner_only(&tmp, &format!("{header}\n{body}"))
        .with_context(|| format!("writing {}", tmp.display()))?;
    fs::rename(&tmp, &path).with_context(|| format!("renaming into {}", path.display()))?;
    Ok(true)
}

/// A parsed catalog file: the rows, plus the album-to-artist links
/// that belong in the client's id index rather than on the rows (see
/// docs/identity.md).
type CatalogRows = (ServerCatalog, Vec<(String, String)>);

/// One row per line, in file order. **Order is load-bearing**: the
/// corpus indexes in insertion order and `search`'s stable sort breaks
/// ties on it, so a reordered catalog is a reordered result list.
fn parse_rows<'a>(lines: impl Iterator<Item = &'a str>) -> CatalogRows {
    let mut catalog = ServerCatalog::default();
    let mut album_artists = Vec::new();
    for row in lines.filter_map(|line| serde_json::from_str::<CatalogRow>(line).ok()) {
        let (id, name) = (row.id.into_owned(), row.name.into_owned());
        match row.k {
            RowKind::Artist => catalog.artists.push(Artist { id, name }),
            RowKind::Album => {
                if let Some(artist_id) = row.artist_id {
                    album_artists.push((id.clone(), artist_id.into_owned()));
                }
                catalog.albums.push(Album {
                    id,
                    name,
                    // an album row has always carried its credited
                    // artist; a row missing it is read as untagged
                    // rather than thrown away.
                    artist: row.artist.unwrap_or_default().into_owned(),
                })
            }
            RowKind::Playlist => catalog.playlists.push(Playlist { id, name }),
        }
    }
    (catalog, album_artists)
}

fn load_catalog_in(dir: &Path, key: &str) -> Option<CatalogRows> {
    let raw = fs::read_to_string(catalog_path(dir, key)).ok()?;
    let mut split = raw.splitn(2, '\n');
    let header: CatalogHeader = serde_json::from_str(split.next()?).ok()?;
    if header.v != CATALOG_FORMAT || header.id != catalog_id(key) {
        return None;
    }
    Some(parse_rows(split.next().unwrap_or("").lines()))
}

fn sweep_foreign_catalogs_in(dir: &Path, keep: &[String]) -> usize {
    let Ok(entries) = fs::read_dir(dir) else {
        return 0;
    };
    let mut removed = 0;
    for entry in entries.flatten() {
        let path = entry.path();
        let stem = path.file_stem().map(|s| s.to_string_lossy().into_owned());
        if stem.is_some_and(|stem| keep.contains(&stem)) {
            continue;
        }
        if fs::remove_file(&path).is_ok() {
            removed += 1;
        }
    }
    removed
}

/// 0600 from the moment the file exists, rather than writing it world-
/// readable and tightening afterwards.
#[cfg(unix)]
fn write_owner_only(path: &Path, contents: &str) -> Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)?;
    file.write_all(contents.as_bytes())?;
    Ok(())
}

#[cfg(not(unix))]
fn write_owner_only(path: &Path, contents: &str) -> Result<()> {
    fs::write(path, contents)?;
    Ok(())
}

#[cfg(unix)]
fn restrict_dir_to_owner(dir: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(dir, fs::Permissions::from_mode(0o700))?;
    Ok(())
}

#[cfg(not(unix))]
fn restrict_dir_to_owner(_dir: &Path) -> Result<()> {
    Ok(())
}

/// Best-effort, same leniency as `load_catalog`: whichever of the two
/// files is there, and a fresh index if neither parses. The ndjson
/// wins when both exist, since it is the only one `save_in` writes.
fn load_in(dir: &Path) -> CacheIndex {
    match fs::read_to_string(dir.join(INDEX_FILE)) {
        Ok(raw) => parse_index_body(&raw),
        Err(_) => fs::read_to_string(dir.join(LEGACY_INDEX_FILE))
            .ok()
            .and_then(|raw| toml::from_str(&raw).ok())
            .unwrap_or_default(),
    }
}

fn save_in(dir: &Path, index: &CacheIndex) -> Result<()> {
    fs::create_dir_all(dir)?;
    restrict_dir_to_owner(dir)?;
    let path = dir.join(INDEX_FILE);
    let temp = dir.join(format!(
        ".{INDEX_FILE}.{}.{}.tmp",
        std::process::id(),
        NEXT_INDEX_TEMP.fetch_add(1, Ordering::Relaxed)
    ));
    let result = (|| -> Result<()> {
        write_new_owner_only(&temp, &index_body(index))
            .with_context(|| format!("writing {}", temp.display()))?;
        fs::rename(&temp, &path).with_context(|| format!("replacing {}", path.display()))?;
        #[cfg(unix)]
        fs::File::open(dir)?.sync_all()?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temp);
    }
    result?;
    // the toml index held a second copy of the same history and is
    // dead the moment the line above lands; a failure to unlink it is
    // nothing but a stale file in a cache dir, so it is not reported.
    let _ = fs::remove_file(dir.join(LEGACY_INDEX_FILE));
    Ok(())
}

fn write_new_owner_only(path: &Path, contents: &str) -> Result<()> {
    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(path)?;
    file.write_all(contents.as_bytes())?;
    file.sync_all()?;
    Ok(())
}

impl CacheIndex {
    /// Best-effort load; a missing/corrupt index just means starting
    /// fresh (nothing known to be cached yet), not an error worth
    /// surfacing, same leniency as `persistence::load`.
    pub fn load() -> Self {
        cache_dir().map(|dir| load_in(&dir)).unwrap_or_default()
    }

    pub fn save(&self) -> Result<()> {
        save_in(&cache_dir()?, self)
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
        // a day label that will not parse records the play but no day:
        // every caller passes `today()`, so this is the hand-edited
        // case, and inventing a day for it would inflate the threshold.
        if let Some(today) = day_number(today).and_then(|d| i32::try_from(d).ok()) {
            // migration for an index written before play days existed:
            // the one earlier day we can still recover is whatever
            // `last_played_secs` points at, so a track played
            // yesterday and played again now counts as the two days it
            // really was, instead of silently restarting from zero.
            if entry.play_days.is_empty() {
                if let Some(prev) = entry.last_played_secs {
                    entry.play_days.push(day_number_of(prev));
                }
            }
            if entry.play_days.last() != Some(&today) {
                entry.play_days.retain(|&d| d != today);
                entry.play_days.push(today);
            }
            if entry.play_days.len() > MAX_TRACKED_DAYS {
                let excess = entry.play_days.len() - MAX_TRACKED_DAYS;
                entry.play_days.drain(..excess);
            }
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

    /// Every tracked id with its entry, in no particular order. What
    /// lets the suggestion engine walk the history instead of asking
    /// of every track in the library whether it has any: a played
    /// library is a fraction of a catalog, and this is one hash per
    /// recorded track rather than one per catalog track. See
    /// `suggest::cost::what_the_history_lookups_cost`.
    pub fn entries(&self) -> impl Iterator<Item = (&str, &CacheEntry)> {
        self.entries.iter().map(|(id, entry)| (id.as_str(), entry))
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

    /// Return the durable cache path, migrating an older uncompressed cached
    /// file to zstd atomically. If compression fails (for example because the
    /// disk is full), the intact original remains usable and is returned.
    pub fn compressed_cached_path(&mut self, id: &str) -> Result<Option<PathBuf>> {
        let Some(source) = self.cached_path(id) else {
            return Ok(None);
        };
        if source.extension().is_some_and(|ext| ext == "zst") {
            return Ok(Some(source));
        }
        let mut dest_name = source
            .file_name()
            .context("cached track path has no filename")?
            .to_os_string();
        dest_name.push(".zst");
        let dest = source.with_file_name(dest_name);
        let part = dest.with_extension("zst.part");
        let result = (|| -> Result<()> {
            let input = fs::File::open(&source).context("cannot open cached track")?;
            let output = fs::File::create(&part).context("cannot create compressed cache file")?;
            let mut encoder = zstd::Encoder::new(output, 3).context("cannot start zstd encoder")?;
            std::io::copy(&mut std::io::BufReader::new(input), &mut encoder)
                .context("cannot compress cached track")?;
            encoder
                .finish()
                .context("cannot finish compressed cache file")?;
            fs::rename(&part, &dest).context("cannot finalize compressed cache file")?;
            Ok(())
        })();
        if let Err(error) = result {
            let _ = fs::remove_file(&part);
            return Err(error);
        }
        // The caller durably saves the updated index before retiring the
        // original. Keeping both files here makes migration recoverable if
        // the subsequent index write fails.
        self.mark_cached(id, dest.clone());
        Ok(Some(dest))
    }

    pub fn mark_cached(&mut self, id: &str, path: PathBuf) {
        let entry = self.entries.entry(id.to_string()).or_default();
        entry.cached_path = Some(path.to_string_lossy().into_owned());
        entry.cached_at_secs = Some(now_secs());
    }

    /// Like `plan_budget_evictions`, but reserve budget for transient bytes
    /// that are managed by Wim but are not indexed cache entries (expanded
    /// playback copies and active transfer `.part` files). These bytes are
    /// counted, never selected for deletion.
    pub(crate) fn plan_budget_evictions_with_extra(
        &mut self,
        max_bytes: u64,
        protected: &str,
        extra_bytes: u64,
    ) -> Vec<PathBuf> {
        let mut total = 0u64;
        let mut candidates = Vec::new();
        for (id, entry) in &self.entries {
            if let Some(path) = entry.cached_path.as_deref() {
                if let Ok(meta) = fs::metadata(path) {
                    total = total.saturating_add(meta.len());
                    if id != protected {
                        candidates.push((
                            entry.cached_at_secs.unwrap_or(0),
                            id.clone(),
                            PathBuf::from(path),
                            meta.len(),
                        ));
                    }
                }
            }
        }
        total = total.saturating_add(extra_bytes);
        candidates.sort_by_key(|(at, _, _, _)| *at);
        let mut removed = Vec::new();
        for (_, id, path, size) in candidates {
            if total <= max_bytes {
                break;
            }
            total = total.saturating_sub(size);
            if let Some(entry) = self.entries.get_mut(&id) {
                entry.cached_path = None;
                entry.cached_at_secs = None;
            }
            removed.push(path);
        }
        removed
    }

    /// Current transient Wim-owned bytes: this process' expanded playback
    /// files and cache transfer `.part` files. Other processes' playback
    /// directories are intentionally not traversed or removed.
    pub(crate) fn transient_managed_bytes(&self) -> u64 {
        fn sum_files(dir: &Path, partial_only: bool) -> u64 {
            let Ok(entries) = fs::read_dir(dir) else {
                return 0;
            };
            entries.flatten().fold(0u64, |total, entry| {
                let path = entry.path();
                let Ok(meta) = entry.metadata() else {
                    return total;
                };
                if meta.is_dir() {
                    total.saturating_add(sum_files(&path, partial_only))
                } else if meta.is_file()
                    && (!partial_only || path.extension().is_some_and(|ext| ext == "part"))
                {
                    total.saturating_add(meta.len())
                } else {
                    total
                }
            })
        }
        let playback = std::env::temp_dir().join(format!("wim-playback-{}", std::process::id()));
        let cache_parts = tracks_dir().map(|dir| sum_files(&dir, true)).unwrap_or(0);
        sum_files(&playback, false).saturating_add(cache_parts)
    }

    /// Path currently indexed for `id`, whether or not it still exists.
    /// Used by download handoff to retire an old copy only after the new
    /// index has been persisted successfully.
    pub(crate) fn recorded_cached_path(&self, id: &str) -> Option<PathBuf> {
        self.entries
            .get(id)
            .and_then(|entry| entry.cached_path.as_deref())
            .map(PathBuf::from)
    }

    /// Remember how a remote song can be fetched again, without retaining
    /// credentials, signed URLs, or transient stream links.
    pub fn remember_download(&mut self, id: &str, source: DownloadSource) {
        self.entries
            .entry(id.to_string())
            .or_default()
            .download_source = Some(source);
    }

    /// The plaintext redownload manifest: stable IDs, server identity, and
    /// human-readable labels. Entries survive stale-cache eviction.
    pub fn download_manifest(&self) -> impl Iterator<Item = (&str, &DownloadSource)> {
        self.entries.iter().filter_map(|(id, entry)| {
            entry
                .download_source
                .as_ref()
                .map(|source| (id.as_str(), source))
        })
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
                match fs::metadata(path) {
                    Ok(meta) if fs::remove_file(path).is_ok() => {
                        if let Some(budget) = active_budget() {
                            budget.release(meta.len());
                        }
                        entry.cached_path = None;
                        entry.cached_at_secs = None;
                        evicted += 1;
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                        entry.cached_path = None;
                        entry.cached_at_secs = None;
                    }
                    _ => {}
                }
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
    fn the_play_index_survives_a_round_trip_through_the_disk() {
        let dir = scratch_dir("playindex");
        let mut idx = CacheIndex::default();
        idx.record_play("t1", "2026-01-01");
        idx.record_play("t1", "2026-01-03");
        idx.mark_cached("t1", PathBuf::from("/c/wim/tracks/t1.mp3"));
        idx.record_play("t2", "2026-01-02");

        save_in(&dir, &idx).unwrap();
        let back = load_in(&dir);
        assert_eq!(back.entries.len(), 2);
        assert_eq!(back.play_count("t1"), 2);
        assert_eq!(back.days_played("t1"), 2);
        assert_eq!(
            back.entry("t1").unwrap().play_days,
            [20_454, 20_456],
            "the day list is what the download threshold counts, and it is ordered \
             (2026-01-01 and 2026-01-03 as day numbers)"
        );
        assert_eq!(
            back.entry("t1").unwrap().cached_path.as_deref(),
            Some("/c/wim/tracks/t1.mp3")
        );
        assert_eq!(
            back.entry("t1").unwrap().last_played_secs,
            idx.entry("t1").unwrap().last_played_secs
        );
        assert_eq!(back.play_count("t2"), 1);
        assert!(back.entry("t2").unwrap().cached_path.is_none(), "no blanks");
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn index_replacement_uses_private_temp_and_leaves_no_partial_file() {
        let dir = scratch_dir("playindex-atomic");
        let mut index = CacheIndex::default();
        index.record_play("song", "2026-01-01");
        save_in(&dir, &index).unwrap();
        index.record_play("song", "2026-01-02");
        save_in(&dir, &index).unwrap();

        assert_eq!(load_in(&dir).days_played("song"), 2);
        let files = fs::read_dir(&dir)
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect::<Vec<_>>();
        assert_eq!(files, [std::ffi::OsString::from(INDEX_FILE)]);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(dir.join(INDEX_FILE))
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
        }
        fs::remove_dir_all(&dir).ok();
    }

    /// The whole point of the migration: someone who has been using
    /// wim already has their play history in `index.toml`, and the
    /// auto-download threshold is counted off it. Losing it would
    /// silently reset everyone's history to zero.
    #[test]
    fn a_toml_index_from_before_the_format_change_is_still_read() {
        let dir = scratch_dir("migrate");
        fs::write(
            dir.join(LEGACY_INDEX_FILE),
            "[entries.t1]\nplay_count = 7\nplay_days = [\"2026-01-01\", \"2026-01-05\"]\n\
             last_played_secs = 1767225600\ncached_path = \"/c/wim/tracks/t1.mp3\"\n",
        )
        .unwrap();

        let back = load_in(&dir);
        assert_eq!(back.play_count("t1"), 7, "the old history has to survive");
        assert_eq!(back.days_played("t1"), 2);
        assert_eq!(
            back.entry("t1").unwrap().cached_path.as_deref(),
            Some("/c/wim/tracks/t1.mp3")
        );

        // and the next save moves it over, leaving no second copy of
        // the whole history behind.
        save_in(&dir, &back).unwrap();
        assert!(dir.join(INDEX_FILE).is_file());
        assert!(
            !dir.join(LEGACY_INDEX_FILE).exists(),
            "a migrated index must not leave the old one on disk"
        );
        assert_eq!(load_in(&dir).play_count("t1"), 7);
        fs::remove_dir_all(&dir).ok();
    }

    /// The reason for one row per line here too: a truncated write or
    /// a hand-edit costs one track, not every play count in the
    /// install.
    #[test]
    fn a_corrupt_play_row_costs_one_track_not_the_history() {
        let dir = scratch_dir("playcorrupt");
        let mut idx = CacheIndex::default();
        for id in ["t1", "t2", "t3"] {
            idx.record_play(id, "2026-01-01");
        }
        save_in(&dir, &idx).unwrap();

        let path = dir.join(INDEX_FILE);
        let mut lines: Vec<String> = fs::read_to_string(&path)
            .unwrap()
            .lines()
            .map(str::to_string)
            .collect();
        lines[1] = "{\"id\":\"t2\",\"play_c".to_string(); // truncated mid-row
        fs::write(&path, lines.join("\n")).unwrap();

        assert_eq!(load_in(&dir).entries.len(), 2);
        fs::remove_dir_all(&dir).ok();
    }

    /// The row shape has to read back exactly what the toml shape
    /// held, field for field, or the format changed under every index
    /// already on disk. Same oracle idea as
    /// `the_flat_row_reads_exactly_what_the_tagged_row_did`.
    #[test]
    fn the_row_shape_reads_exactly_what_the_toml_shape_did() {
        let mut idx = CacheIndex::default();
        idx.record_play("t1", "2026-01-01");
        idx.record_play("t1", "2026-01-02");
        idx.mark_cached("t1", PathBuf::from("/c/t1.mp3"));
        idx.record_play("t2", "2026-01-02");
        idx.entries.get_mut("t2").unwrap().last_played_secs = None;

        let via_rows = parse_index_body(&index_body(&idx));
        let via_toml: CacheIndex = toml::from_str(&toml::to_string_pretty(&idx).unwrap()).unwrap();
        assert_eq!(via_rows.entries.len(), via_toml.entries.len());
        for (id, want) in &via_toml.entries {
            let got = via_rows.entries.get(id).expect("every track must survive");
            assert_eq!(got.play_count, want.play_count, "{id}");
            assert_eq!(got.play_days, want.play_days, "{id}");
            assert_eq!(got.last_played_secs, want.last_played_secs, "{id}");
            assert_eq!(got.cached_path, want.cached_path, "{id}");
            assert_eq!(got.cached_at_secs, want.cached_at_secs, "{id}");
        }
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
    fn legacy_cached_tracks_migrate_atomically_and_remain_playable() {
        let dir = scratch_dir("cache-migrate");
        let source = dir.join("legacy.mp3");
        let bytes = b"lossless cache migration fixture".repeat(4096);
        fs::write(&source, &bytes).unwrap();
        let mut index = CacheIndex::default();
        index.mark_cached("migrate", source.clone());

        let compressed = index.compressed_cached_path("migrate").unwrap().unwrap();
        assert_eq!(
            compressed.extension().and_then(|ext| ext.to_str()),
            Some("zst")
        );
        assert!(source.exists(), "migration leaves deletion to its caller");
        assert_eq!(index.cached_path("migrate"), Some(compressed.clone()));

        save_in(&dir, &index).unwrap();
        let restored = load_in(&dir);
        assert_eq!(restored.cached_path("migrate"), Some(compressed.clone()));
        fs::remove_file(&source).unwrap();

        let expanded = playback_path(&compressed).unwrap();
        assert_eq!(fs::read(&expanded).unwrap(), bytes);
        assert_eq!(playback_path(&compressed).unwrap(), expanded);
        fs::remove_file(compressed).unwrap();
        cleanup_playback_files();
        fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn playback_expansion_reserves_budget_and_releases_it_on_refusal() {
        let dir = scratch_dir("playback-budget");
        let tag = dir.file_name().unwrap().to_string_lossy().into_owned();
        let compressed = dir.join(format!("{tag}.mp3.zst"));
        let bytes = b"audio payload".repeat(4096);
        let file = fs::File::create(&compressed).unwrap();
        let mut encoder = zstd::Encoder::new(file, 3).unwrap();
        encoder.write_all(&bytes).unwrap();
        encoder.finish().unwrap();
        let budget = Arc::new(CacheBudget::new(8, 0));

        let result = playback_path_with_budget(&compressed, Some(budget.clone()));

        assert!(result.is_err());
        assert_eq!(
            budget.used_bytes(),
            0,
            "partial expansion reservation released"
        );
        let playback_dir =
            std::env::temp_dir().join(format!("wim-playback-{}", std::process::id()));
        if let Ok(entries) = fs::read_dir(playback_dir) {
            assert!(entries
                .flatten()
                .all(|entry| { !entry.file_name().to_string_lossy().contains(&tag) }));
        }
        fs::remove_file(compressed).unwrap();
        fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn plaintext_redownload_manifest_round_trips_with_safe_server_identity() {
        let mut index = CacheIndex::default();
        index.remember_download(
            "song-17",
            DownloadSource {
                server_url: subsonic::safe_server_identity(
                    "https://listener:pw@music.example/base/?token=secret&key=credential#private",
                ),
                username: "listener".into(),
                title: "A song".into(),
                artist: "An artist".into(),
            },
        );
        let body = index_body(&index);
        assert!(body.contains("A song"));
        assert!(body.contains("https://music.example/base"));
        assert!(!body.contains("listener:pw"));
        assert!(!body.contains("token"));
        assert!(!body.contains("secret"));
        assert!(!body.contains("private"));
        assert!(!body.contains("password"));
        assert!(!body.contains("token"));
        assert!(!body.contains("salt"));

        let restored = parse_index_body(&body);
        let (id, source) = restored.download_manifest().next().unwrap();
        assert_eq!(id, "song-17");
        assert_eq!(source.username, "listener");
        assert_eq!(source.title, "A song");
        assert_eq!(source.server_url, "https://music.example/base");
    }

    #[test]
    fn uncompressed_paths_remain_directly_playable() {
        let path = PathBuf::from("/music/already-local.mp3");
        assert_eq!(playback_path(&path).unwrap(), path);
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
    fn manifest_survives_disk_roundtrip_and_cache_eviction() {
        let dir = scratch_dir("manifest-eviction");
        let path = dir.join("song.zst");
        fs::write(&path, b"cached").unwrap();
        let mut idx = CacheIndex::default();
        idx.mark_cached("song", path.clone());
        idx.remember_download(
            "song",
            DownloadSource {
                server_url: "https://music.example/base".into(),
                username: "listener".into(),
                title: "A song".into(),
                artist: "An artist".into(),
            },
        );
        idx.entries.get_mut("song").unwrap().last_played_secs = Some(0);
        save_in(&dir, &idx).unwrap();
        let mut loaded = load_in(&dir);
        assert_eq!(loaded.sweep_stale(1), 1);
        save_in(&dir, &loaded).unwrap();
        let loaded = load_in(&dir);
        assert!(loaded.cached_path("song").is_none());
        assert_eq!(loaded.download_manifest().next().unwrap().0, "song");
        fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn budget_plan_evicts_oldest_compressed_cache_and_preserves_protected_file() {
        let dir = scratch_dir("budget");
        let old = dir.join("old.opus.zst");
        let new = dir.join("new.opus.zst");
        fs::write(&old, [1u8; 12]).unwrap();
        fs::write(&new, [2u8; 8]).unwrap();
        let mut idx = CacheIndex::default();
        idx.mark_cached("old", old.clone());
        idx.mark_cached("new", new.clone());
        idx.entries.get_mut("old").unwrap().cached_at_secs = Some(1);
        idx.entries.get_mut("new").unwrap().cached_at_secs = Some(2);
        let evicted = idx.plan_budget_evictions_with_extra(8, "new", 0);
        assert_eq!(evicted.as_slice(), std::slice::from_ref(&old));
        assert!(idx.cached_path("old").is_none());
        assert_eq!(idx.cached_path("new"), Some(new));
        assert!(old.is_file(), "planning never unlinks before persistence");
        fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn budget_plan_reserves_transient_bytes_without_deleting_them() {
        let dir = scratch_dir("budget-transient");
        let old = dir.join("old.zst");
        let current = dir.join("current.zst");
        fs::write(&old, b"1234").unwrap();
        fs::write(&current, b"5678").unwrap();
        let mut idx = CacheIndex::default();
        idx.mark_cached("old", old.clone());
        idx.entries.get_mut("old").unwrap().cached_at_secs = Some(1);
        idx.mark_cached("current", current.clone());
        let evicted = idx.plan_budget_evictions_with_extra(5, "current", 2);
        assert_eq!(evicted, vec![old.clone()]);
        assert!(old.exists(), "planner must not unlink files itself");
        assert!(current.exists());
        fs::remove_dir_all(dir).ok();
    }

    /// Its own temp dir per test, so nothing here ever reads or
    /// writes the real `~/.cache/wim`.
    fn scratch_dir(label: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("wim-catalog-{label}-{nanos}"));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// `save_catalog_in` for a whole `ServerCatalog`; the real caller
    /// passes the three slices separately so persisting a fetch costs
    /// no clone of it.
    fn save(dir: &Path, key: &str, catalog: &ServerCatalog) -> Result<bool> {
        save_catalog_in(
            dir,
            key,
            &catalog.artists,
            &catalog.albums,
            &catalog.playlists,
            &|_| None,
        )
    }

    /// `save`, but with every album credited to one artist id, the
    /// shape a real server's catalog has.
    fn save_with_artist_ids(
        dir: &Path,
        key: &str,
        catalog: &ServerCatalog,
        artist_id: &str,
    ) -> Result<bool> {
        save_catalog_in(
            dir,
            key,
            &catalog.artists,
            &catalog.albums,
            &catalog.playlists,
            &|_| Some(artist_id.to_string()),
        )
    }

    fn sample_catalog(artists: usize, albums: usize, playlists: usize) -> ServerCatalog {
        ServerCatalog {
            artists: (0..artists)
                .map(|i| Artist {
                    id: format!("ar-{i}"),
                    name: format!("artist {i}"),
                })
                .collect(),
            albums: (0..albums)
                .map(|i| Album {
                    id: format!("al-{i}"),
                    name: format!("album {i}"),
                    artist: format!("artist {}", i % artists.max(1)),
                })
                .collect(),
            playlists: (0..playlists)
                .map(|i| Playlist {
                    id: format!("pl-{i}"),
                    name: format!("playlist {i}"),
                })
                .collect(),
        }
    }

    #[test]
    fn a_catalog_survives_a_round_trip_through_the_disk() {
        let dir = scratch_dir("roundtrip");
        let catalog = sample_catalog(3, 5, 2);
        assert!(save_with_artist_ids(&dir, "u@https://example.invalid", &catalog, "ar-7").unwrap());

        let (back, album_artists) =
            load_catalog_in(&dir, "u@https://example.invalid").expect("must load back");
        assert_eq!(
            album_artists.len(),
            5,
            "every album's artist id should survive the round trip"
        );
        assert_eq!(
            album_artists[0],
            ("al-0".to_string(), "ar-7".to_string()),
            "the link is (album id, artist id)"
        );
        assert_eq!(back.artists.len(), 3);
        assert_eq!(back.albums.len(), 5);
        assert_eq!(back.playlists.len(), 2);
        assert_eq!(back.artists[1].name, "artist 1");
        assert_eq!(back.albums[2].artist, "artist 2");
        assert_eq!(back.playlists[0].id, "pl-0");
        fs::remove_dir_all(&dir).ok();
    }

    /// No credentials in it, but it is a map of someone's library and
    /// which server it came from; 0600 regardless, from the moment the
    /// file exists.
    #[cfg(unix)]
    #[test]
    fn a_persisted_catalog_is_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let dir = scratch_dir("perms");
        let one = sample_catalog(1, 1, 1);
        save(&dir, "u@https://example.invalid", &one).unwrap();

        let path = catalog_path(&dir, "u@https://example.invalid");
        let file_mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(file_mode, 0o600, "catalog maps out a private library");
        let dir_mode = fs::metadata(&dir).unwrap().permissions().mode() & 0o777;
        assert_eq!(dir_mode, 0o700);
        assert!(
            !dir.join("README").exists(),
            "no temp file should be left behind"
        );
        let leftovers: Vec<PathBuf> = fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|e| e == "tmp"))
            .collect();
        assert!(leftovers.is_empty(), "left a temp file: {leftovers:?}");
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn an_unchanged_catalog_is_not_rewritten() {
        let dir = scratch_dir("skip");
        let catalog = sample_catalog(2, 4, 1);
        assert!(save(&dir, "k", &catalog).unwrap(), "first write");
        assert!(
            !save(&dir, "k", &catalog).unwrap(),
            "an auto-sync that found the same catalog must not rewrite it"
        );
        let mut grown = catalog.clone();
        grown.albums.push(Album {
            id: "al-new".into(),
            name: "new".into(),
            artist: "artist 0".into(),
        });
        assert!(
            save(&dir, "k", &grown).unwrap(),
            "a real change must be written"
        );
        assert_eq!(load_catalog_in(&dir, "k").unwrap().0.albums.len(), 5);
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_catalog_written_for_another_server_is_not_loaded() {
        let dir = scratch_dir("foreign");
        save(&dir, "a@https://one.invalid", &sample_catalog(1, 1, 0)).unwrap();
        // same directory, different key: a different file, and even a
        // digest collision would be caught by the stored key.
        assert!(load_catalog_in(&dir, "b@https://two.invalid").is_none());

        let path = catalog_path(&dir, "a@https://one.invalid");
        let raw = fs::read_to_string(&path).unwrap();
        let tampered = raw.replacen(
            &catalog_id("a@https://one.invalid"),
            &catalog_id("b@https://two.invalid"),
            1,
        );
        fs::write(&path, tampered).unwrap();
        assert!(
            load_catalog_in(&dir, "a@https://one.invalid").is_none(),
            "a header naming another server must not be served as this one's"
        );
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_catalog_from_an_older_format_is_ignored_rather_than_half_parsed() {
        let dir = scratch_dir("version");
        fs::write(
            catalog_path(&dir, "k"),
            "{\"v\":0,\"id\":\"x\",\"sum\":\"x\",\"at\":1}\n{\"k\":\"artist\",\"id\":\"a\",\"name\":\"n\"}\n",
        )
        .unwrap();
        assert!(load_catalog_in(&dir, "k").is_none());
        fs::remove_dir_all(&dir).ok();
    }

    /// A catalog written before album rows carried an artist id is the
    /// common case on the first launch after this change, and it has
    /// to load as "no ids known", which is the name-fallback case, not
    /// as an unparseable row.
    #[test]
    fn an_album_row_without_an_artist_id_still_loads() {
        let dir = scratch_dir("noartistid");
        let header = serde_json::to_string(&CatalogHeader {
            v: CATALOG_FORMAT,
            id: catalog_id("k"),
            sum: "ignored".into(),
            at: 1,
        })
        .unwrap();
        fs::write(
            catalog_path(&dir, "k"),
            format!(
                "{header}\n{}\n",
                r#"{"k":"album","id":"al-1","name":"Crest","artist":"Bladee"}"#
            ),
        )
        .unwrap();

        let (back, album_artists) = load_catalog_in(&dir, "k").expect("an old row must still load");
        assert_eq!(back.albums.len(), 1);
        assert_eq!(back.albums[0].name, "Crest");
        assert!(album_artists.is_empty(), "nothing to learn, and no blanks");
        fs::remove_dir_all(&dir).ok();
    }

    /// And the reverse: a server that reports no artist ids must write
    /// the same bytes it always did, so nobody's catalog is rewritten
    /// for a field that is not there.
    #[test]
    fn an_album_row_with_no_artist_id_omits_the_field_entirely() {
        let body = catalog_body(&[], &sample_catalog(1, 1, 0).albums, &[], &|_| None);
        assert_eq!(
            body.trim(),
            r#"{"k":"album","id":"al-0","name":"album 0","artist":"artist 0"}"#
        );
    }

    /// The reason for newline-delimited rows: one bad line costs one
    /// entry, not the whole library.
    #[test]
    fn a_corrupt_row_costs_one_entry_not_the_catalog() {
        let dir = scratch_dir("corrupt");
        save(&dir, "k", &sample_catalog(3, 0, 0)).unwrap();
        let path = catalog_path(&dir, "k");
        let mut lines: Vec<String> = fs::read_to_string(&path)
            .unwrap()
            .lines()
            .map(str::to_string)
            .collect();
        lines[2] = "{\"k\":\"artist\",\"id\":".to_string(); // truncated mid-row
        fs::write(&path, lines.join("\n")).unwrap();

        let (back, _) = load_catalog_in(&dir, "k").expect("the rest must still load");
        assert_eq!(back.artists.len(), 2);
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn sweeping_drops_catalogs_for_servers_that_are_gone() {
        let dir = scratch_dir("sweep");
        save(&dir, "keep", &sample_catalog(1, 1, 0)).unwrap();
        save(&dir, "drop", &sample_catalog(1, 1, 0)).unwrap();

        let keep = vec![digest_hex("keep")];
        assert_eq!(sweep_foreign_catalogs_in(&dir, &keep), 1);
        assert!(load_catalog_in(&dir, "keep").is_some());
        assert!(load_catalog_in(&dir, "drop").is_none());
        assert_eq!(
            sweep_foreign_catalogs_in(&dir, &keep),
            0,
            "nothing left to sweep"
        );
        fs::remove_dir_all(&dir).ok();
    }

    /// What the row shape used to be, kept only as the baseline
    /// `catalog_parse_scaling` measures against: the same bytes,
    /// deserialized through an internally-tagged enum.
    #[derive(Debug, Deserialize)]
    #[serde(tag = "k", rename_all = "lowercase")]
    enum TaggedRow {
        Artist {
            id: String,
            name: String,
        },
        Album {
            id: String,
            name: String,
            artist: String,
            #[serde(default)]
            artist_id: Option<String>,
        },
        Playlist {
            id: String,
            name: String,
        },
    }

    fn parse_rows_tagged<'a>(lines: impl Iterator<Item = &'a str>) -> CatalogRows {
        let mut catalog = ServerCatalog::default();
        let mut album_artists = Vec::new();
        for row in lines.filter_map(|line| serde_json::from_str::<TaggedRow>(line).ok()) {
            match row {
                TaggedRow::Artist { id, name } => catalog.artists.push(Artist { id, name }),
                TaggedRow::Album {
                    id,
                    name,
                    artist,
                    artist_id,
                } => {
                    if let Some(artist_id) = artist_id {
                        album_artists.push((id.clone(), artist_id));
                    }
                    catalog.albums.push(Album { id, name, artist })
                }
                TaggedRow::Playlist { id, name } => catalog.playlists.push(Playlist { id, name }),
            }
        }
        (catalog, album_artists)
    }

    /// The flat row shape has to read the same catalog the tagged one
    /// did, row for row, or the format changed under every catalog
    /// already on disk.
    #[test]
    fn the_flat_row_reads_exactly_what_the_tagged_row_did() {
        let catalog = sample_catalog(3, 5, 2);
        let body = catalog_body(
            &catalog.artists,
            &catalog.albums,
            &catalog.playlists,
            &|_| Some("ar-9".to_string()),
        );
        let (flat, flat_links) = parse_rows(body.lines());
        let (tagged, tagged_links) = parse_rows_tagged(body.lines());
        assert_eq!(flat_links, tagged_links);
        assert_eq!(flat.artists.len(), tagged.artists.len());
        assert_eq!(flat.albums.len(), tagged.albums.len());
        assert_eq!(flat.playlists.len(), tagged.playlists.len());
        for (a, b) in flat.albums.iter().zip(&tagged.albums) {
            assert_eq!((&a.id, &a.name, &a.artist), (&b.id, &b.name, &b.artist));
        }
        for (a, b) in flat.artists.iter().zip(&tagged.artists) {
            assert_eq!((&a.id, &a.name), (&b.id, &b.name));
        }
        // and an unknown kind is still one skipped row, not a failure
        assert_eq!(
            parse_rows(["{\"k\":\"genre\",\"id\":\"g\",\"name\":\"n\"}"].into_iter())
                .0
                .artists
                .len(),
            0
        );
    }

    /// The candidate parallel row parse, kept here rather than in the
    /// module so the numbers decide whether it ships; see
    /// `catalog_parse_scaling`. Chunks keep file order and are
    /// concatenated in chunk order, so the result is row-for-row the
    /// sequential one (`parse_rows`'s order contract).
    fn parse_rows_parallel(body: &str, workers: usize) -> CatalogRows {
        let lines: Vec<&str> = body.lines().collect();
        if lines.is_empty() {
            return CatalogRows::default();
        }
        let chunk = lines.len().div_ceil(workers.max(1));
        let parts: Vec<CatalogRows> = std::thread::scope(|scope| {
            lines
                .chunks(chunk)
                .map(|part| scope.spawn(move || parse_rows(part.iter().copied())))
                .collect::<Vec<_>>()
                .into_iter()
                .map(|handle| handle.join().unwrap_or_default())
                .collect()
        });
        let mut out = CatalogRows::default();
        for (catalog, links) in parts {
            out.0.artists.extend(catalog.artists);
            out.0.albums.extend(catalog.albums);
            out.0.playlists.extend(catalog.playlists);
            out.1.extend(links);
        }
        out
    }

    /// A catalog body with a real library's row mix (artists, albums,
    /// a handful of playlists) at `rows` total rows.
    fn body_of(rows: usize) -> String {
        let artists = rows * 12 / 100;
        let albums = rows - artists - rows / 300;
        let playlists = rows / 300;
        let catalog = sample_catalog(artists, albums, playlists);
        let body = catalog_body(
            &catalog.artists,
            &catalog.albums,
            &catalog.playlists,
            &|id| Some(format!("ar-{}", id.len())),
        );
        assert_eq!(body.lines().count(), rows);
        body
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

    /// Cpu time, not wall time, for the same reason
    /// `search::tests::cpu_now` exists: this machine's other cores are
    /// busy during a bench run, so wall clock moves 2-3x between runs
    /// while the work done does not. The single-threaded numbers are
    /// only comparable in this clock.
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

    /// Alternating batches in one process, best of each, in
    /// microseconds. Same helper and the same reason as
    /// the alternating-batch benchmark harness: sequential runs on this
    /// box differ 1.6-1.8x under load, so two numbers from two runs
    /// are not comparable and this is the only shape that is.
    fn best_each(
        rounds: usize,
        iters: usize,
        a: &mut dyn FnMut(),
        b: &mut dyn FnMut(),
    ) -> (f64, f64) {
        let mut best = (f64::MAX, f64::MAX);
        let time = |f: &mut dyn FnMut()| {
            let at = std::time::Instant::now();
            for _ in 0..iters {
                f();
            }
            at.elapsed().as_secs_f64() * 1e6 / iters as f64
        };
        for _ in 0..rounds {
            best.0 = best.0.min(time(a));
            best.1 = best.1.min(time(b));
        }
        best
    }

    /// `best_each` on the process cpu clock instead of the wall clock,
    /// for the pairs where both sides are single-threaded. This box's
    /// other cores are not idle during a bench run (load average 28
    /// observed, wall 6x cpu on the same work), so wall clock moves
    /// several-fold between batches while the work done does not. A
    /// pooled candidate still has to be read on the wall clock, since
    /// cpu time is exactly what it trades away.
    fn best_each_cpu(
        rounds: usize,
        iters: usize,
        a: &mut dyn FnMut(),
        b: &mut dyn FnMut(),
    ) -> (f64, f64) {
        let cpu_now = || {
            let mut ts = libc::timespec {
                tv_sec: 0,
                tv_nsec: 0,
            };
            // SAFETY: clock_gettime only writes the timespec we hand it.
            unsafe { libc::clock_gettime(libc::CLOCK_PROCESS_CPUTIME_ID, &mut ts) };
            ts.tv_sec as f64 * 1e6 + ts.tv_nsec as f64 / 1e3
        };
        let mut best = (f64::MAX, f64::MAX);
        let time = |f: &mut dyn FnMut()| {
            let at = cpu_now();
            for _ in 0..iters {
                f();
            }
            (cpu_now() - at) / iters as f64
        };
        for _ in 0..rounds {
            best.0 = best.0.min(time(a));
            best.1 = best.1.min(time(b));
        }
        best
    }

    /// Which profile a measurement ran in. `cargo test` defaults to
    /// `opt-level = 0`, where one of these paths measured 11x its
    /// release cost, and a debug number quoted as the cost is how a
    /// non-problem becomes a blocker.
    fn profile() -> &'static str {
        if cfg!(debug_assertions) {
            "DEBUG (not the shipping cost; re-run with --release)"
        } else {
            "release"
        }
    }

    /// The candidate pooled row parse, kept here rather than in the
    /// module because the numbers rejected it; see
    /// `play_index_parse_legs`. The map insert stays serial, since
    /// hashing every id once is the irreducible part, which is most of
    /// why it scales so poorly.
    fn parse_index_body_pooled(raw: &str) -> CacheIndex {
        let lines: Vec<&str> = raw.lines().collect();
        let workers = std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(4)
            .min(lines.len().max(1));
        let chunk = lines.len().div_ceil(workers.max(1)).max(1);
        let parts: Vec<Vec<(String, CacheEntry)>> = std::thread::scope(|scope| {
            lines
                .chunks(chunk)
                .map(|part| {
                    scope.spawn(move || part.iter().copied().filter_map(play_entry).collect())
                })
                .collect::<Vec<_>>()
                .into_iter()
                .map(|h| h.join().unwrap_or_default())
                .collect()
        });
        let mut entries = HashMap::with_capacity(lines.len());
        entries.extend(parts.into_iter().flatten());
        CacheIndex { entries }
    }

    /// `index_body` as it was before the days were packed: the day
    /// list written as `YYYY-MM-DD` labels, which is what every index
    /// already on disk holds until its first save after this change.
    /// The baseline both for the format's cost and for "reads the same
    /// history".
    fn index_body_labelled(index: &CacheIndex) -> String {
        #[derive(Serialize)]
        struct LabelledRow<'a> {
            id: &'a str,
            play_count: u32,
            play_days: Vec<String>,
            #[serde(skip_serializing_if = "Option::is_none")]
            last_played_secs: Option<u64>,
            #[serde(skip_serializing_if = "Option::is_none")]
            cached_path: Option<&'a str>,
            #[serde(skip_serializing_if = "Option::is_none")]
            cached_at_secs: Option<u64>,
        }
        let mut body = String::new();
        for (id, entry) in &index.entries {
            let row = LabelledRow {
                id,
                play_count: entry.play_count,
                play_days: entry
                    .play_days
                    .iter()
                    .map(|&d| day_of(d as u64 * 86_400))
                    .collect(),
                last_played_secs: entry.last_played_secs,
                cached_path: entry.cached_path.as_deref(),
                cached_at_secs: entry.cached_at_secs,
            };
            if let Ok(line) = serde_json::to_string(&row) {
                body.push_str(&line);
                body.push('\n');
            }
        }
        body
    }

    /// Every index already on disk holds `YYYY-MM-DD` day labels, and
    /// the day count is exactly what the auto-download threshold is
    /// measured against: a packed reader that could not read them
    /// would silently reset everyone's history to one day. Same oracle
    /// shape as `the_row_shape_reads_exactly_what_the_toml_shape_did`.
    #[test]
    fn an_index_written_with_day_labels_reads_the_same_days_as_a_packed_one() {
        let mut idx = play_index(200, 5);
        idx.record_play("t-odd", "2026-01-09");
        idx.mark_cached("t-odd", PathBuf::from("/c/wim/tracks/t-odd.mp3"));
        let labelled = index_body_labelled(&idx);
        assert!(
            labelled.contains("\"play_days\":[\"2023-11-14\""),
            "the baseline has to be the label shape: {}",
            &labelled[..labelled.len().min(160)]
        );
        let from_labels = parse_index_body(&labelled);
        assert_eq!(
            from_labels.entries, idx.entries,
            "an index written with day labels has to load as the same history"
        );
        assert_eq!(
            from_labels.entries,
            parse_index_body(&index_body(&idx)).entries
        );
    }

    /// Both shapes in one list, plus one that is neither. The row
    /// still loads and keeps every day it could read: the ndjson index
    /// costs one track per bad line, but the legacy `index.toml` is
    /// read as one document, so a single hand-edited date must not
    /// reset a whole install's history.
    #[test]
    fn an_unreadable_day_costs_that_day_and_not_the_track() {
        let raw = concat!(
            r#"{"id":"t1","play_count":4,"#,
            r#""play_days":["2026-01-01","nonsense",20456,"2026-13-99"],"#,
            r#""last_played_secs":1}"#,
            "\n"
        );
        let idx = parse_index_body(raw);
        assert_eq!(
            idx.entry("t1").expect("the row still loads").play_days,
            [20_454, 20_456],
            "2026-01-01 read from a label, 20456 read as itself"
        );
        assert_eq!(idx.play_count("t1"), 4);

        let legacy_raw =
            "[entries.t1]\nplay_count = 7\nplay_days = [\"2026-01-01\", \"2026-13-99\"]\n";
        let legacy: CacheIndex = toml::from_str(legacy_raw).expect("the document must still parse");
        assert_eq!(legacy.days_played("t1"), 1);
        assert_eq!(legacy.play_count("t1"), 7);
    }

    #[test]
    fn day_number_is_the_exact_inverse_of_day_of() {
        for secs in [0u64, 86_399, 86_400, 1_709_164_800, 1_767_225_600] {
            assert_eq!(
                day_number(&day_of(secs)),
                Some((secs / 86_400) as i64),
                "round trip failed for {secs}"
            );
            assert_eq!(day_number_of(secs), (secs / 86_400) as i32);
        }
        assert_eq!(day_number("garbage"), None);
        assert_eq!(day_number("2026-13-01"), None);
    }

    /// `parse_index_body` as it was before the map was pre-sized:
    /// kept as the baseline `play_index_parse_legs` measures against.
    fn parse_index_body_growing(raw: &str) -> CacheIndex {
        let mut index = CacheIndex::default();
        for row in raw
            .lines()
            .filter_map(|line| serde_json::from_str::<PlayRow>(line).ok())
        {
            index.entries.insert(
                row.id.into_owned(),
                CacheEntry {
                    play_count: row.play_count,
                    play_days: row.play_days,
                    last_played_secs: row.last_played_secs,
                    cached_path: row.cached_path.map(Cow::into_owned),
                    cached_at_secs: row.cached_at_secs,
                    download_source: row.download_source,
                },
            );
        }
        index
    }

    /// `CacheIndex::load` is the one startup read still on the **main
    /// thread**, before the first frame and with nothing overlapping
    /// it, so every microsecond here is a microsecond of first frame.
    /// Three things at once: the growing map against the pre-sized one
    /// (cpu clock, both single-threaded), the pooled candidate against
    /// the serial parse (wall clock, which is what a pool trades cpu
    /// for), and what `play_days` costs, which the 4-day and 64-day
    /// rows at the same track count price apart.
    ///
    /// **What shipped:** the pre-sized map, 1.07-1.16x of the parse,
    /// the same at every size but 64 days (where allocating the day
    /// strings is ~90% of the parse and swamps it).
    ///
    /// **What did not: the pool.** Measured 1.09-1.47x on a box at
    /// load average 14-18 and 0.36-1.90x on the same box at load 25-31
    /// (same sizes, same harness, one process), so there is no floor
    /// this machine can set honestly, which is the same thing
    /// `docs/startup-cache.md` records about fanning the catalog parse
    /// out. It would be buying 1-2ms at 5000 tracks, the size the
    /// docs call a long-running install, for a thread pool on the
    /// startup path.
    ///
    /// **What also shipped: packed `play_days`.** The parse used to go
    /// almost entirely into `play_days`: 20000 tracks cost 36-41ms at
    /// 4 days each and 256-271ms at 64, nine tenths of it allocating
    /// a `Vec<String>` of `YYYY-MM-DD` per track. Day numbers instead
    /// (`index_body_labelled` is the label shape, kept as the
    /// baseline) take that back; the ratio is the last column.
    ///
    /// `cargo test --release -- --ignored --nocapture
    /// play_index_parse_legs`.
    #[test]
    #[ignore = "measurement, run explicitly"]
    fn play_index_parse_legs() {
        println!("--- play index parse, {} build", profile());
        for (tracks, days) in [
            (1_000usize, 3usize),
            (2_000, 3),
            (4_000, 4),
            (5_000, 4),
            (20_000, 4),
            (20_000, 64),
        ] {
            let index = play_index(tracks, days);
            let raw = index_body(&index);
            let labelled = index_body_labelled(&index);
            let (labels, packed) = best_each_cpu(
                8,
                2,
                &mut || {
                    parse_index_body(&labelled);
                },
                &mut || {
                    parse_index_body(&raw);
                },
            );
            let (growing, sized) = best_each_cpu(
                8,
                2,
                &mut || {
                    parse_index_body_growing(&raw);
                },
                &mut || {
                    parse_index_body(&raw);
                },
            );
            let (serial, pooled) = best_each(
                8,
                2,
                &mut || {
                    parse_index_body(&raw);
                },
                &mut || {
                    parse_index_body_pooled(&raw);
                },
            );
            println!(
                "{tracks:>6} tracks x {days:>2} days  cpu: growing map \
                 {growing:>9.0}us pre-sized {sized:>9.0}us {:.2}x   wall: serial \
                 {serial:>9.0}us pooled {pooled:>9.0}us {:.2}x   cpu: day labels \
                 ({:>9}B) {labels:>9.0}us packed ({:>9}B) {packed:>9.0}us {:.2}x",
                growing / sized,
                serial / pooled,
                labelled.len(),
                raw.len(),
                labels / packed,
            );
        }
    }

    /// Pre-sizing the map is a capacity, not a format: it must not
    /// change, drop or merge a row. Pinned against a body with a row
    /// the generated ones do not have, and against the rejected pooled
    /// candidate, which is only evidence while it still reads the same
    /// index.
    #[test]
    fn pre_sizing_the_map_reads_exactly_what_growing_it_did() {
        let mut idx = play_index(300, 5);
        idx.record_play("t-odd", "2026-01-09");
        idx.mark_cached("t-odd", PathBuf::from("/c/wim/tracks/t-odd.mp3"));
        let raw = index_body(&idx);
        assert_eq!(raw.lines().count(), 301, "precondition");

        let sized = parse_index_body(&raw);
        assert_eq!(sized.entries, idx.entries, "the parse round-trips");
        assert_eq!(parse_index_body_growing(&raw).entries, sized.entries);
        assert_eq!(parse_index_body_pooled(&raw).entries, sized.entries);
    }

    /// Does the row parse need parallelising? It is on the startup
    /// path (`search::spawn_warm_from_cache`), where it overlaps the
    /// mpv spawn; past the point where it outgrows that overlap it
    /// would resurface on the first frame. Prints sequential against
    /// `parse_rows_parallel` at the real size and two multiples of it.
    /// `cargo test --release -- --ignored --nocapture
    /// catalog_parse_scaling`.
    #[test]
    #[ignore = "measurement, run explicitly"]
    fn catalog_parse_scaling() {
        let threads = std::thread::available_parallelism().map_or(1, |n| n.get());
        println!("{threads} cores available");
        for rows in [34_000usize, 100_000, 300_000] {
            let body = body_of(rows);
            let (tagged_cpu, _) = best_cpu(5, || parse_rows_tagged(body.lines()));
            let (flat_cpu, one) = best_cpu(5, || parse_rows(body.lines()));
            let (flat, _) = best(5, || parse_rows(body.lines()));
            let (par, many) = best(5, || parse_rows_parallel(&body, threads));
            let (chunked, _) = best(5, || parse_rows_parallel(&body, 1));
            assert_eq!(one.0.albums.len(), many.0.albums.len());
            println!(
                "{rows:>7} rows ({:>8} bytes)  cpu: tagged {tagged_cpu:>10.3?} flat \
                 {flat_cpu:>10.3?}   wall: flat {flat:>10.3?} threads({threads}) \
                 {par:>10.3?} 1-chunk {chunked:>10.3?}",
                body.len()
            );
        }
    }

    /// Why newline-delimited json and not `toml::to_string_pretty`
    /// or pretty json: at catalog scale the
    /// format is most of the file. Prints the comparison; run with
    /// `cargo test -- --ignored --nocapture`.
    #[test]
    #[ignore = "measurement, run explicitly"]
    fn catalog_format_size_and_load_time() {
        use std::time::Instant;
        let dir = scratch_dir("bench");
        let catalog = sample_catalog(4_000, 30_000, 120);

        let started = Instant::now();
        save(&dir, "k", &catalog).unwrap();
        let write = started.elapsed();
        let path = catalog_path(&dir, "k");
        let ndjson_bytes = fs::metadata(&path).unwrap().len();

        let started = Instant::now();
        let (back, _) = load_catalog_in(&dir, "k").unwrap();
        let read = started.elapsed();
        assert_eq!(back.albums.len(), 30_000);

        // same rows as one pretty json document, for scale
        let rows: Vec<CatalogRow> = catalog
            .albums
            .iter()
            .map(|a| CatalogRow {
                k: RowKind::Album,
                id: Cow::Borrowed(&a.id),
                name: Cow::Borrowed(&a.name),
                artist: Some(Cow::Borrowed(&a.artist)),
                artist_id: None,
            })
            .collect();
        let pretty = serde_json::to_string_pretty(&rows).unwrap().len();
        let compact = serde_json::to_string(&rows).unwrap().len();

        eprintln!(
            "catalog 4000 artists + 30000 albums + 120 playlists:\n  \
             ndjson {ndjson_bytes} bytes, write {write:?}, load {read:?}\n  \
             albums alone: compact json array {compact} bytes, pretty {pretty} bytes"
        );
        fs::remove_dir_all(&dir).ok();
    }

    /// A play index the size a long-running install reaches: `tracks`
    /// entries, each with `days` recorded play days.
    fn play_index(tracks: usize, days: usize) -> CacheIndex {
        let mut idx = CacheIndex::default();
        for t in 0..tracks {
            idx.entries.insert(
                format!("track-id-{t:08}"),
                CacheEntry {
                    play_count: days as u32,
                    play_days: (0..days)
                        .map(|d| day_number_of(1_700_000_000 + d as u64 * 86_400))
                        .collect(),
                    last_played_secs: Some(1_700_000_000),
                    // only a minority of played tracks are ever
                    // downloaded, so most rows carry no path.
                    cached_path: (t % 10 == 0)
                        .then(|| format!("/home/u/.cache/wim/tracks/track-id-{t:08}.mp3")),
                    cached_at_secs: (t % 10 == 0).then_some(1_700_000_000),
                    download_source: None,
                },
            );
        }
        idx
    }

    /// `CacheIndex::load` is the one startup read still on the **main
    /// thread** (main.rs builds the `Arc<Mutex<_>>` before the first
    /// frame), and `save` runs on every track change from that same
    /// thread. Why it is not toml, at three install ages:
    /// `cargo test --release -- --ignored --nocapture
    /// cache_index_load_scaling`.
    #[test]
    #[ignore = "measurement, run explicitly"]
    fn cache_index_load_scaling() {
        for (tracks, days) in [(200usize, 3usize), (2_000, 3), (5_000, 4), (20_000, 64)] {
            let idx = play_index(tracks, days);
            let as_toml = toml::to_string_pretty(&idx).unwrap();
            let (t_parse, t_back) = best_cpu(5, || toml::from_str::<CacheIndex>(&as_toml).unwrap());
            let (t_render, _) = best_cpu(3, || toml::to_string_pretty(&idx).unwrap().len());
            assert_eq!(t_back.entries.len(), tracks);

            let as_rows = index_body(&idx);
            let (r_parse, r_back) = best_cpu(5, || parse_index_body(&as_rows));
            let (r_render, _) = best_cpu(3, || index_body(&idx).len());
            assert_eq!(r_back.entries.len(), tracks);

            println!(
                "{tracks:>6} tracks x {days:>2} days | toml {:>9}B parse {t_parse:>10.3?} \
                 render {t_render:>10.3?} | ndjson {:>9}B parse {r_parse:>10.3?} render {r_render:>10.3?}",
                as_toml.len(),
                as_rows.len()
            );
        }
    }

    /// Where a warm catalog read actually spends its time: the file
    /// read, the header line, and the row parse, separately. The row
    /// parse is the only one `catalog_parse_scaling` covers.
    /// `cargo test --release -- --ignored --nocapture
    /// catalog_load_phases`.
    #[test]
    #[ignore = "measurement, run explicitly"]
    fn catalog_load_phases() {
        let dir = scratch_dir("phases");
        for rows in [34_000usize, 100_000] {
            let body = body_of(rows);
            let path = catalog_path(&dir, "k");
            let header = serde_json::to_string(&CatalogHeader {
                v: CATALOG_FORMAT,
                id: catalog_id("k"),
                sum: "ignored".into(),
                at: 1,
            })
            .unwrap();
            fs::write(&path, format!("{header}\n{body}")).unwrap();

            let (read, raw) = best_cpu(5, || fs::read_to_string(&path).unwrap());
            let (split, _) = best_cpu(5, || {
                let mut s = raw.splitn(2, '\n');
                let h: CatalogHeader = serde_json::from_str(s.next().unwrap()).unwrap();
                h.v
            });
            let (parse, parsed) = best_cpu(5, || parse_rows(body.lines()));
            let (whole, _) = best_cpu(5, || load_catalog_in(&dir, "k").unwrap());
            assert_eq!(
                parsed.0.albums.len(),
                load_catalog_in(&dir, "k").unwrap().0.albums.len()
            );
            println!(
                "{rows:>7} rows ({:>8} bytes)  read {read:>10.3?}  header {split:>10.3?}  \
                 rows {parse:>10.3?}  whole {whole:>10.3?}  links {}",
                raw.len(),
                parsed.1.len()
            );
        }
        fs::remove_dir_all(&dir).ok();
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

#[cfg(test)]
mod safe_file_stem_tests {
    use super::*;

    #[test]
    fn ordinary_ids_keep_their_names() {
        assert_eq!(safe_file_stem("abc123"), "abc123");
        assert_eq!(safe_file_stem("a.b-c_d"), "a.b-c_d");
    }

    #[test]
    fn ids_that_could_leave_the_tracks_dir_are_hashed() {
        for id in ["../../x", "a/b", "a\\b", "..", ".", "", "x\0y"] {
            let stem = safe_file_stem(id);
            assert!(stem.starts_with("id-"), "{id:?} -> {stem}");
            assert!(!stem.contains(['/', '\\', '\0']));
        }
        assert_ne!(safe_file_stem("a/b"), safe_file_stem("a/c"));
    }
}

#[cfg(test)]
mod day_number_bounds_tests {
    use super::*;

    #[test]
    fn absurd_years_are_unparseable_not_an_overflow() {
        assert_eq!(day_number("99999999999999999-01-01"), None);
        assert_eq!(day_number("-5-01-01"), None);
        assert!(day_number("2026-11-14").is_some());
        assert_eq!(day_number("1970-01-01"), Some(0));
    }
}
