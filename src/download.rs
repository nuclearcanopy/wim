//! Background track downloads into the local cache (see `cache.rs`):
//! byte-level transfer happens off the render thread, same reasoning as
//! `main::spawn_background_sync`. Progress is reported back over an
//! mpsc channel tagged by track id, polled once per frame in `main`'s
//! loop the same way `BackgroundSyncResult` already is.
//!
//! A batch is bounded rather than a thread per track: `core::
//! download_resolved` hands a whole discography straight through, so
//! the download key on an artist meant 800 threads and 800 sockets off
//! one keypress. `DOWNLOAD_CONCURRENCY` workers pull from a shared
//! counter instead, share one pooled agent that has stall timeouts,
//! and a big batch reports as one coalesced toast rather than one per
//! track (see `BatchTally`).

use std::fs::File;
use std::io::{Read, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex, OnceLock};
use std::thread;
use std::time::Duration;

use crate::cache::{self, CacheIndex};

/// One update about a download in flight, tagged by track id so
/// several interleaved downloads (a whole album requested at once) can
/// all report over the same channel without main.rs needing a
/// separate channel per track.
///
/// `id` is always the real track id (main.rs keys the live progress box
/// and its `downloading` set off it). `label` is the track's own name
/// only while a batch is small enough to be read one toast at a time;
/// past `PER_TRACK_TOASTS` every outcome carries the batch's first
/// label instead, which is what collapses it into one counted box.
#[derive(Debug)]
pub enum DownloadEvent {
    Progress {
        id: String,
        downloaded: u64,
        total: Option<u64>,
    },
    Done {
        id: String,
        label: String,
    },
    Failed {
        id: String,
        label: String,
        error: String,
    },
}

const CHUNK_SIZE: usize = 64 * 1024;

/// How many tracks of a batch transfer at once. Deliberately lower
/// than `subsonic::ALBUM_FETCH_CONCURRENCY`'s 8: those are small json
/// round trips where concurrency hides latency, these are whole audio
/// files where the link is the bottleneck, so more sockets only make
/// every single track land later. Same figure and same reasoning as
/// `enrich::ENRICH_CONCURRENCY`.
const DOWNLOAD_CONCURRENCY: usize = 4;

/// Stall budgets for one track. No overall deadline: a long flac on a
/// slow link is legitimately minutes of transfer, so what has to be
/// bounded is inactivity. A black-holing server trips the read timeout,
/// the transfer errors, and the `.part` file goes, instead of pinning a
/// worker and leaving the stub on disk forever.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const READ_TIMEOUT: Duration = Duration::from_secs(30);

/// Up to this many tracks, a batch still reports one outcome toast per
/// track, the way a single download always has. Past it every outcome
/// reuses the first one's text, so `notify`'s own push coalescing
/// shows one counted box: it stacks five at once, so per-track toasts
/// beyond that only evict each other instead of being read.
const PER_TRACK_TOASTS: usize = 5;
static REPLACEMENT_PATH_ID: AtomicUsize = AtomicUsize::new(0);

/// Where a finished download lands: `cache::track_path` in the app,
/// a temp dir under test, so nothing ever writes to the real cache.
type DestFor<'a> = &'a (dyn Fn(&str, &str) -> anyhow::Result<PathBuf> + Send + Sync);

/// What a batch writes through. Injected rather than reached for
/// directly so a test drives the whole path without touching
/// `~/.cache/wim`.
struct Sink<'a> {
    cache: &'a Arc<Mutex<CacheIndex>>,
    budget: Arc<cache::CacheBudget>,
    dest_for: DestFor<'a>,
    /// `CacheIndex::save` in the app. Called per finished track, not
    /// once per batch: the files are already on disk, and an index
    /// that forgot them would leak them past the next sweep.
    persist: &'a (dyn Fn(&CacheIndex) -> anyhow::Result<()> + Send + Sync),
    max_bytes: u64,
}

struct BudgetWriter {
    file: File,
    budget: Arc<cache::CacheBudget>,
    reserved: u64,
}

impl BudgetWriter {
    fn new(file: File, budget: Arc<cache::CacheBudget>) -> Self {
        Self {
            file,
            budget,
            reserved: 0,
        }
    }

    fn into_lease(mut self) -> BudgetLease {
        BudgetLease {
            budget: self.budget.clone(),
            bytes: std::mem::take(&mut self.reserved),
            committed: false,
        }
    }
}

impl Write for BudgetWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        if !self.budget.reserve(buf.len() as u64) {
            return Err(std::io::Error::other("offline cache budget is full"));
        }
        self.reserved = self.reserved.saturating_add(buf.len() as u64);
        match self.file.write(buf) {
            Ok(written) => {
                let excess = buf.len().saturating_sub(written) as u64;
                self.reserved = self.reserved.saturating_sub(excess);
                self.budget.release(excess);
                Ok(written)
            }
            Err(error) => {
                self.reserved = self.reserved.saturating_sub(buf.len() as u64);
                self.budget.release(buf.len() as u64);
                Err(error)
            }
        }
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.file.flush()
    }
}

impl Drop for BudgetWriter {
    fn drop(&mut self) {
        self.budget.release(self.reserved);
    }
}

struct BudgetLease {
    budget: Arc<cache::CacheBudget>,
    bytes: u64,
    committed: bool,
}

impl BudgetLease {
    fn commit(&mut self) {
        self.committed = true;
    }
}

impl Drop for BudgetLease {
    fn drop(&mut self) {
        if !self.committed {
            self.budget.release(self.bytes);
        }
    }
}

/// Keeps a big batch's outcomes collapsible. The text of its first
/// `Done` and of its first `Failed` is reused by every later one of
/// that kind, which is the whole reason `notify` can coalesce them:
/// `finish_progress` falls back to `push` for any id whose live box
/// has already been evicted, and `push` merges identical messages into
/// one box with a count. A failing discography was 800 toasts; it is
/// now one reading `download failed: <a track>: <the error> (x800)`.
/// The named track is one example of the batch rather than the only
/// one that failed, which is the cost of staying inside notify's api.
#[derive(Default)]
struct BatchTally {
    done_label: OnceLock<String>,
    failure: OnceLock<(String, String)>,
}

