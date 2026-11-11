//! Development-only startup timing, off unless the `dev-telemetry` feature
//! and `WIM_STARTUP_TRACE` is set to a value other than `0` or empty:
//! every `mark` prints elapsed-since-`init` to stderr,
//! so time-to-first-frame is a measured number rather than a guess.
//! Lines go to stderr while the alternate screen is up, so redirect it
//! (`wim 2>/tmp/wim-trace.log`) to read them.

#[cfg(any(feature = "dev-telemetry", test))]
use std::io::Write;
#[cfg(feature = "dev-telemetry")]
use std::sync::atomic::AtomicBool;
#[cfg(any(feature = "dev-telemetry", test))]
use std::sync::atomic::{AtomicU64, Ordering};
#[cfg(feature = "dev-telemetry")]
use std::sync::OnceLock;
#[cfg(any(feature = "dev-telemetry", test))]
use std::time::Instant;

#[cfg(feature = "dev-telemetry")]
static ENABLED: AtomicBool = AtomicBool::new(false);
#[cfg(feature = "dev-telemetry")]
static ORIGIN: OnceLock<Instant> = OnceLock::new();

/// Called as the first statement in `main`; everything `mark` reports
/// is measured from here, which is as close to process start as this
/// process can observe.
#[cfg(feature = "dev-telemetry")]
pub fn init() {
    let on = std::env::var("WIM_STARTUP_TRACE")
        .map(|v| !v.is_empty() && v != "0")
        .unwrap_or(false);
    let _ = ORIGIN.set(Instant::now());
    ENABLED.store(on, Ordering::Relaxed);
}

/// Milliseconds since `init`, or 0 if it was never called (unit tests,
/// which don't go through `main`).
#[cfg(feature = "dev-telemetry")]
fn elapsed_ms() -> f64 {
    ORIGIN
        .get()
        .map(|o| o.elapsed().as_secs_f64() * 1000.0)
        .unwrap_or(0.0)
}

/// One startup milestone. Safe from any thread (the background scan and
/// sync threads both mark). Ordinary builds use the no-op implementation;
/// instrumented call sites are compiled out by `trace_mark!`.
#[cfg(feature = "dev-telemetry")]
pub fn mark(label: &str) {
    if !ENABLED.load(Ordering::Relaxed) {
        return;
    }
    let _ = writeln!(std::io::stderr(), "{}", format_mark(elapsed_ms(), label));
}

/// Fixed-width elapsed column so a run's marks line up when read as a
/// block; milliseconds (not micros) because everything measured here is
/// a terminal, filesystem, or network operation.
#[cfg(any(feature = "dev-telemetry", test))]
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

    #[cfg(not(feature = "dev-telemetry"))]
    #[test]
    fn default_build_compiles_trace_calls_out_without_evaluating_fields() {
        let calls = std::cell::Cell::new(0);
        crate::trace_mark!({
            calls.set(calls.get() + 1);
            "startup"
        });
        crate::trace_diag!(
            {
                calls.set(calls.get() + 1);
                "component"
            },
            "event",
            {
                calls.set(calls.get() + 1);
                &[]
            }
        );
        assert_eq!(calls.get(), 0);
    }
}

// --- runtime diagnostics ---
//
// Development-only runtime diagnostics, compiled out unless the feature
// is enabled and `WIM_DIAGNOSTICS_FILE` names a writable file. Every event
// is one line: elapsed milliseconds, a
// sequence number, `component/event`, then key=value fields. Written
// straight to the file (never the terminal, which mpv may own) and
// flushed per event, so a hung run still leaves everything up to the
// hang on disk.
//
// Redaction is enforced by the `Value` type, not by call-site
// discipline: there is no variant that carries text, so a token, URL,
// title, filename, account id or typed character cannot reach the
// file — passing one is a type error, not a code-review find. Fields
// are categories, counts, durations and indices only.

#[cfg(any(feature = "dev-telemetry", test))]
use std::fs::{File, OpenOptions};
#[cfg(any(feature = "dev-telemetry", test))]
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
#[cfg(any(feature = "dev-telemetry", test))]
use std::sync::Mutex;

#[cfg(any(feature = "dev-telemetry", test))]
static DIAG_FILE: Mutex<Option<(File, Instant)>> = Mutex::new(None);
#[cfg(any(feature = "dev-telemetry", test))]
static DIAG_SEQ: AtomicU64 = AtomicU64::new(0);

/// A field value a diagnostic event may carry. The whole privacy
/// contract is this enum having no text-carrying variant: everything
/// a leak could be made of (strings, ids, urls, user input) has to be
/// summarized into one of these before it can be logged.
#[derive(Debug, Clone, Copy)]
#[cfg(any(feature = "dev-telemetry", test))]
pub enum Value {
    /// A count, index, duration or size. Units belong in the field
    /// name (`ms=`, `-len=`), not the value.
    Num(f64),
    /// A boolean.
    Flag(bool),
    /// A word from a fixed vocabulary wim or mpv defines: a component,
    /// an event, a vo name, a page name. Never user text.
    Label(&'static str),
}

#[cfg(any(feature = "dev-telemetry", test))]
impl std::fmt::Display for Value {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            // f64's Display is the shortest round-trip form, so
            // whole numbers print bare (`250`, not `250.0`).
            Value::Num(n) => write!(f, "{n}"),
            Value::Flag(b) => write!(f, "{b}"),
            Value::Label(s) => write!(f, "{s}"),
        }
    }
}

