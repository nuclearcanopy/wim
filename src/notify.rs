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
    /// Plain text-presentation glyphs, not emoji pictographs, same
    /// monochrome-safety reasoning as the result-row icons.
    pub fn icon(self) -> char {
        match self {
            Severity::Info => 'i',
            Severity::Alert => '⚠',
            Severity::Error => '✗',
        }
    }
}

pub struct Notification {
    pub severity: Severity,
    pub message: String,
    shown_at: Instant,
}

const LIFETIME: Duration = Duration::from_secs(5);
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
        self.items.push(Notification { severity, message: message.into(), shown_at: Instant::now() });
        if self.items.len() > MAX_STACKED {
            self.items.remove(0);
        }
    }

    /// Drops anything past its lifetime; called once per render frame
    /// rather than on a separate timer, since the render loop already
    /// ticks at least every 100ms (see the event poll timeout).
    pub fn expire(&mut self) {
        self.items.retain(|n| n.shown_at.elapsed() < LIFETIME);
    }

    pub fn visible(&self) -> &[Notification] {
        &self.items
    }
}
