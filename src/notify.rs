//! Transient popup notifications: a dedicated stack, not text crammed
//! into a menu label or the one-line status bar. Alerts (a navidrome
//! server going unreachable, a failed reconnect, a playback error)
//! deserve something that actually grabs attention and then goes away
//! on its own, not a sentence buried in whatever's already on screen.
//!
//! Download progress (see `download.rs`) rides the same stack via a
//! second, parallel entry point (`push_progress`/`update_progress`/
//! `finish_progress`): these are keyed by track id rather than shown
//! once and forgotten, and don't expire on a timer like an ordinary
//! `push` does; they sit there live-updating until the download
//! actually finishes (or fails), then convert into an ordinary
//! fixed-lifetime notification same as any other.

use std::time::{Duration, Instant};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Severity {
    Info,
    Alert,
    Error,
}

impl Severity {
    /// Plain ascii, same reasoning as the result-row `[sk]` codes: even
    /// nominally-safe unicode glyphs (tried `⚠`/`✗` first) can still be
    /// emoji-presentation in some font/terminal combos. A letter can't.
    pub fn icon(self) -> char {
        match self {
            Severity::Info => 'i',
            Severity::Alert => '!',
            Severity::Error => 'x',
        }
    }
}

/// How far along a download in progress is; `total` is `None` when the
/// server's response never reported a `Content-Length` (some
/// transcoded streams don't), in which case the box just shows bytes
/// downloaded so far instead of a percentage/bar.
#[derive(Debug, Clone, Copy)]
pub struct Progress {
    pub downloaded: u64,
    pub total: Option<u64>,
}

/// Fixed-width block bar, same monochrome-bar convention as
/// `ui::visualizer::LevelMeter`'s own history fill; `None` total means
/// indeterminate progress, shown as a plain byte count instead of a
/// bar nothing can size correctly.
const BAR_WIDTH: usize = 10;

fn progress_suffix(p: Progress) -> String {
    match p.total {
        Some(total) if total > 0 => {
            let pct = (p.downloaded as f64 / total as f64).clamp(0.0, 1.0);
            let filled = (pct * BAR_WIDTH as f64).round() as usize;
            let filled = filled.min(BAR_WIDTH);
            let bar: String = "#".repeat(filled) + &"-".repeat(BAR_WIDTH - filled);
            format!("[{bar}] {:.0}%", pct * 100.0)
        }
        _ => format!("({} KB)", p.downloaded / 1024),
    }
}

pub struct Notification {
    pub severity: Severity,
    pub message: String,
    /// `Some` only for progress notifications (see the module doc);
    /// used both to find the right one to update/finish and to decide
    /// whether `expire()` is even allowed to drop it on a timer.
    id: Option<String>,
    /// The label a progress notification was created with, kept
    /// separately from `message` so `update_progress` can rebuild the
    /// bar/percentage each tick without the caller re-passing it.
    label: String,
    pub progress: Option<Progress>,
    shown_at: Instant,
    lifetime: Duration,
}

impl Notification {
    /// Seconds left before this one expires, for the countdown shown
    /// in its box; saturates at 0 rather than going negative in the
    /// one frame between it passing `lifetime` and `expire()` running.
    pub fn remaining_secs(&self) -> u64 {
        self.lifetime
            .saturating_sub(self.shown_at.elapsed())
            .as_secs()
    }

    /// Text for the bottom-border countdown/status: a live percentage
    /// while downloading (and never a stale value: an in-flight
    /// download never actually expires, see `expire()`), the normal
    /// `Ns` countdown otherwise.
    pub fn footer(&self) -> String {
        match self.progress {
            Some(Progress {
                downloaded,
                total: Some(total),
            }) if total > 0 => {
                let pct = (downloaded as f64 / total as f64) * 100.0;
                format!("{:.0}%", pct.clamp(0.0, 100.0))
            }
            Some(_) => "...".to_string(),
            None => format!("{}s", self.remaining_secs()),
        }
    }
}

const MIN_LIFETIME: Duration = Duration::from_secs(3);
const MAX_LIFETIME: Duration = Duration::from_secs(10);
/// Longer messages get more time to actually read, not a fixed
/// duration regardless of length; scales from the 3s floor by roughly
/// a second per 15 characters, capped at the 10s ceiling.
fn lifetime_for(message: &str) -> Duration {
    let extra = Duration::from_secs((message.chars().count() / 15) as u64);
    (MIN_LIFETIME + extra).min(MAX_LIFETIME)
}

/// Caps how many stack up at once; a burst of failures (e.g. every
/// configured server down at once) shouldn't paper over the whole
/// screen, the oldest just drops off.
const MAX_STACKED: usize = 5;

