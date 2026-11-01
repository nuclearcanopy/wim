//! Resuming playback (queue, position, volume) across restarts is
//! standard in podcast/audiobook apps and most modern music players;
//! not optional polish. State is saved as plain paths; resuming
//! reconstructs minimal `SearchItem`s from them (filename-derived title,
//! no artist/album) rather than depending on a prior library scan still
//! matching, so a moved/renamed library doesn't make resume fail outright
//!; paths that no longer exist are just silently skipped.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::library::{self, MediaKind};
use crate::search::SearchItem;

/// The four values a resume needs. **The field names are the on-disk
/// format**: `load` cannot tell a renamed field from a first launch
/// (an `Option` field just reads as `None`), so the shape is pinned by
/// `a_state_file_written_by_an_older_wim_still_resumes` against a
/// literal fixture rather than left to be discovered by a user whose
/// queue vanished.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct PlaybackState {
    pub now_playing_path: Option<String>,
    pub queue_paths: Vec<String>,
    pub position_secs: Option<f64>,
    pub volume: Option<f64>,
}

fn state_path() -> Result<PathBuf> {
    // state_dir() is Linux-only (XDG_STATE_HOME) per the dirs crate;
    // data_dir() is the next best cross-platform fit for "not config,
    // not cache, but not critical user data either."
    let dir = dirs::state_dir()
        .or_else(dirs::data_dir)
        .context("no state or data dir found for this platform")?;
    Ok(dir.join("wim").join("state.toml"))
}

pub fn save(state: &PlaybackState) -> Result<()> {
    save_to(&state_path()?, state)
}

fn save_to(path: &Path, state: &PlaybackState) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let toml = toml::to_string_pretty(state)?;
    fs::write(path, toml).with_context(|| format!("writing {}", path.display()))?;
    Ok(())
}

/// Best-effort load; a missing state file just means nothing to
/// resume. Prefer `load_reporting` at a call site that can say
/// something: this one cannot distinguish a first launch from a state
/// file that exists and will not parse.
pub fn load() -> Option<PlaybackState> {
    load_reporting().0
}

/// The state plus the reason there isn't any, when there is a reason
/// worth saying. Same shape and same purpose as
/// `config::load_reporting`: a state file that is *there* and will not
/// parse is someone's queue being dropped, and the previous `.ok()?`
/// made that indistinguishable from never having saved one.
pub fn load_reporting() -> (Option<PlaybackState>, Option<String>) {
    match state_path() {
        Ok(path) => load_from(&path),
        Err(e) => (
            None,
            Some(format!("couldn't find where to resume from: {e}")),
        ),
    }
}

fn load_from(path: &Path) -> (Option<PlaybackState>, Option<String>) {
    let raw = match fs::read_to_string(path) {
        Ok(raw) => raw,
        // nothing saved yet is the overwhelmingly common case and not
        // worth a word; anything else (a permission problem, a
        // directory where the file should be) is.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return (None, None),
        Err(e) => return (None, Some(format!("couldn't read {}: {e}", path.display()))),
    };
    match toml::from_str(&raw) {
        Ok(state) => (Some(state), None),
        Err(e) => (
            None,
            Some(format!(
                "{} is not a state file this wim understands, so there is \
                 nothing to resume: {e}",
                path.display()
            )),
        ),
    }
}

