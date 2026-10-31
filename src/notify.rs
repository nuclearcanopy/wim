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
    /// The outcome text, without the repeat count. The count is a
    /// separate piece on purpose: appended to this string it was the
    /// first thing a box's fit cut off, so an 80-cell terminal showed
    /// five identical sentences about 800 failures and no number
    /// anywhere. See `count_suffix`.
    pub message: String,
    /// `Some` only for progress notifications (see the module doc);
    /// used both to find the right one to update/finish and to decide
    /// whether `expire()` is even allowed to drop it on a timer.
    id: Option<String>,
    /// The text `message` gets rebuilt from, kept separately so it can
    /// be re-derived without the caller re-passing it: the track label
    /// for a progress notification (percentage appended each tick), the
    /// bare message for an ordinary one (repeat count appended).
    label: String,
    /// How many times this exact message has been pushed; `1` for
    /// anything that's only happened once. See `push`.
    repeats: u32,
    pub progress: Option<Progress>,
    shown_at: Instant,
    lifetime: Duration,
}

impl Notification {
    /// Seconds left before this one expires, for the countdown shown
    /// in its box; `anim::countdown_secs` rounds up and saturates at
    /// 0, so it never goes negative in the one frame between passing
    /// `lifetime` and `expire()` running.
    pub fn remaining_secs(&self) -> u64 {
        crate::anim::countdown_secs(self.shown_at.elapsed(), self.lifetime)
    }

    /// How many times this exact message has fired, and how it reads
    /// in a box: `" (x800)"`, or nothing at all for a one-off. The
    /// renderer reserves these cells ahead of fitting the message,
    /// which is the whole point of splitting them: the count is the
    /// only thing a coalesced box says that the message does not, so
    /// it is the one part that must never be the piece that gets cut.
    pub fn count_suffix(&self) -> String {
        if self.repeats > 1 {
            format!(" (x{})", self.repeats)
        } else {
            String::new()
        }
    }