impl BatchTally {
    fn done_label(&self, label: &str) -> String {
        self.done_label.get_or_init(|| label.to_string()).clone()
    }

    fn failure_text(&self, label: &str, error: String) -> (String, String) {
        self.failure
            .get_or_init(|| (label.to_string(), error))
            .clone()
    }
}

/// Guesses a filename extension from the stream response's own
/// Content-Type; falls back to a generic extension rather than failing
/// the download outright over a missing/unrecognized header. Playback
/// doesn't care about the extension at all (mpv sniffs real content),
/// this is purely so the cached file looks plausible to anything else
/// that lists the cache directory.
fn ext_for_content_type(ct: Option<&str>) -> &'static str {
    let ct = ct.unwrap_or_default();
    if ct.contains("flac") {
        "flac"
    } else if ct.contains("ogg") || ct.contains("vorbis") {
        "ogg"
    } else if ct.contains("opus") {
        "opus"
    } else if ct.contains("mp4") || ct.contains("m4a") || ct.contains("aac") {
        "m4a"
    } else if ct.contains("wav") {
        "wav"
    } else if ct.contains("mpeg") || ct.contains("mp3") {
        "mp3"
    } else {
        "audio"
    }
}

/// Why a transfer failed, without the url it failed on. ureq's text for
/// both error variants starts with the whole request url, and a stream
/// url carries `u=`, `t=md5(password + salt)` and that salt; main.rs
/// toasts this string, so the url is dropped and the status or the
/// transport cause kept. Twin of `subsonic::request_failed`, which the
/// json path already goes through.
fn transfer_failed(e: ureq::Error) -> String {
    match e {
        ureq::Error::Status(code, _) => format!("http {code}"),
        ureq::Error::Transport(t) => {
            let mut why = t.kind().to_string();
            if let Some(message) = t.message() {
                why.push_str(": ");
                why.push_str(message);
            }
            if let Some(source) = std::error::Error::source(&t) {
                why.push_str(": ");
                why.push_str(&source.to_string());
            }
            why
        }
    }
}

/// Whether a response body is plainly not audio. Subsonic refuses a
/// `stream` request with an error document rather than an http error
/// ("returns binary data on success, or an XML document on error, in
/// which case the HTTP content type will start with text/xml"), and
/// caching that marks the track cached and reports it downloaded.
/// Anything binary is let through, including a missing header: this
/// refuses what is known to be wrong, not everything unrecognized.
fn is_error_document(ct: Option<&str>) -> bool {
    let ct = ct.unwrap_or_default().trim().to_ascii_lowercase();
    ct.starts_with("text/")
        || ct.starts_with("application/xml")
        || ct.starts_with("application/json")
}

/// One agent for a whole batch: stall timeouts, and a pool big enough
/// that the workers reuse connections instead of each track paying a
/// fresh tcp+tls handshake (`ureq::get` builds a throwaway agent per
/// call, so a 15-track album was 15 handshakes). Same reasoning as
/// `subsonic::MAX_IDLE_CONNECTIONS_PER_HOST`.
fn batch_agent() -> ureq::Agent {
    ureq::AgentBuilder::new()
        .timeout_connect(CONNECT_TIMEOUT)
        .timeout_read(READ_TIMEOUT)
        .max_idle_connections_per_host(DOWNLOAD_CONCURRENCY)
        .build()
}

/// Downloads one track into the cache dir, reporting progress as
/// chunks arrive. Pure blocking I/O on whichever thread calls it;
/// every real caller is already inside a spawned worker (see
/// `spawn_tracks`), never the render thread. `Err` is the text for the
/// failure toast; the label it gets attached to is the caller's
/// decision, since that depends on how big the batch is.
fn download_one(
    agent: &ureq::Agent,
    budget: &Arc<cache::CacheBudget>,
    id: &str,
    url: &str,
    dest_for: DestFor,
    tx: &mpsc::Sender<DownloadEvent>,
) -> Result<(PathBuf, BudgetLease), String> {
    let resp = agent.get(url).call().map_err(transfer_failed)?;
    let total = resp
        .header("Content-Length")
        .and_then(|v| v.parse::<u64>().ok());
    let content_type = resp.header("Content-Type");
    if is_error_document(content_type) {
        return Err("the server sent an error document, not audio".to_string());
    }
    let ext = format!("{}.zst", ext_for_content_type(content_type));
    let mut dest = dest_for(id, &ext).map_err(|e| e.to_string())?;
    // A rebuild must not overwrite the currently indexed file before its
    // replacement has been persisted in the cache index. Keep the normal
    // stable filename for first downloads; use a unique sibling for handoff.
    if dest.exists() {
        let filename = dest
            .file_name()
            .ok_or_else(|| "cache path has no filename".to_string())?;
        let filename = filename.to_string_lossy();
        let media_name = filename.strip_suffix(".zst").unwrap_or(&filename);
        let media_path = std::path::Path::new(media_name);
        let stem = media_path.file_stem().unwrap_or_default().to_string_lossy();
        let media_ext = media_path
            .extension()
            .map(|ext| format!(".{}", ext.to_string_lossy()))
            .unwrap_or_default();
        let parent = dest.parent().unwrap_or_else(|| std::path::Path::new("."));
        loop {
            let suffix = REPLACEMENT_PATH_ID.fetch_add(1, Ordering::Relaxed);
            let candidate = parent.join(format!("{stem}-{suffix}{media_ext}.zst"));
            if !candidate.exists() && !candidate.with_extension("zst.part").exists() {
                dest = candidate;
                break;
            }
        }
    }
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    // downloads into a `.part` sibling first so a half-finished
    // transfer (app killed mid-download) never looks like a valid
    // cached file to `CacheIndex::cached_path`'s is_file() check.
    let part_path = dest.with_extension(format!("{ext}.part"));
    let file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&part_path)
        .map_err(|e| e.to_string())?;
    let writer = BudgetWriter::new(file, budget.clone());
    let mut encoder = match zstd::Encoder::new(writer, 3) {
        Ok(encoder) => encoder,
        Err(error) => {
            let _ = std::fs::remove_file(&part_path);
            return Err(error.to_string());
        }
    };

    // every exit from here on removes the stub rather than leaving a
    // `.part` nothing will ever finish or sweep.
    let abandon = |e: String| {
        let _ = std::fs::remove_file(&part_path);
        Err(e)
    };
    let mut reader = resp.into_reader();
    let mut buf = [0u8; CHUNK_SIZE];
    let mut downloaded = 0u64;
    loop {
        let n = match reader.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) => return abandon(e.to_string()),
        };
        if let Err(e) = encoder.write_all(&buf[..n]) {
            return abandon(e.to_string());
        }
        downloaded += n as u64;
        // a dead receiver is the app shutting down, so stop moving
        // bytes for a cache nobody is going to read.
        if tx
            .send(DownloadEvent::Progress {
                id: id.to_string(),
                downloaded,
                total,
            })
            .is_err()
        {
            return abandon("download channel closed".to_string());
        }
    }
    let writer = match encoder.finish() {
        Ok(writer) => writer,
        Err(error) => return abandon(error.to_string()),
    };
    let lease = writer.into_lease();
    // a transcode that produced nothing still answers 200 with audio
    // headers; renaming that in would cache a zero-byte file that
    // `cached_path` treats as the download from then on.
    if downloaded == 0 {
        return abandon("the server sent an empty body".to_string());
    }
    if let Err(e) = std::fs::rename(&part_path, &dest) {
        return abandon(e.to_string());
    }
    Ok((dest, lease))
}

