//! Background track downloads into the local cache (see `cache.rs`):
//! byte-level transfer happens off the render thread, same reasoning as
//! `main::spawn_background_sync`. Progress is reported back over an
//! mpsc channel tagged by track id, polled once per frame in `main`'s
//! loop the same way `BackgroundSyncResult` already is.

use std::fs::File;
use std::io::{Read, Write};
use std::path::PathBuf;
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread;

use crate::cache::{self, CacheIndex};

/// One update about a download in flight, tagged by track id so
/// several interleaved downloads (a whole album requested at once) can
/// all report over the same channel without main.rs needing a
/// separate channel per track.
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

/// Downloads one track into the cache dir, reporting progress as
/// chunks arrive. Pure blocking I/O on whichever thread calls it;
/// every real caller is already inside a spawned thread (see
/// `spawn_tracks`), never the render thread.
fn download_one(
    id: &str,
    label: &str,
    url: &str,
    tx: &mpsc::Sender<DownloadEvent>,
) -> Option<PathBuf> {
    let fail = |tx: &mpsc::Sender<DownloadEvent>, error: String| {
        let _ = tx.send(DownloadEvent::Failed {
            id: id.to_string(),
            label: label.to_string(),
            error,
        });
    };

    let resp = match ureq::get(url).call() {
        Ok(r) => r,
        Err(e) => {
            fail(tx, e.to_string());
            return None;
        }
    };
    let total = resp
        .header("Content-Length")
        .and_then(|v| v.parse::<u64>().ok());
    let ext = ext_for_content_type(resp.header("Content-Type"));
    let dest = match cache::track_path(id, ext) {
        Ok(p) => p,
        Err(e) => {
            fail(tx, e.to_string());
            return None;
        }
    };
    if let Some(parent) = dest.parent() {
        if let Err(e) = std::fs::create_dir_all(parent) {
            fail(tx, e.to_string());
            return None;
        }
    }
    // downloads into a `.part` sibling first so a half-finished
    // transfer (app killed mid-download) never looks like a valid
    // cached file to `CacheIndex::cached_path`'s is_file() check.
    let part_path = dest.with_extension(format!("{ext}.part"));
    let mut file = match File::create(&part_path) {
        Ok(f) => f,
        Err(e) => {
            fail(tx, e.to_string());
            return None;
        }
    };

    let mut reader = resp.into_reader();
    let mut buf = [0u8; CHUNK_SIZE];
    let mut downloaded = 0u64;
    loop {
        let n = match reader.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) => {
                fail(tx, e.to_string());
                let _ = std::fs::remove_file(&part_path);
                return None;
            }
        };
        if let Err(e) = file.write_all(&buf[..n]) {
            fail(tx, e.to_string());
            let _ = std::fs::remove_file(&part_path);
            return None;
        }
        downloaded += n as u64;
        let _ = tx.send(DownloadEvent::Progress {
            id: id.to_string(),
            downloaded,
            total,
        });
    }
    drop(file);
    if let Err(e) = std::fs::rename(&part_path, &dest) {
        fail(tx, e.to_string());
        return None;
    }
    Some(dest)
}

/// Downloads every `(id, label, url)` triple concurrently, one thread
/// per track (same `thread::scope` discipline every other batch
/// network operation in this app already follows, see main.rs's own
/// module doc), updating `cache` and reporting progress over `tx` as
/// each one completes. Always called from inside the thread
/// `spawn_tracks` itself spawns, never directly from the render thread.
fn download_tracks(
    tracks: Vec<(String, String, String)>,
    cache: Arc<Mutex<CacheIndex>>,
    tx: mpsc::Sender<DownloadEvent>,
) {
    thread::scope(|scope| {
        for (id, label, url) in &tracks {
            let tx = tx.clone();
            let cache = cache.clone();
            scope.spawn(move || {
                if let Some(path) = download_one(id, label, url, &tx) {
                    if let Ok(mut idx) = cache.lock() {
                        idx.mark_cached(id, path);
                        let _ = idx.save();
                    }
                    let _ = tx.send(DownloadEvent::Done {
                        id: id.clone(),
                        label: label.clone(),
                    });
                }
            });
        }
    });
}

/// Entry point every caller actually uses: hands the whole batch off
/// to its own thread so even kicking off a whole album's worth of
/// downloads returns to the render loop immediately rather than
/// blocking on `thread::scope` itself (that scope still waits for
/// every spawned download to finish, just on this fresh thread, not
/// the one the UI is drawn from).
pub fn spawn_tracks(
    tracks: Vec<(String, String, String)>,
    cache: Arc<Mutex<CacheIndex>>,
    tx: mpsc::Sender<DownloadEvent>,
) {
    if tracks.is_empty() {
        return;
    }
    thread::spawn(move || download_tracks(tracks, cache, tx));
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
