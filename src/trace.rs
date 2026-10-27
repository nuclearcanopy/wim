//! Startup timing, off unless `WIM_STARTUP_TRACE` is set to anything
//! but `0`/empty: every `mark` prints elapsed-since-`init` to stderr,
//! so time-to-first-frame is a measured number rather than a guess.
//! Lines go to stderr while the alternate screen is up, so redirect it
//! (`wim 2>/tmp/wim-trace.log`) to read them.

use std::io::Write;
use std::sync::atomic::{AtomicBool, Ordering};
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