/// Opens the diagnostics file (mode 0600, appended) when
/// `WIM_DIAGNOSTICS_FILE` names a path. Returns whether tracing is on,
/// so the caller can tell the user when the file could not be opened.
pub fn init_diagnostics() -> bool {
    #[cfg(any(feature = "dev-telemetry", test))]
    {
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
                // `mode` applies only to files this call creates. A file
                // that already exists keeps whatever it was made with, so
                // tighten it rather than trust it — the log is owner-only
                // by contract, not just by default.
                if let Ok(meta) = file.metadata() {
                    let mut perms = meta.permissions();
                    if perms.mode() & 0o777 != 0o600 {
                        perms.set_mode(0o600);
                        let _ = std::fs::set_permissions(&path, perms);
                    }
                }
                if let Ok(mut guard) = DIAG_FILE.lock() {
                    *guard = Some((file, Instant::now()));
                }
                true
            }
            Err(_) => false,
        }
    }
    #[cfg(not(any(feature = "dev-telemetry", test)))]
    {
        true
    }
}

#[cfg(any(feature = "dev-telemetry", test))]
pub fn diag_enabled() -> bool {
    DIAG_FILE
        .lock()
        .map(|guard| guard.is_some())
        .unwrap_or(false)
}
/// One runtime diagnostic event. Uses `try_lock` so a contended file can
/// never block the render loop — diagnostics losing an event is
/// acceptable, the ui freezing is not.
#[cfg(any(feature = "dev-telemetry", test))]
pub fn diag(component: &str, event: &str, fields: &[(&str, Value)]) {
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

/// Emits startup timings only in explicit development-telemetry builds.
/// The macro (rather than a no-op function) also avoids evaluating labels,
/// including dynamically formatted ones, in ordinary builds.
#[macro_export]
macro_rules! trace_mark {
    ($label:expr) => {{
        #[cfg(feature = "dev-telemetry")]
        {
            $crate::trace::mark($label);
        }
    }};
}

/// Emits a diagnostic event only in explicit development-telemetry builds;
/// field construction and formatting are compiled out otherwise.
#[macro_export]
macro_rules! trace_diag {
    ($($arg:tt)*) => {{
        #[cfg(feature = "dev-telemetry")]
        { $crate::trace::diag($($arg)*); }
    }};
}

#[cfg(test)]
mod diag_tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    /// The diag globals are process-wide, so the tests serialize on
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
            &[("ms", Value::Num(12.5)), ("outcome", Value::Label("ok"))],
        );
        diag("video", "arm", &[("vo", Value::Label("tct"))]);
        let contents = std::fs::read_to_string(&path).unwrap();
        let mut lines = contents.lines();
        let first = lines.next().unwrap();
        assert!(first.starts_with("[wim diag] "), "{first:?}");
        assert!(first.contains("#000000 resolve/done"), "{first:?}");
        assert!(first.contains("ms=12.5 outcome=ok"), "{first:?}");
        let second = lines.next().unwrap();
        assert!(second.contains("video/arm vo=tct"), "{second:?}");
        assert!(lines.next().is_none(), "exactly two events: {contents:?}");
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600, "diagnostics are owner-only");
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn whole_numbers_print_bare_and_flags_print_plain() {
        let _guard = DIAG_TEST_LOCK.lock().unwrap();
        let path = diag_file("numbers");
        std::env::set_var("WIM_DIAGNOSTICS_FILE", &path);
        assert!(init_diagnostics());
        diag(
            "loop",
            "slow-iteration",
            &[
                ("gap-ms", Value::Num(250.0)),
                ("overlay", Value::Flag(false)),
            ],
        );
        let contents = std::fs::read_to_string(&path).unwrap();
        assert!(
            contents.contains("gap-ms=250 overlay=false"),
            "got {contents:?}"
        );
        std::fs::remove_file(&path).ok();
    }

    /// A secret held in this test's own memory cannot appear in the
    /// log, because no `Value` variant accepts text. The compile-time
    /// guarantee is the enum; this is the belt to its braces: format
    /// every variant and prove none of them is a string escape hatch.
    #[test]
    fn no_value_variant_can_carry_text() {
        let secret = "wim-diag-secret-token-xyzzy";
        let values = [
            Value::Num(1.0),
            Value::Flag(true),
            Value::Label("file-loaded"),
        ];
        for v in values {
            let formatted = v.to_string();
            assert!(
                !formatted.contains(secret) && formatted.len() <= 12,
                "{formatted:?} is not from a fixed vocabulary"
            );
        }
    }

    /// `mode(0o600)` only applies at creation. A file created looser
    /// by someone else is tightened on open, so the log is owner-only
    /// even when it did not start that way.
    #[test]
    fn a_loose_existing_file_is_tightened_on_open() {
        let _guard = DIAG_TEST_LOCK.lock().unwrap();
        let path = diag_file("tighten");
        std::fs::write(&path, b"").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        std::env::set_var("WIM_DIAGNOSTICS_FILE", &path);
        assert!(init_diagnostics());
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600, "an existing loose log is tightened");
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