/// Rebuilds a minimal playable `SearchItem` from a saved path, or
/// `None` if the file no longer exists / isn't a recognized media type.
pub fn item_from_path(path_str: &str) -> Option<SearchItem> {
    let path = PathBuf::from(path_str);
    if !path.is_file() {
        return None;
    }
    let ext = path.extension()?.to_str()?;
    let title = library::filename_title(&path);
    match library::classify(ext)? {
        MediaKind::Music => Some(SearchItem::LocalMusic {
            path,
            title,
            artist: None,
            album: None,
        }),
        MediaKind::Video => Some(SearchItem::LocalVideo { path, title }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn nanos() -> u128 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    }

    /// Its own temp dir per test, so nothing here ever reads or writes
    /// the real `~/.local/state/wim`; same convention as
    /// `cache::tests::scratch_dir`.
    fn scratch_dir(label: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("wim-state-{label}-{}", nanos()));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn sample_state() -> PlaybackState {
        PlaybackState {
            now_playing_path: Some("/music/Bladee/Crest/01 - Wiking.flac".to_string()),
            queue_paths: vec![
                "/music/Bladee/Crest/02 - I Think.flac".to_string(),
                "/videos/Blade Runner 2049.mkv".to_string(),
            ],
            position_secs: Some(91.5),
            volume: Some(73.0),
        }
    }

    #[test]
    fn item_from_path_rejects_missing_files() {
        assert!(item_from_path("/definitely/does/not/exist.mp3").is_none());
    }

    #[test]
    fn item_from_path_rebuilds_a_minimal_local_music_item() {
        let dir = scratch_dir("music-item");
        let path = dir.join("03 - Who Goes There.mp3");
        fs::write(&path, "not real audio, just needs to exist").unwrap();

        let result = item_from_path(&path.to_string_lossy());

        match result {
            Some(SearchItem::LocalMusic {
                path: got,
                title,
                artist,
                album,
            }) => {
                // the one field this actually computes: the title comes
                // off the filename, never off a tag read.
                assert_eq!(title, "03 - Who Goes There");
                assert_eq!(got, path);
                // and by contract, not by accident: a resume must not
                // depend on a prior scan still matching, so no tags are
                // read here at all.
                assert!(artist.is_none(), "resume reads no tags");
                assert!(album.is_none(), "resume reads no tags");
            }
            other => panic!("expected LocalMusic, got {other:?}"),
        }
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn item_from_path_rebuilds_a_video_and_refuses_a_non_media_file() {
        let dir = scratch_dir("video-item");
        let video = dir.join("Blade Runner 2049.mkv");
        let other = dir.join("notes.txt");
        fs::write(&video, "x").unwrap();
        fs::write(&other, "x").unwrap();

        match item_from_path(&video.to_string_lossy()) {
            Some(SearchItem::LocalVideo { path, title }) => {
                assert_eq!(title, "Blade Runner 2049");
                assert_eq!(path, video);
            }
            other => panic!("expected LocalVideo, got {other:?}"),
        }
        assert!(
            item_from_path(&other.to_string_lossy()).is_none(),
            "an unrecognized extension is not resumable"
        );
        fs::remove_dir_all(&dir).ok();
    }

    /// `PlaybackState` had no round-trip test at all, and `load` ends
    /// in a swallowed error, so a field rename would have turned every
    /// saved queue into "nothing to resume" with nothing said.
    #[test]
    fn playback_state_survives_a_round_trip_through_the_disk() {
        let dir = scratch_dir("roundtrip");
        let path = dir.join("state.toml");
        let state = sample_state();
        save_to(&path, &state).unwrap();

        let (back, warning) = load_from(&path);
        assert_eq!(warning, None, "a file we just wrote must parse");
        assert_eq!(
            back.expect("must load back"),
            state,
            "every field has to survive, not just the ones a test happened to name"
        );
        fs::remove_dir_all(&dir).ok();
    }

    /// The real guard on the serde shape: the literal bytes an older
    /// wim wrote. Rename a field and this stops compiling or stops
    /// passing, instead of silently reading as an empty queue on every
    /// install.
    #[test]
    fn a_state_file_written_by_an_older_wim_still_resumes() {
        let dir = scratch_dir("fixture");
        let path = dir.join("state.toml");
        fs::write(
            &path,
            "now_playing_path = \"/music/Bladee/Crest/01 - Wiking.flac\"\n\
             queue_paths = [\"/music/Bladee/Crest/02 - I Think.flac\", \
             \"/videos/Blade Runner 2049.mkv\"]\n\
             position_secs = 91.5\n\
             volume = 73.0\n",
        )
        .unwrap();

        let (back, warning) = load_from(&path);
        assert_eq!(warning, None);
        assert_eq!(
            back.expect("the saved shape must still load"),
            sample_state()
        );
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn nothing_saved_yet_is_not_a_complaint() {
        let dir = scratch_dir("absent");
        let (back, warning) = load_from(&dir.join("state.toml"));
        assert!(back.is_none());
        assert_eq!(
            warning, None,
            "a first launch must not be reported as a problem"
        );
        fs::remove_dir_all(&dir).ok();
    }

    /// Alternating batches in one process, best of each, on the
    /// process cpu clock: both sides here are single-threaded. Same
    /// shape as `framesource::frame_budget::best_each`, different
    /// clock. This box's
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

    fn profile() -> &'static str {
        if cfg!(debug_assertions) {
            "DEBUG (not the shipping cost; re-run with --release)"
        } else {
            "release"
        }
    }

    /// A saved queue the size a real one reaches: `main.rs` notes ~200
    /// on a real session, and queueing an artist's discography makes
    /// it thousands.
    fn long_state(queued: usize) -> PlaybackState {
        PlaybackState {
            now_playing_path: Some("/home/u/Music/Bladee/Crest/01 - Wiking.flac".to_string()),
            queue_paths: (0..queued)
                .map(|i| {
                    format!(
                        "/home/u/Music/artist {}/album {}/{:02} - some song title.flac",
                        i / 170,
                        i / 17,
                        i % 17
                    )
                })
                .collect(),
            position_secs: Some(91.5),
            volume: Some(73.0),
        }
    }

    /// The resume state is read on the main thread before the first
    /// frame, so its parse is first-frame time. Is toml costing
    /// anything worth a format change at the sizes a real queue
    /// reaches?
    ///
    /// **Measured and declined.** json parses the same four fields
    /// 3.85-4.53x faster (cpu clock, one process), but the whole toml
    /// parse is 144us at the ~200 queued paths `main.rs` calls a real
    /// saved queue and 1.41ms at 2000, against a first frame of 82ms
    /// warm. That is 0.2% of a first frame for a format migration of
    /// the one file whose loss is a user's queue, and `save` is the
    /// only writer of it. The queue's real startup cost is not the
    /// parse: `main.rs` then calls `item_from_path` once per saved
    /// path, which is a `stat` each, on the main thread before the
    /// frame, while the matching mpv appends were already deferred
    /// past it.
    ///
    /// `cargo test --release -- --ignored --nocapture
    /// resume_state_parse`.
    #[test]
    #[ignore = "measurement, run explicitly"]
    fn resume_state_parse() {
        println!("--- resume state parse, {} build", profile());
        for queued in [200usize, 2_000] {
            let state = long_state(queued);
            let as_toml = toml::to_string_pretty(&state).unwrap();
            let as_json = serde_json::to_string(&state).unwrap();
            assert_eq!(
                toml::from_str::<PlaybackState>(&as_toml).unwrap(),
                serde_json::from_str::<PlaybackState>(&as_json).unwrap()
            );

            let (from_toml, from_json) = best_each_cpu(
                8,
                5,
                &mut || {
                    toml::from_str::<PlaybackState>(&as_toml).unwrap();
                },
                &mut || {
                    serde_json::from_str::<PlaybackState>(&as_json).unwrap();
                },
            );
            println!(
                "{queued:>5} queued  cpu: toml {:>7}B {from_toml:>9.1}us  json {:>7}B \
                 {from_json:>9.1}us  {:.2}x",
                as_toml.len(),
                as_json.len(),
                from_toml / from_json,
            );
        }
    }

    /// The case `.ok()?` hid: the file is there, it does not parse, and
    /// the user's queue is gone. That has to be said, not silently
    /// read as a first launch.
    #[test]
    fn a_state_file_that_will_not_parse_is_reported_not_swallowed() {
        let dir = scratch_dir("corrupt");
        let path = dir.join("state.toml");
        // the exact shape of a field rename: `queue_paths` is not
        // optional, so the key the current struct wants is missing.
        fs::write(&path, "queue_pathz = [\"/music/a.flac\"]\n").unwrap();

        let (back, warning) = load_from(&path);
        assert!(back.is_none());
        let warning = warning.expect("a queue dropped on load must be reported");
        assert!(
            warning.contains("nothing to resume"),
            "the message has to say what was lost: {warning}"
        );
        assert!(
            warning.contains("state.toml"),
            "and which file to look at: {warning}"
        );
        fs::remove_dir_all(&dir).ok();
    }
}
