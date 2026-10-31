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
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
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

/// Today's `day_of`. The one wall-clock read in the day-threshold
/// path; everything that decides anything takes the day as an
/// argument instead, so the policy stays testable against fixed days.
pub fn today() -> String {
    day_of(now_secs())
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
/// and the same measurement behind it: this read is on the **main
/// thread before the first frame** (main.rs builds the shared index
/// there), and the write happens on every track change. At the size a
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
    #[serde(default, borrow)]
    play_days: Vec<Cow<'a, str>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    last_played_secs: Option<u64>,
    #[serde(default, borrow, skip_serializing_if = "Option::is_none")]
    cached_path: Option<Cow<'a, str>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    cached_at_secs: Option<u64>,
}

fn index_body(index: &CacheIndex) -> String {
    let mut body = String::new();
    for (id, entry) in &index.entries {
        let row = PlayRow {
            id: Cow::Borrowed(id),
            play_count: entry.play_count,
            play_days: entry
                .play_days
                .iter()
                .map(|d| Cow::Borrowed(d.as_str()))
                .collect(),
            last_played_secs: entry.last_played_secs,
            cached_path: entry.cached_path.as_deref().map(Cow::Borrowed),
            cached_at_secs: entry.cached_at_secs,
        };
        if let Ok(line) = serde_json::to_string(&row) {
            body.push_str(&line);
            body.push('\n');
        }
    }
    body
}

fn parse_index_body(raw: &str) -> CacheIndex {
    let mut index = CacheIndex::default();
    for row in raw
        .lines()
        .filter_map(|line| serde_json::from_str::<PlayRow>(line).ok())
    {
        index.entries.insert(
            row.id.into_owned(),
            CacheEntry {
                play_count: row.play_count,
                play_days: row.play_days.into_iter().map(Cow::into_owned).collect(),
                last_played_secs: row.last_played_secs,
                cached_path: row.cached_path.map(Cow::into_owned),
                cached_at_secs: row.cached_at_secs,
            },
        );
    }
    index
}

/// Where a given track id would land on disk once downloaded, with the
/// given extension (no leading dot).
pub fn track_path(id: &str, ext: &str) -> Result<PathBuf> {
    Ok(tracks_dir()?.join(format!("{id}.{ext}")))
}

/// Bumped whenever the row shapes below change; a catalog written by
/// an older wim is dropped whole rather than half-parsed.
const CATALOG_FORMAT: u32 = 1;

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
    let path = dir.join(INDEX_FILE);
    fs::write(&path, index_body(index)).with_context(|| format!("writing {}", path.display()))?;
    // the toml index held a second copy of the same history and is
    // dead the moment the line above lands; a failure to unlink it is
    // nothing but a stale file in a cache dir, so it is not reported.
    let _ = fs::remove_file(dir.join(LEGACY_INDEX_FILE));
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
            ["2026-01-01", "2026-01-03"],
            "the day list is what the download threshold counts, and it is ordered"
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
                        .map(|d| day_of(1_700_000_000 + d as u64 * 86_400))
                        .collect(),
                    last_played_secs: Some(1_700_000_000),
                    // only a minority of played tracks are ever
                    // downloaded, so most rows carry no path.
                    cached_path: (t % 10 == 0)
                        .then(|| format!("/home/u/.cache/wim/tracks/track-id-{t:08}.mp3")),
                    cached_at_secs: (t % 10 == 0).then_some(1_700_000_000),
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
