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

/// Where a finished download lands: `cache::track_path` in the app,
/// a temp dir under test, so nothing ever writes to the real cache.
type DestFor<'a> = &'a (dyn Fn(&str, &str) -> anyhow::Result<PathBuf> + Send + Sync);

/// What a batch writes through. Injected rather than reached for
/// directly so a test drives the whole path without touching
/// `~/.cache/wim`.
struct Sink<'a> {
    cache: &'a Arc<Mutex<CacheIndex>>,
    dest_for: DestFor<'a>,
    /// `CacheIndex::save` in the app. Called per finished track, not
    /// once per batch: the files are already on disk, and an index
    /// that forgot them would leak them past the next sweep.
    persist: &'a (dyn Fn(&CacheIndex) + Send + Sync),
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
    id: &str,
    url: &str,
    dest_for: DestFor,
    tx: &mpsc::Sender<DownloadEvent>,
) -> Result<PathBuf, String> {
    let resp = agent.get(url).call().map_err(|e| e.to_string())?;
    let total = resp
        .header("Content-Length")
        .and_then(|v| v.parse::<u64>().ok());
    let ext = ext_for_content_type(resp.header("Content-Type"));
    let dest = dest_for(id, ext).map_err(|e| e.to_string())?;
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    // downloads into a `.part` sibling first so a half-finished
    // transfer (app killed mid-download) never looks like a valid
    // cached file to `CacheIndex::cached_path`'s is_file() check.
    let part_path = dest.with_extension(format!("{ext}.part"));
    let mut file = File::create(&part_path).map_err(|e| e.to_string())?;

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
        if let Err(e) = file.write_all(&buf[..n]) {
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
    drop(file);
    if let Err(e) = std::fs::rename(&part_path, &dest) {
        return abandon(e.to_string());
    }
    Ok(dest)
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
                let event = match download_one(agent, id, url, sink.dest_for, &tx) {
                    Ok(path) => {
                        if let Ok(mut idx) = sink.cache.lock() {
                            idx.mark_cached(id, path);
                            (sink.persist)(&idx);
                        }
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
) {
    if tracks.is_empty() {
        return;
    }
    thread::spawn(move || {
        let sink = Sink {
            cache: &cache,
            dest_for: &cache::track_path,
            persist: &|idx: &CacheIndex| {
                let _ = idx.save();
            },
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
            dest_for: &dest,
            // never the real index file
            persist: &|_: &CacheIndex| {},
        };
        download_tracks(tracks, &sink, &tx, agent);
        drop(tx);
        rx.into_iter().collect()
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
    fn spawn_tracks_is_a_noop_on_an_empty_batch() {
        // mostly a smoke test that this doesn't spawn a thread (and
        // thus doesn't later send anything) when there's nothing to do.
        let (tx, rx) = mpsc::channel();
        let cache = Arc::new(Mutex::new(CacheIndex::default()));
        spawn_tracks(Vec::new(), cache, tx);
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
            dest_for: &dest,
            persist: &|_: &CacheIndex| {},
        };
        let (tx, rx) = mpsc::channel();
        drop(rx);
        download_tracks(&tracks, &sink, &tx, &batch_agent());
        assert!(
            server.requests() <= DOWNLOAD_CONCURRENCY,
            "kept fetching after the receiver was dropped: {} requests",
            server.requests()
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
