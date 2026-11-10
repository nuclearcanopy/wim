//! Startup timing, off unless `WIM_STARTUP_TRACE` is set to anything
//! but `0`/empty: every `mark` prints elapsed-since-`init` to stderr,
//! so time-to-first-frame is a measured number rather than a guess.
//! Lines go to stderr while the alternate screen is up, so redirect it
//! (`wim 2>/tmp/wim-trace.log`) to read them.

use std::io::Write;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::OnceLock;
use std::time::Instant;

static ENABLED: AtomicBool = AtomicBool::new(false);
static ORIGIN: OnceLock<Instant> = OnceLock::new();

/// Called as the first statement in `main`; everything `mark` reports
/// is measured from here, which is as close to process start as this
/// process can observe.
pub fn init() {
    let on = std::env::var("WIM_STARTUP_TRACE")
        .map(|v| !v.is_empty() && v != "0")
        .unwrap_or(false);
    let _ = ORIGIN.set(Instant::now());
    ENABLED.store(on, Ordering::Relaxed);
}

/// Milliseconds since `init`, or 0 if it was never called (unit tests,
/// which don't go through `main`).
fn elapsed_ms() -> f64 {
    ORIGIN
        .get()
        .map(|o| o.elapsed().as_secs_f64() * 1000.0)
        .unwrap_or(0.0)
}

/// One startup milestone. Safe from any thread (the background scan and
/// sync threads both mark), and a single relaxed load when tracing is
/// off, so call sites don't need their own guard.
pub fn mark(label: &str) {
    if !ENABLED.load(Ordering::Relaxed) {
        return;
    }
    let _ = writeln!(std::io::stderr(), "{}", format_mark(elapsed_ms(), label));
}

/// Fixed-width elapsed column so a run's marks line up when read as a
/// block; milliseconds (not micros) because everything measured here is
/// a terminal, filesystem, or network operation.
fn format_mark(ms: f64, label: &str) -> String {
    format!("[wim startup] {ms:>9.3}ms  {label}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn marks_are_padded_to_one_column() {
        let short = format_mark(1.5, "a");
        let long = format_mark(1234.5678, "b");
        assert_eq!(
            short.find("ms  "),
            long.find("ms  "),
            "elapsed column must be fixed width: {short:?} vs {long:?}"
        );
        assert!(long.ends_with("1234.568ms  b"), "got {long:?}");
    }
}

// --- runtime diagnostics ---
//
// Opt-in runtime diagnostics, off unless `WIM_DIAGNOSTICS_FILE` names a
// writable file. Every event is one line: elapsed milliseconds, a
// sequence number, `component/event`, then pre-redacted key=value
// fields. Written straight to the file (never the terminal, which mpv
// may own) and flushed per event, so a hung run still leaves everything
// up to the hang on disk. Values are redacted at the call sites by
// construction: tokens, URLs, titles, filenames, account ids, and query
// text never reach a field — only categories, counts, durations, and
// indices.

use std::fs::{File, OpenOptions};
use std::os::unix::fs::OpenOptionsExt;
use std::sync::Mutex;

static DIAG_FILE: Mutex<Option<(File, Instant)>> = Mutex::new(None);
static DIAG_SEQ: AtomicU64 = AtomicU64::new(0);

/// Opens the diagnostics file (mode 0600, appended) when
/// `WIM_DIAGNOSTICS_FILE` names a path. Returns whether tracing is on,
/// so the caller can tell the user when the file could not be opened.
pub fn init_diagnostics() -> bool {
    let Ok(path) = std::env::var("WIM_DIAGNOSTICS_FILE") else {
        return false;
    };
    // a previous init's file never survives a new init: an unwritable
    // path means tracing is off, not "keep whatever was open".
    if let Ok(mut guard) = DIAG_FILE.lock() {
        *guard = None;
    }
    if path.is_empty() {
        return false;
    }
    match OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .open(&path)
    {
        Ok(file) => {
            if let Ok(mut guard) = DIAG_FILE.lock() {
                *guard = Some((file, Instant::now()));
            }
            true
        }
        Err(_) => false,
    }
}

pub fn diag_enabled() -> bool {
    DIAG_FILE
        .lock()
        .map(|guard| guard.is_some())
        .unwrap_or(false)
}

/// One runtime diagnostic event. Field values are redacted at the call
/// sites by construction; this function only formats and writes. Uses
/// `try_lock` so a contended file can never block the render loop —
/// diagnostics losing an event is acceptable, the ui freezing is not.
pub fn diag(component: &str, event: &str, fields: &[(&str, String)]) {
    let Ok(mut guard) = DIAG_FILE.try_lock() else {
        return;
    };
    let Some((file, origin)) = guard.as_mut() else {
        return;
    };
    let seq = DIAG_SEQ.fetch_add(1, Ordering::Relaxed);
    let ms = origin.elapsed().as_secs_f64() * 1000.0;
    let mut line = format!("[wim diag] {ms:>10.3}ms #{seq:06} {component}/{event}");
    for (key, value) in fields {
        line.push_str(&format!(" {key}={value}"));
    }
    let _ = writeln!(file, "{line}");
    let _ = file.flush();
}

#[cfg(test)]
mod diag_tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    /// The diag globals are process-wide, so the two tests serialize on
    /// this lock: each sets the env var, inits, asserts, and cleans up
    /// while holding it.
    static DIAG_TEST_LOCK: Mutex<()> = Mutex::new(());

    /// A private temp file for one test; the path is unique per call so
    /// parallel tests never share a writer.
    fn diag_file(tag: &str) -> std::path::PathBuf {
        let path = std::env::temp_dir().join(format!("wim-diag-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_file(&path);
        path
    }

    #[test]
    fn events_are_one_redacted_looking_line_with_sequence_and_elapsed() {
        let _guard = DIAG_TEST_LOCK.lock().unwrap();
        let path = diag_file("format");
        std::env::set_var("WIM_DIAGNOSTICS_FILE", &path);
        assert!(init_diagnostics());
        diag(
            "resolve",
            "done",
            &[("ms", "12.5".to_string()), ("outcome", "ok".to_string())],
        );
        diag("video", "arm", &[]);
        let contents = std::fs::read_to_string(&path).unwrap();
        let mut lines = contents.lines();
        let first = lines.next().unwrap();
        assert!(first.starts_with("[wim diag] "), "{first:?}");
        assert!(first.contains("#000000 resolve/done"), "{first:?}");
        assert!(first.contains("ms=12.5 outcome=ok"), "{first:?}");
        let second = lines.next().unwrap();
        assert!(second.contains("video/arm"), "{second:?}");
        assert!(lines.next().is_none(), "exactly two events: {contents:?}");
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600, "diagnostics are owner-only");
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn no_file_means_no_events_and_no_panic() {
        let _guard = DIAG_TEST_LOCK.lock().unwrap();
        std::env::set_var("WIM_DIAGNOSTICS_FILE", "/nonexistent-dir/wim-diag");
        assert!(!init_diagnostics());
        assert!(!diag_enabled());
        diag("torbox", "search-start", &[]); // must not panic
    }
}
