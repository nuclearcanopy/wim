//! Acting on the transport controls, and the two playback readings
//! the display asks for.
//!
//! Second step of main.rs's decomposition (see ROADMAP). The seam is
//! `controlsbar::Field`: everything in here either presses one of
//! those or reads playback to draw one, and none of it knows about
//! panes, menus or the input tiers.

use std::time::Instant;

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
    if field == Field::Crossfade {
        let _ = surf.open_crossfade();
        return;
    }
    // Every remaining control except the scrubber is a command, so it goes
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

pub fn now_playing_is_video(now_playing: &Option<SearchItem>) -> bool {
    matches!(
        now_playing,
        Some(
            SearchItem::LocalVideo { .. }
                | SearchItem::TorBoxFile { video: true, .. }
                | SearchItem::WatchSource { .. }
        )
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    /// What counts as video: this decides whether mpv gets a drawing
    /// vo *and* whether the workspace paints frames inline, so a
    /// remote video file is video here and a remote song is not — a
    /// TorBox song can carry a video stream and still must never take
    /// the screen.
    #[test]
    fn local_and_remote_video_count_and_music_does_not() {
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
        // a remote video file: the frame source decodes its resolved
        // url, and 'v' hands it to mpv like a local one. the client is
        // built offline the way queues.rs's tests build it — no ping,
        // no network, and the token is a throwaway.
        let dir = std::env::temp_dir().join(format!("wim-vidtest-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let token_path = dir.join("token");
        std::fs::write(&token_path, "unit-test-token").unwrap();
        let client =
            std::sync::Arc::new(crate::torbox::Client::from_token_file(&token_path).unwrap());
        let torbox = |video: bool| SearchItem::TorBoxFile {
            service: crate::torbox::Service::Torrent,
            item_id: 1,
            file_id: if video { 2 } else { 3 },
            title: "remote file".into(),
            video,
            client: client.clone(),
            resolved_url: None,
        };
        assert!(now_playing_is_video(&Some(torbox(true))));
        // a remote song, even from the same account: not video.
        assert!(!now_playing_is_video(&Some(torbox(false))));
        std::fs::remove_file(&token_path).ok();
        std::fs::remove_dir(&dir).ok();
    }
}