#[derive(Default)]
pub struct Notifications {
    items: Vec<Notification>,
}

impl Notifications {
    pub fn push(&mut self, severity: Severity, message: impl Into<String>) {
        let message = message.into();
        let lifetime = lifetime_for(&message);
        self.items.push(Notification {
            severity,
            message,
            id: None,
            label: String::new(),
            progress: None,
            shown_at: Instant::now(),
            lifetime,
        });
        if self.items.len() > MAX_STACKED {
            self.items.remove(0);
        }
    }

    /// Starts a persistent download-progress notification keyed by
    /// `id`; stays on screen (never expiring on its own) until
    /// `finish_progress(id, ...)` converts it into an ordinary one.
    pub fn push_progress(&mut self, id: impl Into<String>, label: impl Into<String>) {
        let id = id.into();
        let label = label.into();
        self.items.push(Notification {
            severity: Severity::Info,
            message: format!("downloading: {label}"),
            id: Some(id),
            label,
            progress: Some(Progress {
                downloaded: 0,
                total: None,
            }),
            shown_at: Instant::now(),
            lifetime: MIN_LIFETIME,
        });
        if self.items.len() > MAX_STACKED {
            self.items.remove(0);
        }
    }

    /// Updates the live byte count for the progress notification keyed
    /// by `id`; a no-op if it's already been finished/dropped (e.g. a
    /// stray late progress event after `finish_progress` already ran).
    pub fn update_progress(&mut self, id: &str, downloaded: u64, total: Option<u64>) {
        if let Some(n) = self.items.iter_mut().find(|n| n.id.as_deref() == Some(id)) {
            let progress = Progress { downloaded, total };
            n.progress = Some(progress);
            n.message = format!("downloading: {} {}", n.label, progress_suffix(progress));
        }
    }

    /// Converts the progress notification keyed by `id` into an
    /// ordinary fixed-lifetime one (success or failure); if no such
    /// notification is still around (unlikely, but e.g. it got pushed
    /// out by `MAX_STACKED`), falls back to just pushing a fresh one
    /// so the outcome is never silently lost.
    pub fn finish_progress(&mut self, id: &str, severity: Severity, message: impl Into<String>) {
        let message = message.into();
        if let Some(n) = self.items.iter_mut().find(|n| n.id.as_deref() == Some(id)) {
            n.severity = severity;
            n.message = message;
            n.id = None;
            n.progress = None;
            n.shown_at = Instant::now();
            n.lifetime = lifetime_for(&n.message);
        } else {
            self.push(severity, message);
        }
    }

    /// Drops anything past its lifetime; called once per render frame
    /// rather than on a separate timer, since the render loop already
    /// ticks at least every 100ms (see the event poll timeout). A
    /// download still in progress never expires this way, regardless
    /// of how long it's taking.
    pub fn expire(&mut self) {
        self.items
            .retain(|n| n.progress.is_some() || n.shown_at.elapsed() < n.lifetime);
    }

    pub fn visible(&self) -> &[Notification] {
        &self.items
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn progress_notification_never_expires_on_its_own() {
        let mut n = Notifications::default();
        n.push_progress("t1", "Some Song");
        // simulate a long time passing; an ordinary notification would
        // have long since expired by now.
        n.items[0].shown_at = Instant::now() - Duration::from_secs(9999);
        n.expire();
        assert_eq!(
            n.visible().len(),
            1,
            "an in-flight download must not expire"
        );
    }

    #[test]
    fn update_progress_rebuilds_the_message_with_a_bar() {
        let mut n = Notifications::default();
        n.push_progress("t1", "Some Song");
        n.update_progress("t1", 50, Some(100));
        assert!(n.visible()[0].message.contains("50%"));
        assert_eq!(n.visible()[0].footer(), "50%");
    }

    #[test]
    fn finish_progress_converts_to_an_expiring_notification() {
        let mut n = Notifications::default();
        n.push_progress("t1", "Some Song");
        n.finish_progress("t1", Severity::Info, "downloaded: Some Song");
        assert_eq!(n.visible()[0].message, "downloaded: Some Song");
        assert!(n.visible()[0].progress.is_none());

        n.items[0].shown_at = Instant::now() - Duration::from_secs(9999);
        n.expire();
        assert!(
            n.visible().is_empty(),
            "once finished, it should expire normally like any other notification"
        );
    }

    #[test]
    fn finish_progress_falls_back_to_a_fresh_push_if_the_id_is_gone() {
        let mut n = Notifications::default();
        n.finish_progress("missing", Severity::Error, "download failed: oops");
        assert_eq!(n.visible().len(), 1);
        assert_eq!(n.visible()[0].message, "download failed: oops");
    }
}