/// Downloads every `(id, label, url)` triple with at most
/// `DOWNLOAD_CONCURRENCY` transfers in flight, updating the cache
/// index and reporting outcomes over `tx` as each one finishes.
///
/// Workers pull from a shared counter rather than each striping a
/// fixed slice (the `subsonic::album_details` pattern, not
/// `enrich`'s), because track sizes vary by an order of magnitude: a
/// three-minute mp3 against a twenty-minute flac. Striping would leave
/// a worker that drew the small ones idle while another finishes the
/// rest of the batch alone.
///
/// Always called from inside the thread `spawn_tracks` itself spawns,
/// never directly from the render thread.
fn download_tracks(
    tracks: &[(String, String, String)],
    sink: &Sink,
    tx: &mpsc::Sender<DownloadEvent>,
    agent: &ureq::Agent,
) {
    let workers = DOWNLOAD_CONCURRENCY.min(tracks.len());
    if workers == 0 {
        return;
    }
    let collapse = tracks.len() > PER_TRACK_TOASTS;
    let tally = BatchTally::default();
    let next = AtomicUsize::new(0);
    thread::scope(|scope| {
        for _ in 0..workers {
            let (next, tally) = (&next, &tally);
            let tx = tx.clone();
            scope.spawn(move || loop {
                let index = next.fetch_add(1, Ordering::Relaxed);
                let Some((id, label, url)) = tracks.get(index) else {
                    return;
                };
                let event = match download_one(agent, &sink.budget, id, url, sink.dest_for, &tx) {
                    Ok((path, mut lease)) => {
                        let handoff = match sink.cache.lock() {
                            Ok(mut index) => {
                                let size = std::fs::metadata(&path)
                                    .map(|metadata| metadata.len())
                                    .unwrap_or(u64::MAX);
                                if size > sink.max_bytes {
                                    Err(format!(
                                        "download is {size} bytes, larger than the configured cache budget"
                                    ))
                                } else {
                                    let previous = index.recorded_cached_path(id);
                                    let mut replacement = index.clone();
                                    replacement.mark_cached(id, path.clone());
                                    let transient = index.transient_managed_bytes();
                                    let evicted = replacement
                                        .plan_budget_evictions_with_extra(sink.max_bytes, id, transient);
                                    match (sink.persist)(&replacement) {
                                        Err(error) => Err(format!(
                                            "could not save cache index; previous copy kept: {error}"
                                        )),
                                        Ok(()) => {
                                            *index = replacement;
                                            for old in evicted {
                                                if old != path {
                                                    release_after_removal(&old, &sink.budget);
                                                }
                                            }
                                            if let Some(previous) = previous
                                                .filter(|previous| previous != &path)
                                            {
                                                release_after_removal(&previous, &sink.budget);
                                            }
                                            lease.commit();
                                            Ok(())
                                        }
                                    }
                                }
                            }
                            Err(_) => Err("cache index is unavailable".to_string()),
                        };
                        match handoff {
                            Ok(()) => {
                                let label = if collapse {
                                    tally.done_label(label)
                                } else {
                                    label.clone()
                                };
                                DownloadEvent::Done {
                                    id: id.clone(),
                                    label,
                                }
                            }
                            Err(error) => {
                                if std::fs::remove_file(&path).is_err() {
                                    // A failed unlink leaves the bytes on disk, so
                                    // retain their reservation rather than claiming
                                    // the space was recovered.
                                    lease.commit();
                                }
                                let (label, error) = if collapse {
                                    tally.failure_text(label, error)
                                } else {
                                    (label.clone(), error)
                                };
                                DownloadEvent::Failed {
                                    id: id.clone(),
                                    label,
                                    error,
                                }
                            }
                        }
                    }
                    Err(error) => {
                        let (label, error) = if collapse {
                            tally.failure_text(label, error)
                        } else {
                            (label.clone(), error)
                        };
                        DownloadEvent::Failed {
                            id: id.clone(),
                            label,
                            error,
                        }
                    }
                };
                // same stance as the progress sends: a closed channel
                // means nothing is listening, so stop taking work.
                if tx.send(event).is_err() {
                    return;
                }
            });
        }
    });
}

fn release_after_removal(path: &std::path::Path, budget: &cache::CacheBudget) {
    let bytes = std::fs::metadata(path).map(|meta| meta.len()).unwrap_or(0);
    if std::fs::remove_file(path).is_ok() {
        budget.release(bytes);
    }
}

