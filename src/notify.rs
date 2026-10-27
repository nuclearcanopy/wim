//! Transient popup notifications: a dedicated stack, not text crammed
//! into a menu label or the one-line status bar. Alerts (a navidrome
//! server going unreachable, a failed reconnect, a playback error)
//! deserve something that actually grabs attention and then goes away
//! on its own, not a sentence buried in whatever's already on screen.

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

pub struct Notification {
    pub severity: Severity,
    pub message: String,
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
            shown_at: Instant::now(),
            lifetime,
        });
        if self.items.len() > MAX_STACKED {
            self.items.remove(0);
        }
    }

    /// Drops anything past its lifetime; called once per render frame
    /// rather than on a separate timer, since the render loop already
    /// ticks at least every 100ms (see the event poll timeout).
    pub fn expire(&mut self) {
        self.items.retain(|n| n.shown_at.elapsed() < n.lifetime);
    }

    pub fn visible(&self) -> &[Notification] {
        &self.items
    }
}
