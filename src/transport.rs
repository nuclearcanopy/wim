//! Acting on the transport controls, and the two playback readings
//! the display asks for.
//!
//! Second step of main.rs's decomposition (see ROADMAP). The seam is
//! `controlsbar::Field`: everything in here either presses one of
//! those or reads playback to draw one, and none of it knows about
//! panes, menus or the input tiers.

use std::time::{Duration, Instant};

use crate::anim;
use crate::core::{apply_op, CoreCtx, Surface};
use crate::mpv;
use crate::notify::{Notifications, Severity};
use crate::search::SearchItem;
use crate::ui::controlsbar::{Field, Scrub};

/// Where enter (or a click) on the scrubber opens its drag: at the
/// current position, which is the only place a key could mean, or at a
/// fraction of the duration, which is where a pointer landed.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ScrubAt {
    Position,
    Fraction(f64),
}

/// The scrubber's drag state, which is the one bar control with no op
/// behind it.
///
/// Everything else this struct used to carry (the app, the queue, the
/// cache, crossfade, the eq menu, the gap) was only here to serve six
/// arms that each duplicated an op; `core::op_for_field` plus
/// `apply_op` replaced them, and the fields went with them.
pub struct TransportCtx<'a> {
    pub scrub: &'a mut Option<Scrub>,
    pub scrub_pending: &'a mut bool,
    pub scrub_sent_at: &'a mut Option<Instant>,
    /// mpv's last reported position and duration, sampled once per
    /// frame by the loop rather than asked for again here.
    pub elapsed_secs: Option<f64>,
    pub duration_secs: Option<f64>,
}

/// The one place a transport control is acted on. Enter on the bottom
/// bar, enter on the maximized player and a mouse click all land here,
/// so the three can never drift into doing different things to the
/// same glyph; `controlsbar::Field` is the whole alphabet.
pub fn activate_transport(
    field: Field,
    at: ScrubAt,
    core: &mut CoreCtx,
    surf: &mut dyn Surface,
    ctx: &mut TransportCtx,
) {
    // every control but the scrubber *is* a command, so it goes
    // through the command. that is not only deduplication: `apply_op`
    // is where the undo tree records, so this is what puts a clicked
    // loop or shuffle in the history alongside a typed one.
    if let Some(op) = crate::core::op_for_field(field) {
        apply_op(op, core, surf);
        return;
    }
    // the scrubber. duration is captured by the caller, once, so each
    // step can clamp without asking mpv again; nothing loaded means
    // nothing to scrub, so the drag just doesn't open.
    if let Some(elapsed) = ctx.elapsed_secs {
        let open = Scrub {
            target_secs: elapsed,
            duration_secs: ctx.duration_secs,
        };
        // opening where it already is has nothing to seek to; opening
        // where a pointer landed does, so only that one leaves a
        // target for the coalescer to send.
        *ctx.scrub = Some(match at {
            ScrubAt::Position => open,
            ScrubAt::Fraction(frac) => open.at_fraction(frac),
        });
        *ctx.scrub_pending = matches!(at, ScrubAt::Fraction(_));
        *ctx.scrub_sent_at = None;
    }
}

/// Finishes an open drag: one absolute seek to the target the bar has
/// been drawing, then the drag closes. Shared by the keyboard's
/// enter/esc and the mouse's button release, so "committed" means the
/// same thing however the drag was being held.
pub fn commit_scrub(
    target: Scrub,
    player: &mpv::Mpv,
    notifications: &mut Notifications,
    scrub: &mut Option<Scrub>,
    scrub_pending: &mut bool,
    scrub_sent_at: &mut Option<Instant>,
) {
    if let Err(e) = player.seek(target.target_secs, false) {
        notifications.push(Severity::Error, format!("error seeking: {e}"));
    }
    *scrub = None;
    *scrub_pending = false;
    *scrub_sent_at = None;
}

/// Current output level, 0..1, for the now-playing level meter. Reads
/// the `astats` tap `build_af_filter` (eq.rs) puts at the end of the af
/// chain, which mpv pushes (`mpv::PROP_LEVEL_RMS`) rather than being
/// asked for: this used to be a blocking IPC round trip per frame. No
/// fresh reading (paused, stopped, or an mpv that doesn't resolve the
/// property) falls back to a calm idle breathe instead of silently
/// showing a dead meter, driven off `elapsed` like every other
/// animation.
pub fn audio_level(playback: &mpv::Playback, elapsed: Duration) -> f32 {
    playback.level().unwrap_or_else(|| {
        (anim::pulse(elapsed, anim::LEVEL_FALLBACK_PERIOD) * 0.6 + 0.05).clamp(0.0, 1.0)
    })
}

pub fn now_playing_is_video(now_playing: &Option<SearchItem>) -> bool {
    matches!(now_playing, Some(SearchItem::LocalVideo { .. }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    /// Only a local video file is video. A remote song can carry a
    /// video stream and is still not what this question is about:
    /// it decides whether mpv gets a drawing vo.
    #[test]
    fn only_a_local_video_counts_as_video() {
        let video = SearchItem::LocalVideo {
            path: PathBuf::from("/v/clip.mkv"),
            title: "clip".into(),
        };
        assert!(now_playing_is_video(&Some(video)));
        assert!(!now_playing_is_video(&None));
        assert!(!now_playing_is_video(&Some(SearchItem::LocalMusic {
            path: PathBuf::from("/m/a.flac"),
            title: "a".into(),
            artist: None,
            album: None,
        })));
    }

    /// With no reading from mpv the meter breathes rather than sitting
    /// dead, and the fallback has to stay inside the meter's range.
    #[test]
    fn a_missing_level_breathes_instead_of_reading_zero() {
        let playback = mpv::Playback::default();
        for ms in [0u64, 250, 500, 900, 1500] {
            let level = audio_level(&playback, Duration::from_millis(ms));
            assert!((0.0..=1.0).contains(&level), "{ms}ms gave {level}");
        }
        // and it actually moves, which is the point of a fallback
        // rather than a constant.
        let a = audio_level(&playback, Duration::ZERO);
        let b = audio_level(&playback, anim::LEVEL_FALLBACK_PERIOD / 2);
        assert!((a - b).abs() > 0.01, "{a} vs {b} is not a breathe");
    }
}