/// Entry point every caller actually uses: hands the whole batch off
/// to its own thread so even kicking off a whole discography's worth
/// of downloads returns to the render loop immediately rather than
/// blocking on `thread::scope` itself (that scope still waits for
/// every transfer to finish, just on this fresh thread, not the one
/// the UI is drawn from).
pub fn spawn_tracks(
    tracks: Vec<(String, String, String)>,
    cache: Arc<Mutex<CacheIndex>>,
    tx: mpsc::Sender<DownloadEvent>,
    max_bytes: u64,
) {
    if tracks.is_empty() {
        return;
    }
    thread::spawn(move || {
        let sink = Sink {
            cache: &cache,
            budget: cache::budget_for_download(max_bytes),
            dest_for: &cache::track_path,
            persist: &|idx: &CacheIndex| idx.save(),
            max_bytes,
        };
        download_tracks(&tracks, &sink, &tx, &batch_agent());
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader};
    use std::net::{TcpListener, TcpStream};
    use std::time::Instant;

    /// What the mock answers a stream request with.
    #[derive(Clone, Copy)]
    enum Mode {
        /// A body of this many bytes, dribbled out so transfers overlap.
        Body(usize),
        /// Headers promising a body, then nothing at all: the
        /// black-holing server the read timeout exists for.
        Stall,
        /// A server-side error on every request, so a whole batch fails
        /// the way an unreachable navidrome does.
        Failing,
        /// 200 with an xml error document, which is how subsonic
        /// refuses a `stream` request.
        ErrorDocument,
        /// 200 with audio headers and no body at all, which is what a
        /// transcode that produced nothing answers.
        Empty,
    }

    /// Minimal http/1.1 server for the download path, same shape as
    /// `subsonic`'s own fixture: canned responses plus counters. Tracks
    /// peak concurrency (how the batch cap is asserted) and accepted
    /// connections (how pooling is).
    struct ByteServer {
        url: String,
        requests: Arc<AtomicUsize>,
        connections: Arc<AtomicUsize>,
        inflight: Arc<AtomicUsize>,
        peak: Arc<AtomicUsize>,
    }

    impl ByteServer {
        fn start(mode: Mode) -> ByteServer {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let url = format!("http://{}", listener.local_addr().unwrap());
            let server = ByteServer {
                url,
                requests: Arc::new(AtomicUsize::new(0)),
                connections: Arc::new(AtomicUsize::new(0)),
                inflight: Arc::new(AtomicUsize::new(0)),
                peak: Arc::new(AtomicUsize::new(0)),
            };
            let (requests, connections, inflight, peak) = (
                server.requests.clone(),
                server.connections.clone(),
                server.inflight.clone(),
                server.peak.clone(),
            );
            // detached: the test binary exits without waiting on it
            thread::spawn(move || {
                for stream in listener.incoming().flatten() {
                    connections.fetch_add(1, Ordering::Relaxed);
                    let (requests, inflight, peak) =
                        (requests.clone(), inflight.clone(), peak.clone());
                    thread::spawn(move || {
                        serve(stream, mode, &requests, &inflight, &peak);
                    });
                }
            });
            server
        }

        fn url_for(&self, id: &str) -> String {
            format!("{}/stream?id={id}", self.url)
        }

        fn requests(&self) -> usize {
            self.requests.load(Ordering::Relaxed)
        }

        fn connections(&self) -> usize {
            self.connections.load(Ordering::Relaxed)
        }

        fn peak(&self) -> usize {
            self.peak.load(Ordering::Relaxed)
        }
    }

    /// One keep-alive connection: requests until the client hangs up.
    fn serve(
        stream: TcpStream,
        mode: Mode,
        requests: &AtomicUsize,
        inflight: &AtomicUsize,
        peak: &AtomicUsize,
    ) {
        let mut reader = BufReader::new(stream.try_clone().unwrap());
        let mut writer = stream;
        loop {
            let mut line = String::new();
            if reader.read_line(&mut line).unwrap_or(0) == 0 {
                return;
            }
            loop {
                let mut header = String::new();
                match reader.read_line(&mut header) {
                    Ok(0) => return,
                    Ok(_) if header.trim().is_empty() => break,
                    Ok(_) => {}
                    Err(_) => return,
                }
            }
            requests.fetch_add(1, Ordering::Relaxed);
            let now = inflight.fetch_add(1, Ordering::Relaxed) + 1;
            peak.fetch_max(now, Ordering::Relaxed);
            let served = respond(&mut writer, mode);
            inflight.fetch_sub(1, Ordering::Relaxed);
            if !served {
                return;
            }
        }
    }

    /// Writes one response; `false` once this connection is finished
    /// with (an error, or a body it deliberately never completes).
    fn respond(writer: &mut TcpStream, mode: Mode) -> bool {
        match mode {
            Mode::Failing => {
                let body = "nope";
                let head = format!(
                    "HTTP/1.1 500 Internal Server Error\r\nContent-Length: {}\r\n\r\n{body}",
                    body.len()
                );
                writer.write_all(head.as_bytes()).is_ok()
            }
            Mode::ErrorDocument => {
                let body = "<subsonic-response status=\"failed\" version=\"1.16.1\">\
                    <error code=\"40\" message=\"Wrong username or password\"/>\
                    </subsonic-response>";
                let head = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/xml\r\nContent-Length: {}\r\n\r\n{body}",
                    body.len()
                );
                writer.write_all(head.as_bytes()).is_ok()
            }
            Mode::Empty => {
                let head =
                    "HTTP/1.1 200 OK\r\nContent-Type: audio/mpeg\r\nContent-Length: 0\r\n\r\n";
                writer.write_all(head.as_bytes()).is_ok()
            }
            Mode::Stall => {
                let head =
                    "HTTP/1.1 200 OK\r\nContent-Type: audio/flac\r\nContent-Length: 4096\r\n\r\n";
                if writer.write_all(head.as_bytes()).is_err() {
                    return false;
                }
                let _ = writer.flush();
                // hold the connection open without sending the body;
                // the client's read timeout is what has to end this.
                thread::sleep(Duration::from_secs(20));
                false
            }
            Mode::Body(len) => {
                let head = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: audio/mpeg\r\nContent-Length: {len}\r\n\r\n"
                );
                if writer.write_all(head.as_bytes()).is_err() {
                    return false;
                }
                // two halves with a pause between them, so several
                // transfers are genuinely in flight at the same time
                // and `peak` means something.
                let half = len / 2;
                if writer.write_all(&vec![b'a'; half]).is_err() {
                    return false;
                }
                let _ = writer.flush();
                thread::sleep(Duration::from_millis(30));
                if writer.write_all(&vec![b'b'; len - half]).is_err() {
                    return false;
                }
                let _ = writer.flush();
                true
            }
        }
    }

    /// A temp dir that takes itself with it, so no test writes to the
    /// real `~/.cache/wim`.
    struct TempDir(PathBuf);

    impl TempDir {
        fn new(tag: &str) -> TempDir {
            let dir = std::env::temp_dir().join(format!(
                "wim-download-test-{tag}-{}-{:?}",
                std::process::id(),
                thread::current().id()
            ));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            TempDir(dir)
        }

        /// Stands in for `cache::track_path`.
        fn dest_for(&self) -> impl Fn(&str, &str) -> anyhow::Result<PathBuf> + Send + Sync + '_ {
            move |id: &str, ext: &str| Ok(self.0.join(format!("{id}.{ext}")))
        }

        fn entries(&self) -> Vec<String> {
            let mut names: Vec<String> = std::fs::read_dir(&self.0)
                .unwrap()
                .flatten()
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .collect();
            names.sort();
            names
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn batch(server: &ByteServer, count: usize) -> Vec<(String, String, String)> {
        (0..count)
            .map(|i| {
                (
                    format!("t{i}"),
                    format!("track {i} · Bladee"),
                    server.url_for(&format!("t{i}")),
                )
            })
            .collect()
    }

    /// One track whose url is shaped like `Client::stream_url`'s: the
    /// real one carries `u=`, `t=md5(password + salt)` and that salt,
    /// so these stand in for them and nothing real is ever in play.
    fn credentialed_track(base: &str) -> Vec<(String, String, String)> {
        vec![(
            "t0".to_string(),
            "track 0".to_string(),
            format!("{base}/rest/stream?id=t0&u=zzuser&t=zztoken&s=zzsalt&v=1.16.1&c=wim"),
        )]
    }

    /// `batch_agent` with a read timeout short enough to wait out. The
    /// only agent here that is not the app's own: every other test
    /// drives `batch_agent()` itself, so its pool and its timeouts are
    /// covered by behaviour rather than by a copy of them.
    fn stall_agent() -> ureq::Agent {
        ureq::AgentBuilder::new()
            .timeout_connect(CONNECT_TIMEOUT)
            .timeout_read(Duration::from_millis(300))
            .max_idle_connections_per_host(DOWNLOAD_CONCURRENCY)
            .build()
    }

    /// Runs a batch to completion and returns everything it reported.
    fn run(tracks: &[(String, String, String)], dir: &TempDir) -> Vec<DownloadEvent> {
        run_with(tracks, dir, &batch_agent())
    }

    fn run_with(
        tracks: &[(String, String, String)],
        dir: &TempDir,
        agent: &ureq::Agent,
    ) -> Vec<DownloadEvent> {
        let (tx, rx) = mpsc::channel();
        let cache = Arc::new(Mutex::new(CacheIndex::default()));
        let dest = dir.dest_for();
        let sink = Sink {
            cache: &cache,
            budget: cache::budget_for_download(u64::MAX),
            dest_for: &dest,
            // never the real index file
            persist: &|_: &CacheIndex| Ok(()),
            max_bytes: u64::MAX,
        };
        download_tracks(tracks, &sink, &tx, agent);
        drop(tx);
        rx.into_iter().collect()
    }

    #[test]
    fn an_active_transfer_reserves_disk_bytes_and_cleans_up_when_full() {
        let server = ByteServer::start(Mode::Body(8192));
        let dir = TempDir::new("budget-full");
        let cache = Arc::new(Mutex::new(CacheIndex::default()));
        let dest = dir.dest_for();
        let budget = Arc::new(cache::CacheBudget::new(1, 0));
        let sink = Sink {
            cache: &cache,
            budget: budget.clone(),
            dest_for: &dest,
            persist: &|_: &CacheIndex| Ok(()),
            max_bytes: 1,
        };
        let (tx, rx) = mpsc::channel();
        download_tracks(&batch(&server, 1), &sink, &tx, &batch_agent());
        drop(tx);
        let events = rx.into_iter().collect::<Vec<_>>();

        assert!(
            events.iter().any(|event| matches!(event,
                DownloadEvent::Failed { error, .. } if error.contains("budget is full")
            )),
            "unexpected events: {events:?}"
        );
        assert!(
            dir.entries().is_empty(),
            "no partial or unindexed file remains"
        );
        assert_eq!(budget.used_bytes(), 0, "failed reservations are released");
        assert!(cache.lock().unwrap().entries().next().is_none());
    }

    /// What a refusal has to look like from the outside.
    struct Refused {
        failed: usize,
        /// How many times the index was saved, which is also how many
        /// tracks were marked cached: `persist` is called under the same
        /// lock, right after `mark_cached`.
        saves: usize,
    }

    /// Runs one track from a `mode` server beside one that works. The
    /// good track is the positive control for the "nothing was cached"
    /// half: a run where nothing lands at all cannot tell a refusal
    /// from a batch that never ran.
    fn run_beside_a_good_track(mode: Mode, dir: &TempDir) -> Refused {
        let good = ByteServer::start(Mode::Body(512));
        let bad = ByteServer::start(mode);
        let tracks = vec![
            (
                "good".to_string(),
                "good track".to_string(),
                good.url_for("good"),
            ),
            (
                "bad".to_string(),
                "bad track".to_string(),
                bad.url_for("bad"),
            ),
        ];
        let saves = Arc::new(AtomicUsize::new(0));
        let counted = saves.clone();
        let cache = Arc::new(Mutex::new(CacheIndex::default()));
        let dest = dir.dest_for();
        let sink = Sink {
            cache: &cache,
            budget: cache::budget_for_download(u64::MAX),
            dest_for: &dest,
            // counted rather than written: never the real index file
            persist: &move |_: &CacheIndex| {
                counted.fetch_add(1, Ordering::Relaxed);
                Ok(())
            },
            max_bytes: u64::MAX,
        };
        let (tx, rx) = mpsc::channel();
        download_tracks(&tracks, &sink, &tx, &batch_agent());
        drop(tx);
        let events: Vec<DownloadEvent> = rx.into_iter().collect();
        let (done, failed) = outcomes(&events);
        assert_eq!(
            done,
            ["good track"],
            "the real body lands and nothing else does"
        );
        assert!(
            cache.lock().unwrap().cached_path("good").is_some(),
            "and still be in the index"
        );
        assert!(
            failed.iter().all(|(label, _)| *label == "bad track"),
            "only the refused track may fail"
        );
        Refused {
            failed: failed.len(),
            saves: saves.load(Ordering::Relaxed),
        }
    }

    fn outcomes(events: &[DownloadEvent]) -> (Vec<&str>, Vec<(&str, &str)>) {
        let mut done = Vec::new();
        let mut failed = Vec::new();
        for e in events {
            match e {
                DownloadEvent::Done { label, .. } => done.push(label.as_str()),
                DownloadEvent::Failed { label, error, .. } => {
                    failed.push((label.as_str(), error.as_str()))
                }
                DownloadEvent::Progress { .. } => {}
            }
        }
        (done, failed)
    }

    #[test]
    fn ext_for_content_type_recognizes_common_audio_types() {
        assert_eq!(ext_for_content_type(Some("audio/flac")), "flac");
        assert_eq!(ext_for_content_type(Some("audio/mpeg")), "mp3");
        assert_eq!(ext_for_content_type(Some("audio/ogg; codecs=opus")), "ogg");
        assert_eq!(ext_for_content_type(Some("audio/opus")), "opus");
        assert_eq!(ext_for_content_type(Some("audio/mp4")), "m4a");
        assert_eq!(ext_for_content_type(None), "audio");
        assert_eq!(ext_for_content_type(Some("text/html")), "audio");
    }

    #[test]
    fn an_error_document_is_told_from_a_binary_body() {
        assert!(is_error_document(Some("text/xml")));
        assert!(is_error_document(Some("text/xml; charset=utf-8")));
        assert!(is_error_document(Some("Text/HTML")));
        assert!(is_error_document(Some("application/json")));
        assert!(!is_error_document(Some("audio/mpeg")));
        assert!(
            !is_error_document(Some("application/octet-stream")),
            "a server that will not name the codec still sends audio"
        );
        assert!(
            !is_error_document(None),
            "a missing header is not evidence of anything"
        );
    }

    #[test]
    fn spawn_tracks_is_a_noop_on_an_empty_batch() {
        // mostly a smoke test that this doesn't spawn a thread (and
        // thus doesn't later send anything) when there's nothing to do.
        let (tx, rx) = mpsc::channel();
        let cache = Arc::new(Mutex::new(CacheIndex::default()));
        spawn_tracks(Vec::new(), cache, tx, u64::MAX);
        assert!(rx.try_recv().is_err());
    }

    /// The bug: pressing download on an artist handed the whole
    /// discography to `spawn_tracks`, which opened a thread and a
    /// socket per track. 40 tracks stands in for 800.
    #[test]
    fn a_batch_never_has_more_than_the_cap_in_flight() {
        let server = ByteServer::start(Mode::Body(4096));
        let dir = TempDir::new("cap");
        let tracks = batch(&server, 40);
        let events = run(&tracks, &dir);
        let (done, failed) = outcomes(&events);
        assert_eq!(done.len(), 40, "every track must still be downloaded");
        assert!(failed.is_empty(), "{failed:?}");
        assert_eq!(server.requests(), 40);
        assert!(
            server.peak() <= DOWNLOAD_CONCURRENCY,
            "{} transfers were in flight at once",
            server.peak()
        );
        assert_eq!(dir.entries().len(), 40, "{:?}", dir.entries());
    }

    #[test]
    fn every_download_is_stored_as_a_lossless_zstd_frame() {
        let server = ByteServer::start(Mode::Body(8192));
        let dir = TempDir::new("zstd-at-rest");
        let events = run(&batch(&server, 1), &dir);
        assert_eq!(outcomes(&events).0.len(), 1);
        let stored = dir.0.join("t0.mp3.zst");
        assert!(stored.is_file(), "downloads use compressed durable paths");
        let decoded = zstd::stream::decode_all(File::open(stored).unwrap()).unwrap();
        assert_eq!(
            decoded.len(),
            8192,
            "the original stream bytes are preserved"
        );
    }

    #[test]
    fn failed_replacement_keeps_the_existing_copy_and_index_entry() {
        let server = ByteServer::start(Mode::Body(4096));
        let dir = TempDir::new("replacement-failure");
        let old = dir.0.join("t0.mp3.zst");
        std::fs::write(&old, b"old verified cached copy").unwrap();
        let mut initial = CacheIndex::default();
        initial.mark_cached("t0", old.clone());
        let cache = Arc::new(Mutex::new(initial));
        let dest = dir.dest_for();
        let sink = Sink {
            cache: &cache,
            budget: cache::budget_for_download(u64::MAX),
            dest_for: &dest,
            persist: &|_: &CacheIndex| anyhow::bail!("injected persistence failure"),
            max_bytes: u64::MAX,
        };
        let (tx, rx) = mpsc::channel();
        download_tracks(&batch(&server, 1), &sink, &tx, &batch_agent());
        drop(tx);
        let events = rx.into_iter().collect::<Vec<_>>();
        assert_eq!(outcomes(&events).1.len(), 1);
        assert_eq!(std::fs::read(&old).unwrap(), b"old verified cached copy");
        assert_eq!(cache.lock().unwrap().cached_path("t0"), Some(old));
        assert_eq!(dir.entries(), ["t0.mp3.zst"]);
    }

    #[test]
    fn successful_replacement_commits_new_path_before_retiring_old_copy() {
        let server = ByteServer::start(Mode::Body(4096));
        let dir = TempDir::new("replacement-success");
        let old = dir.0.join("t0.mp3.zst");
        std::fs::write(&old, b"old verified cached copy").unwrap();
        let mut initial = CacheIndex::default();
        initial.mark_cached("t0", old.clone());
        let cache = Arc::new(Mutex::new(initial));
        let dest = dir.dest_for();
        let persisted = Arc::new(Mutex::new(None::<String>));
        let persisted_copy = persisted.clone();
        let sink = Sink {
            cache: &cache,
            budget: cache::budget_for_download(u64::MAX),
            dest_for: &dest,
            persist: &move |index: &CacheIndex| {
                *persisted_copy.lock().unwrap() = index
                    .cached_path("t0")
                    .map(|path| path.to_string_lossy().into_owned());
                Ok(())
            },
            max_bytes: u64::MAX,
        };
        let (tx, rx) = mpsc::channel();
        download_tracks(&batch(&server, 1), &sink, &tx, &batch_agent());
        drop(tx);
        let events = rx.into_iter().collect::<Vec<_>>();
        assert_eq!(outcomes(&events).0.len(), 1);
        let current = cache.lock().unwrap().cached_path("t0").unwrap();
        assert_ne!(current, old);
        assert_eq!(
            persisted.lock().unwrap().as_deref(),
            Some(current.to_str().unwrap())
        );
        assert!(current.is_file());
        assert!(!old.exists());
    }

    /// `ureq::get` builds a throwaway agent per call, so every track
    /// paid its own handshake: 20 tracks measured 20 connections. One
    /// agent for the whole batch is a flat `DOWNLOAD_CONCURRENCY`
    /// however many tracks there are. What this pins is the shared
    /// agent; the pool size on it is only worth a connection or two at
    /// four workers, and is pinned by the config test below instead.
    #[test]
    fn a_batch_reuses_connections_instead_of_one_per_track() {
        let server = ByteServer::start(Mode::Body(2048));
        let dir = TempDir::new("pool");
        let tracks = batch(&server, 20);
        run(&tracks, &dir);
        assert_eq!(server.requests(), 20);
        assert!(
            server.connections() <= DOWNLOAD_CONCURRENCY,
            "opened {} connections for 20 tracks",
            server.connections()
        );
    }

    /// A server that takes the request and then says nothing used to
    /// pin a worker forever and leave the `.part` stub behind, because
    /// the request had no timeout at all.
    #[test]
    fn a_black_holing_server_times_out_and_leaves_no_part_file() {
        let server = ByteServer::start(Mode::Stall);
        let dir = TempDir::new("stall");
        let tracks = batch(&server, 2);
        let started = Instant::now();
        let events = run_with(&tracks, &dir, &stall_agent());
        let elapsed = started.elapsed();
        let (done, failed) = outcomes(&events);
        assert!(done.is_empty(), "{done:?}");
        assert_eq!(failed.len(), 2, "both tracks have to report an outcome");
        assert!(
            elapsed < Duration::from_secs(10),
            "a stalled transfer pinned its worker for {elapsed:?}"
        );
        assert_eq!(
            dir.entries(),
            Vec::<String>::new(),
            "a timed-out transfer must not leave a .part stub"
        );
    }

    /// The stall test has to shorten the read timeout to wait it out,
    /// so this is what pins the real durations: without them a
    /// black-holing server pins a worker again, and a read timeout
    /// alongside an overall deadline would be overridden by it.
    #[test]
    fn the_batch_agent_carries_the_stall_timeouts_and_a_pool() {
        let debug = format!("{:?}", batch_agent());
        assert!(
            debug.contains(&format!("timeout_read: Some({READ_TIMEOUT:?})")),
            "{debug}"
        );
        assert!(
            debug.contains(&format!("timeout_connect: Some({CONNECT_TIMEOUT:?})")),
            "{debug}"
        );
        assert!(
            debug.contains(&format!("max_idle_per_host: {DOWNLOAD_CONCURRENCY}")),
            "{debug}"
        );
        // an overall deadline would abort a long legitimate transfer,
        // and it takes precedence over the read timeout if both are set.
        assert!(debug.contains("timeout: None"), "{debug}");
    }

    /// A small batch is still one toast per track: that is what a
    /// single auto-download has always done, and five is what `notify`
    /// can show at once anyway.
    #[test]
    fn a_small_batch_still_names_every_track() {
        let server = ByteServer::start(Mode::Body(512));
        let dir = TempDir::new("small");
        let tracks = batch(&server, PER_TRACK_TOASTS);
        let events = run(&tracks, &dir);
        let (mut done, _) = outcomes(&events);
        done.sort();
        assert_eq!(
            done,
            vec![
                "track 0 · Bladee",
                "track 1 · Bladee",
                "track 2 · Bladee",
                "track 3 · Bladee",
                "track 4 · Bladee",
            ]
        );
    }

    /// The other half of the keypress bug: 800 tracks against a server
    /// that is refusing them is 800 failure toasts. Identical text is
    /// what lets `notify::Notifications::push` collapse them into one
    /// counted box, so every event in a big batch has to carry the
    /// same label and error.
    #[test]
    fn a_big_failing_batch_reports_one_coalescable_outcome() {
        let server = ByteServer::start(Mode::Failing);
        let dir = TempDir::new("coalesce");
        let tracks = batch(&server, 30);
        let events = run(&tracks, &dir);
        let (done, failed) = outcomes(&events);
        assert!(done.is_empty(), "{done:?}");
        assert_eq!(failed.len(), 30, "every id still has to be resolved");
        let distinct: std::collections::HashSet<(&str, &str)> = failed.iter().copied().collect();
        assert_eq!(
            distinct.len(),
            1,
            "a batch must report as one message, not {} of them",
            distinct.len()
        );
        let (label, error) = failed[0];
        assert!(label.starts_with("track "), "{label}");
        assert!(!error.is_empty());
        // the ids stay per track: main.rs clears `downloading` and the
        // live progress box by id, not by label.
        let ids: std::collections::HashSet<&str> = events
            .iter()
            .filter_map(|e| match e {
                DownloadEvent::Failed { id, .. } => Some(id.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(ids.len(), 30);
    }

    /// ureq's text for both of its error variants starts with the whole
    /// request url, and a stream url carries `u=`, `t=md5(password +
    /// salt)` and that salt; main.rs toasts this string as "download
    /// failed: <track>: <error>", so a 500 from the server put a
    /// crackable hash of the password and its own salt on screen. Twin
    /// of `subsonic::request_failed`, which the json path already has.
    #[test]
    fn a_server_error_keeps_the_stream_url_out_of_the_failure() {
        let server = ByteServer::start(Mode::Failing);
        let dir = TempDir::new("redact-status");
        let tracks = credentialed_track(&server.url);
        let events = run(&tracks, &dir);
        let (_, failed) = outcomes(&events);
        assert_eq!(failed.len(), 1);
        let error = failed[0].1;
        // never interpolated into a failure message: this string is
        // exactly the one that reaches a toast.
        assert!(
            !error.contains('?'),
            "the request url is in the error, and it carries u=, t= and s="
        );
        assert!(!error.contains("zztoken"), "the auth token is in the error");
        assert!(!error.contains("zzsalt"), "its salt is in the error");
        assert!(!error.contains("zzuser"), "the username is in the error");
        assert!(error.contains("500"), "the status still has to survive");
    }

    /// The other variant: a server that is not there fails in transport,
    /// which puts the url in its text the same way.
    #[test]
    fn an_unreachable_server_keeps_the_stream_url_out_of_the_failure() {
        // bound to claim a port, then dropped, so nothing answers on it
        let dead = TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", dead.local_addr().unwrap());
        drop(dead);
        let dir = TempDir::new("redact-transport");
        let tracks = credentialed_track(&base);
        let events = run(&tracks, &dir);
        let (_, failed) = outcomes(&events);
        assert_eq!(failed.len(), 1);
        let error = failed[0].1;
        assert!(
            !error.contains('?'),
            "the request url is in the error, and it carries u=, t= and s="
        );
        assert!(!error.contains("zztoken"), "the auth token is in the error");
        assert!(!error.contains("zzsalt"), "its salt is in the error");
        assert!(
            error.to_lowercase().contains("connection"),
            "the cause still has to survive"
        );
    }

    /// Subsonic refuses a `stream` request with an error document rather
    /// than a transport failure: the spec's own wording is "returns
    /// binary data on success, or an XML document on error (in which
    /// case the HTTP content type will start with text/xml)". Trusting
    /// that body wrote a few hundred bytes of xml into the cache, marked
    /// the track cached and toasted "downloaded", so nothing ever
    /// re-downloaded it and the next offline play was mpv on an xml file.
    #[test]
    fn an_error_document_is_refused_instead_of_cached_as_the_track() {
        let dir = TempDir::new("error-doc");
        let refused = run_beside_a_good_track(Mode::ErrorDocument, &dir);
        assert_eq!(
            refused.failed, 1,
            "a wrong password answers 200: an error document is not a download"
        );
        assert_eq!(
            dir.entries(),
            ["good.mp3.zst"],
            "an error document must not be cached as the track"
        );
        assert_eq!(refused.saves, 1, "and the index must not learn about it");
    }

    /// A transcode that produced nothing (navidrome whose ffmpeg is
    /// missing) still answers 200 with audio headers and an empty body.
    /// Renaming that into place marked the track cached and toasted
    /// "downloaded", so the cache held a zero-byte file that nothing
    /// would ever replace: `cached_path` only asks whether the file is
    /// there, so every later play resolved to it.
    #[test]
    fn an_empty_body_is_refused_instead_of_cached_as_the_track() {
        let dir = TempDir::new("empty-body");
        let refused = run_beside_a_good_track(Mode::Empty, &dir);
        assert_eq!(refused.failed, 1, "zero bytes is not a track");
        assert_eq!(
            dir.entries(),
            ["good.mp3.zst"],
            "an empty body must not be cached as the track"
        );
        assert_eq!(refused.saves, 1, "and the index must not learn about it");
    }

    /// Successes collapse the same way, so downloading a 30-track
    /// discography is one "downloaded" box rather than 30 that evict
    /// each other.
    #[test]
    fn a_big_successful_batch_reports_one_coalescable_outcome() {
        let server = ByteServer::start(Mode::Body(256));
        let dir = TempDir::new("coalesce-ok");
        let tracks = batch(&server, 30);
        let events = run(&tracks, &dir);
        let (done, failed) = outcomes(&events);
        assert!(failed.is_empty(), "{failed:?}");
        assert_eq!(done.len(), 30);
        let distinct: std::collections::HashSet<&str> = done.iter().copied().collect();
        assert_eq!(distinct.len(), 1, "{distinct:?}");
    }

    /// Quitting mid-batch drops the receiver; the workers have to stop
    /// taking work rather than download a cache nobody will read.
    #[test]
    fn a_dropped_receiver_stops_the_batch() {
        let server = ByteServer::start(Mode::Body(4096));
        let dir = TempDir::new("dropped");
        let tracks = batch(&server, 40);
        let cache = Arc::new(Mutex::new(CacheIndex::default()));
        let dest = dir.dest_for();
        let sink = Sink {
            cache: &cache,
            budget: cache::budget_for_download(u64::MAX),
            dest_for: &dest,
            persist: &|_: &CacheIndex| Ok(()),
            max_bytes: u64::MAX,
        };
        let (tx, rx) = mpsc::channel();
        drop(rx);
        download_tracks(&tracks, &sink, &tx, &batch_agent());
        // exact, because the floor matters as much as the cap: a
        // worker cannot discover the channel is closed until its first
        // `Progress` send, so one request per worker is both the most
        // a dropped receiver may cost and proof the fan-out ran at
        // all. `<=` alone passed on a batch that spawned no workers.
        assert_eq!(
            server.requests(),
            DOWNLOAD_CONCURRENCY,
            "one in-flight request per worker and no more, out of {} tracks",
            tracks.len()
        );
    }

    /// Cost of a batch at the cap, against the mock's own dribbled
    /// body; prints rather than asserts, same convention as
    /// `search::bench_cpu_search`.
    ///
    /// Read the connection and peak columns, not the wall clock: a
    /// localhost mock has no shared link, so uncapped transfers all
    /// overlap for free there and 200 tracks measured 1.11s uncapped
    /// against 1.57s capped. What it does measure honestly is the
    /// footprint, 200 threads and 200 handshakes with a peak of 135 in
    /// flight, against 4 and 4.
    #[test]
    #[ignore = "timing, run explicitly"]
    fn bench_batch_download() {
        for count in [8usize, 40, 200] {
            let server = ByteServer::start(Mode::Body(64 * 1024));
            let dir = TempDir::new("bench");
            let tracks = batch(&server, count);
            let started = Instant::now();
            let events = run(&tracks, &dir);
            let elapsed = started.elapsed();
            let (done, _) = outcomes(&events);
            println!(
                "{count:>4} tracks: {:>3} done, {:>3} connections, peak {} in flight, {elapsed:?}",
                done.len(),
                server.connections(),
                server.peak(),
            );
        }
    }
}