    /// Message and count as one line, for a front-end with no box to
    /// put the count in: shellswim's scrollback is its only output.
    pub fn text(&self) -> String {
        format!("{}{}", self.message, self.count_suffix())
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

/// Whether `n` is a finished notification already saying this exact
/// thing, i.e. one a repeat belongs on. A download still in flight is
/// never one: its box is live and keyed by id.
fn same_outcome(n: &Notification, severity: Severity, message: &str) -> bool {
    n.progress.is_none() && n.severity == severity && n.label == message
}

#[derive(Default)]
pub struct Notifications {
    items: Vec<Notification>,
}

impl Notifications {
    pub fn push(&mut self, severity: Severity, message: impl Into<String>) {
        let message = message.into();
        // a failure that re-fires every frame (a dead mpv ipc socket
        // being the one that actually happened) would otherwise stack
        // five identical boxes and then keep shoving the rest of the
        // screen aside forever; collapse repeats onto the existing one
        // with a count and a fresh timer instead.
        if let Some(existing) = self
            .items
            .iter_mut()
            .find(|n| same_outcome(n, severity, &message))
        {
            existing.repeats += 1;
            existing.shown_at = Instant::now();
            existing.lifetime = lifetime_for(&existing.text());
            return;
        }
        let lifetime = lifetime_for(&message);
        self.items.push(Notification {
            severity,
            label: message.clone(),
            message,
            id: None,
            repeats: 1,
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
            repeats: 1,
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
        let Some(at) = self.items.iter().position(|n| n.id.as_deref() == Some(id)) else {
            self.push(severity, message);
            return;
        };
        // a batch past `download::PER_TRACK_TOASTS` hands every
        // outcome the same text so these coalesce, and the ones whose
        // live box was already evicted take the `push` above. a box
        // still on the stack has to join that count rather than
        // convert in place beside it: 800 failures ended as four
        // identical uncounted boxes next to one reading "(x796)".
        if self
            .items
            .iter()
            .any(|n| same_outcome(n, severity, &message))
        {
            self.items.remove(at);
            self.push(severity, message);
            return;
        }
        let n = &mut self.items[at];
        n.severity = severity;
        n.label = message.clone();
        n.message = message;
        n.id = None;
        n.repeats = 1;
        n.progress = None;
        n.shown_at = Instant::now();
        n.lifetime = lifetime_for(&n.message);
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

    /// Takes every pending message as text and clears them. For a
    /// front-end with no toast layer (shellswim), where an overlay
    /// would cover the thing it is talking about; the scrollback is
    /// the only output it has.
    pub fn drain_text(&mut self) -> Vec<String> {
        // `text`, not `message`: there is no box here to carry the
        // count in a reserved corner, so it goes in the line.
        let out = self
            .items
            .iter()
            .map(Notification::text)
            .collect::<Vec<_>>();
        self.items.clear();
        out
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
    fn the_same_message_collapses_into_one_box_with_a_count() {
        let mut n = Notifications::default();
        for _ in 0..4 {
            n.push(Severity::Error, "writing to mpv ipc socket");
        }
        assert_eq!(n.visible().len(), 1, "repeats must not stack up");
        // the count is its own piece, so a narrow box reserves cells
        // for it instead of fitting it off the end of the sentence.
        assert_eq!(n.visible()[0].message, "writing to mpv ipc socket");
        assert_eq!(n.visible()[0].count_suffix(), " (x4)");
        assert_eq!(n.visible()[0].text(), "writing to mpv ipc socket (x4)");
    }

    /// A one-off says nothing about counts, so nothing is reserved
    /// for it and the message gets the whole box.
    #[test]
    fn a_single_notification_carries_no_count_at_all() {
        let mut n = Notifications::default();
        n.push(Severity::Info, "starred");
        assert_eq!(n.visible()[0].count_suffix(), "");
        assert_eq!(n.visible()[0].text(), "starred");
    }

    /// What `download.rs` does over this api for a batch past its
    /// `PER_TRACK_TOASTS`: a progress box per track, then one outcome
    /// per track reusing the first one's text. `MAX_STACKED` evicts
    /// most of the live boxes, so most outcomes take the `push`
    /// fallback and coalesce; the handful whose box survived used to
    /// convert in place and sit beside the counted one as uncounted
    /// twins, so 800 failures rendered as five boxes, four of them
    /// silent about the other 796.
    fn failed_batch(tracks: usize, collapse: bool) -> Notifications {
        let mut n = Notifications::default();
        for i in 0..tracks {
            n.push_progress(i.to_string(), format!("Track {i}"));
        }
        for i in 0..tracks {
            let named = if collapse { 0 } else { i };
            n.finish_progress(
                &i.to_string(),
                Severity::Error,
                format!("download failed: Track {named}: 500"),
            );
        }
        n
    }

    #[test]
    fn a_coalesced_batch_is_one_box_counting_every_outcome() {
        for tracks in [6usize, 7, 50, 800] {
            let n = failed_batch(tracks, true);
            assert_eq!(
                n.visible().len(),
                1,
                "{tracks} identical outcomes left {} boxes",
                n.visible().len()
            );
            assert_eq!(
                n.visible()[0].count_suffix(),
                format!(" (x{tracks})"),
                "{tracks} outcomes"
            );
        }
    }

    /// Below the threshold every outcome still has its own text, so
    /// they stay separate boxes and none of them counts.
    #[test]
    fn a_small_batch_keeps_a_box_per_track() {
        let n = failed_batch(5, false);
        assert_eq!(n.visible().len(), 5);
        for item in n.visible() {
            assert_eq!(item.count_suffix(), "");
        }
        let texts: Vec<String> = n.visible().iter().map(Notification::text).collect();
        assert_eq!(texts.len(), 5, "and each names its own track");
        for i in 0..5 {
            assert!(
                texts.iter().any(|t| t.contains(&format!("Track {i}:"))),
                "track {i} is in no box: {texts:?}"
            );
        }
    }

    #[test]
    fn a_single_download_still_reports_once_with_no_count() {
        let n = failed_batch(1, false);
        assert_eq!(n.visible().len(), 1);
        assert_eq!(n.visible()[0].text(), "download failed: Track 0: 500");
    }

    #[test]
    fn a_repeat_refreshes_the_timer_rather_than_expiring_mid_burst() {
        let mut n = Notifications::default();
        n.push(Severity::Error, "server unreachable");
        n.items[0].shown_at = Instant::now() - Duration::from_secs(9999);
        n.push(Severity::Error, "server unreachable");
        n.expire();
        assert_eq!(n.visible().len(), 1);
    }

    #[test]
    fn different_messages_and_severities_stay_separate() {
        let mut n = Notifications::default();
        n.push(Severity::Error, "one");
        n.push(Severity::Error, "two");
        // same text, different severity: still its own notification,
        // since the icon and meaning differ.
        n.push(Severity::Info, "one");
        assert_eq!(n.visible().len(), 3);
    }

    #[test]
    fn a_burst_of_distinct_failures_is_still_capped() {
        let mut n = Notifications::default();
        for i in 0..MAX_STACKED + 3 {
            n.push(Severity::Error, format!("server {i} down"));
        }
        assert_eq!(n.visible().len(), MAX_STACKED);
        // the oldest dropped off, so the newest must have survived.
        assert!(n.visible().last().unwrap().message.contains("server 7"));
    }

    #[test]
    fn a_progress_box_is_never_coalesced_into() {
        let mut n = Notifications::default();
        n.push_progress("t1", "Some Song");
        n.push(Severity::Info, "downloading: Some Song");
        assert_eq!(
            n.visible().len(),
            2,
            "an in-flight download must keep its own live box"
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
