//! Fullscreen video: mpv draws it, wim stops drawing.
//!
//! Nothing here rasterizes a frame, which is a division of labour and
//! not a limitation: mpv has terminal video outputs of its own (`tct`,
//! `kitty`) and this module only picks which one to ask for. wim's own
//! frames come from `crate::framesource`. The one thing that is
//! genuinely impossible is `screenshot-raw` over the json ipc socket,
//! and only that; see `docs/video.md` for both halves.

use ratatui::buffer::{Buffer, Cell};
use ratatui::layout::Rect;
use ratatui_image::picker::ProtocolType;

use crate::config::VideoOutput;

/// An mpv `vo` name wim is willing to set. `set_property vo <name>`
/// reports success for a name mpv has never heard of (verified on
/// 0.41.0) and then just never draws, so the value handed to mpv is
/// always one of these rather than a string from config.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Vo {
    /// Audio-only, and mpv's default here: draws nothing at all.
    Null,
    /// True-color half-blocks. Works in every terminal.
    Tct,
    /// Kitty graphics protocol. Needs a terminal that decodes it.
    Kitty,
}

impl Vo {
    pub fn mpv_name(self) -> &'static str {
        match self {
            Vo::Null => "null",
            Vo::Tct => "tct",
            Vo::Kitty => "kitty",
        }
    }
}

/// Which vo to hand mpv for a fullscreen video, or `None` for "do not
/// draw video at all".
///
/// `auto` resolves to kitty only on the terminal having *proved* it
/// speaks the kitty graphics protocol. mpv's kitty vo probes nothing:
/// pointed at a plain pty it writes its `ESC _G` escapes regardless and
/// reports no error, so an optimistic guess shows garbage rather than
/// falling back. `tct` is the safe answer for every other case,
/// including a probe that has not answered yet (`None`).
pub fn drawing_vo(want: VideoOutput, probed: Option<ProtocolType>) -> Option<Vo> {
    match want {
        // wim is painting it, so mpv must not; `showing` is the
        // decision that can tell those apart.
        VideoOutput::Off | VideoOutput::Ascii => None,
        VideoOutput::Tct => Some(Vo::Tct),
        VideoOutput::Kitty => Some(Vo::Kitty),
        VideoOutput::Auto if probed == Some(ProtocolType::Kitty) => Some(Vo::Kitty),
        VideoOutput::Auto => Some(Vo::Tct),
    }
}

/// Everything that has to hold before mpv is allowed to draw over the
/// ui, gathered here so the handover is one decision instead of a
/// condition spread through the main loop.
#[derive(Debug, Clone, Copy)]
pub struct Screen {
    /// 'v' is on.
    pub fullscreen: bool,
    /// The player is collapsed to the mini bar.
    pub minimized: bool,
    /// A menu is open: onboarding, nav manager, help, settings,
    /// keybinds or eq.
    pub overlay: bool,
    /// What is playing is a `SearchItem::LocalVideo`.
    pub is_video: bool,
}

/// Which vo mpv should be drawing with right now, or `None` for "the
/// screen belongs to wim".
///
/// `is_video` is the load-bearing one: it is what keeps ordinary music
/// playback on `vo=null` forever, since handing the screen to mpv for
/// an audio file blanks the whole ui to show a picture that does not
/// exist. `overlay` takes the screen back on purpose too; a menu nobody
/// can see is a hang as far as the user is concerned.
/// What should be on screen where the video goes.
///
/// Three answers, not two: "mpv draws nothing" and "there is no video"
/// used to be the same `None`, and they are not. In `Ascii` wim paints
/// the cells itself and mpv stays on `vo=null` as the audio clock, so
/// a caller that only asked "which vo" could not tell that apart from
/// a paused music track.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Showing {
    /// Not fullscreen, minimized, an overlay is up, or what is playing
    /// is not a video.
    Nothing,
    /// mpv draws over the ui with this vo.
    Mpv(Vo),
    /// wim pulls frames and paints them as cells; see
    /// `framesource::FrameSource`.
    Ascii,
}

impl Showing {
    /// The vo to hand mpv, which is `None` for ascii as much as for
    /// nothing: in both cases mpv must not draw.
    pub fn mpv_vo(self) -> Option<Vo> {
        match self {
            Showing::Mpv(vo) => Some(vo),
            Showing::Nothing | Showing::Ascii => None,
        }
    }

    pub fn is_ascii(self) -> bool {
        self == Showing::Ascii
    }
}

/// The same screen gates apply to ascii as to a real vo: 'v' is on,
/// the player is not collapsed, no menu is over it, and what plays is
/// a video. Non-fullscreen video is the separate sub-rect item.
pub fn showing(screen: Screen, want: VideoOutput, probed: Option<ProtocolType>) -> Showing {
    if !screen.fullscreen || screen.minimized || screen.overlay || !screen.is_video {
        return Showing::Nothing;
    }
    if want == VideoOutput::Ascii {
        return Showing::Ascii;
    }
    match drawing_vo(want, probed) {
        Some(vo) => Showing::Mpv(vo),
        None => Showing::Nothing,
    }
}

/// What the loop should do about mpv's video output this frame.
///
/// The handover was four nested conditions doing ipc inline, so the one
/// rule that is easy to get wrong could not be tested at all: a *vo
/// change* has to re-arm (bouncing would keep the old one) while a
/// *size change* on the same vo has to bounce (re-arming would be the
/// same work plus a reclaim). Same split as `framesource::plan`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Move {
    /// Nothing to do, which is every frame once the screen has settled.
    Nothing,
    /// Hand mpv the screen, at this size. Also the answer when the vo
    /// itself changes, since the new one has to be armed from scratch.
    Arm { vo: Vo, size: (u16, u16) },
    /// Take the screen back and put mpv on `vo=null`.
    Disarm,
    /// Same vo, new size. mpv reads the vo's size once, at init, so
    /// this is a null-geometry-vo bounce rather than a resize; wim is
    /// not drawing, so the screen it flickers through belongs to
    /// nobody in between.
    Rebounce { vo: Vo, size: (u16, u16) },
}

/// `want` is `showing(..).mpv_vo()`, `have` is the vo mpv is drawing
/// with right now, and `armed` is the size it was given when it was
/// armed.
pub fn handover(want: Option<Vo>, have: Option<Vo>, size: (u16, u16), armed: (u16, u16)) -> Move {
    match want {
        // the vo changed (or there was none), so arming is the only
        // move: a bounce would re-init the one being replaced.
        Some(vo) if have != Some(vo) => Move::Arm { vo, size },
        Some(vo) if size != armed => Move::Rebounce { vo, size },
        Some(_) => Move::Nothing,
        // only when there is something to take back, so an ordinary
        // frame with no video is no work and no ipc.
        None if have.is_some() => Move::Disarm,
        None => Move::Nothing,
    }
}

/// The mode actually in force: a runtime override if the user set one,
/// the config otherwise.
pub fn effective(configured: VideoOutput, over: Option<VideoOutput>) -> VideoOutput {
    over.unwrap_or(configured)
}

/// What the ascii toggle leaves the override at.
///
/// Turning it off clears the override rather than naming a mode, so a
/// user who edits `video_output` and comes back gets the new value; the
/// one case that cannot clear is ascii being what the config says, and
/// there `Auto` is the honest other half of the toggle.
pub fn toggle_ascii(configured: VideoOutput, over: Option<VideoOutput>) -> Option<VideoOutput> {
    if effective(configured, over) == VideoOutput::Ascii {
        (configured == VideoOutput::Ascii).then_some(VideoOutput::Auto)
    } else {
        Some(VideoOutput::Ascii)
    }
}

pub fn wanted(screen: Screen, want: VideoOutput, probed: Option<ProtocolType>) -> Option<Vo> {
    showing(screen, want, probed).mpv_vo()
}

/// A rect wim deliberately leaves unpainted, so mpv's own output can
/// live there while the rest of the ui carries on.
///
/// **The mechanism is ratatui's own, and needs no change to it.**
/// `Terminal::flush` diffs the *previous buffer* against the *current
/// buffer* and writes only the cells that differ; it has no knowledge
/// of what is actually on the terminal. So a rect left untouched in
/// two consecutive frames holds identical cells in both buffers, the
/// diff yields nothing for it, and whatever mpv painted there is never
/// written over. docs/video.md used to record this as the blocker for
/// sub-rect video on the theory that the diff would "restore" the
/// cells; it cannot, because it does not know what they look like.
///
/// What is actually required is the opposite of clever: wim must not
/// draw in the rect, and something must notice when that promise is
/// broken so mpv can be asked to redraw. `painted` is that notice.
/// Unused in the binary until the render path keeps its half of the
/// bargain and skips the rect, which is a `main.rs` change queued
/// behind the library split. The mechanism and its proof are the
/// deliverable here; wiring it is the separate, blocked step. Same
/// `cfg_attr` shape `shellsong::NoCandidates` uses for a type that is
/// real but not yet reached.
#[cfg_attr(not(test), allow(dead_code))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Hole {
    pub area: Rect,
}

#[cfg_attr(not(test), allow(dead_code))]
impl Hole {
    pub fn new(area: Rect) -> Hole {
        Hole { area }
    }

    /// Whether anything is painted inside the hole in `buf`.
    ///
    /// True means the promise was broken this frame: an overlay, a
    /// toast, or a full repaint after a resize wrote into it. Those
    /// cells now differ from the previous frame, so the diff *will*
    /// emit them and mpv's picture there is about to be destroyed.
    /// Nothing here can repair that; the host's answer is to ask mpv
    /// to redraw once whatever covered the hole has gone.
    pub fn painted(&self, buf: &Buffer) -> bool {
        let vis = self.area.intersection(buf.area);
        for y in vis.y..vis.y + vis.height {
            for x in vis.x..vis.x + vis.width {
                if buf.cell((x, y)).is_some_and(|c| *c != Cell::EMPTY) {
                    return true;
                }
            }
        }
        false
    }
}

/// The ascii toggle's two directions, which are not symmetric: off
/// means "whatever the config says now", and that cannot be expressed
/// when the config itself says ascii.
/// The vo handover, whose two non-obvious cases are that a vo change
/// re-arms and a size change on the same vo bounces. Neither was
/// reachable by a test while this lived inline in the frame loop.
#[cfg(test)]
mod handover_tests {
    use super::*;

    const SMALL: (u16, u16) = (80, 24);
    const BIG: (u16, u16) = (200, 60);

    /// Both idle cases. `Disarm` only when there is something to take
    /// back: otherwise every music frame would set `vo=null` and
    /// reclaim the terminal it already owns.
    #[test]
    fn a_settled_screen_is_no_work() {
        assert_eq!(handover(None, None, SMALL, SMALL), Move::Nothing);
        assert_eq!(
            handover(Some(Vo::Kitty), Some(Vo::Kitty), SMALL, SMALL),
            Move::Nothing
        );
        assert_eq!(
            handover(None, Some(Vo::Kitty), SMALL, SMALL),
            Move::Disarm,
            "mpv is drawing and should not be"
        );
    }

    /// Arming carries the size, so the first arm at a new size does not
    /// need a bounce behind it.
    #[test]
    fn taking_the_screen_arms_at_the_current_size() {
        assert_eq!(
            handover(Some(Vo::Tct), None, BIG, SMALL),
            Move::Arm {
                vo: Vo::Tct,
                size: BIG
            }
        );
    }

    /// A different vo has to be armed, not bounced: a bounce re-inits
    /// the vo mpv already has, which is the one being replaced.
    #[test]
    fn a_changed_vo_is_armed_rather_than_bounced() {
        assert_eq!(
            handover(Some(Vo::Tct), Some(Vo::Kitty), SMALL, SMALL),
            Move::Arm {
                vo: Vo::Tct,
                size: SMALL
            }
        );
        // and the size being stale at the same time changes nothing:
        // arming sets both, so there is no second move owed.
        assert_eq!(
            handover(Some(Vo::Tct), Some(Vo::Kitty), BIG, SMALL),
            Move::Arm {
                vo: Vo::Tct,
                size: BIG
            }
        );
    }

    /// Same vo, new size. The size that matters is the one mpv was
    /// armed with, not the one it was asked for, because the vo reads
    /// it once at init.
    #[test]
    fn a_resize_under_the_same_vo_is_a_bounce() {
        assert_eq!(
            handover(Some(Vo::Kitty), Some(Vo::Kitty), BIG, SMALL),
            Move::Rebounce {
                vo: Vo::Kitty,
                size: BIG
            }
        );
    }

    /// A resize with nothing armed is nothing: there is no vo holding
    /// a stale size, and bouncing one that does not exist would hand
    /// mpv a drawing vo the screen state never asked for.
    #[test]
    fn a_resize_while_nothing_is_armed_does_nothing() {
        assert_eq!(handover(None, None, BIG, SMALL), Move::Nothing);
    }
}

#[cfg(test)]
mod toggle_tests {
    use super::*;

    /// The ordinary case: no override until the user asks, and asking
    /// twice puts it back to following the config.
    #[test]
    fn the_toggle_returns_to_the_config_it_came_from() {
        let on = toggle_ascii(VideoOutput::Auto, None);
        assert_eq!(on, Some(VideoOutput::Ascii));
        assert_eq!(effective(VideoOutput::Auto, on), VideoOutput::Ascii);

        let off = toggle_ascii(VideoOutput::Auto, on);
        assert_eq!(off, None, "off has to stop overriding, not pick a mode");
        assert_eq!(effective(VideoOutput::Auto, off), VideoOutput::Auto);
    }

    /// Clearing the override here would turn ascii straight back on,
    /// so the toggle would do nothing at all; `Auto` is the only other
    /// mode that can still produce a picture.
    #[test]
    fn turning_it_off_works_even_when_the_config_says_ascii() {
        let off = toggle_ascii(VideoOutput::Ascii, None);
        assert_eq!(off, Some(VideoOutput::Auto));
        assert_ne!(effective(VideoOutput::Ascii, off), VideoOutput::Ascii);

        let on = toggle_ascii(VideoOutput::Ascii, off);
        assert_eq!(effective(VideoOutput::Ascii, on), VideoOutput::Ascii);
    }

    /// The property both cases above are instances of: one press
    /// flips whether ascii is in force, from either starting point and
    /// whatever the config says. A toggle that no-ops for one
    /// configured mode is the bug worth pinning.
    #[test]
    fn one_press_flips_whether_ascii_is_in_force() {
        for configured in [
            VideoOutput::Auto,
            VideoOutput::Off,
            VideoOutput::Kitty,
            VideoOutput::Tct,
            VideoOutput::Ascii,
        ] {
            let mut over = None;
            for press in 0..4 {
                let before = effective(configured, over) == VideoOutput::Ascii;
                over = toggle_ascii(configured, over);
                let after = effective(configured, over) == VideoOutput::Ascii;
                assert_ne!(
                    before, after,
                    "{configured:?} did not flip on press {press}"
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 'v' on, player up, no menu, playing a video.
    fn playing_video() -> Screen {
        Screen {
            fullscreen: true,
            minimized: false,
            overlay: false,
            is_video: true,
        }
    }

    /// The whole point of `auto`: kitty on a proven-kitty terminal, tct
    /// everywhere else. A probe that answered with some *other*
    /// protocol, or did not answer at all, must not be read as kitty;
    /// mpv would emit graphics escapes into a terminal that renders
    /// them as mojibake.
    #[test]
    fn auto_picks_kitty_only_when_the_probe_proved_kitty() {
        assert_eq!(
            drawing_vo(VideoOutput::Auto, Some(ProtocolType::Kitty)),
            Some(Vo::Kitty)
        );
        for other in [
            None,
            Some(ProtocolType::Halfblocks),
            Some(ProtocolType::Sixel),
            Some(ProtocolType::Iterm2),
        ] {
            assert_eq!(
                drawing_vo(VideoOutput::Auto, other),
                Some(Vo::Tct),
                "{other:?} is not kitty"
            );
        }
    }

    /// An explicitly named output is an explicit ask and ignores the
    /// probe, both ways: `tct` on a kitty terminal (the maintainer
    /// wanting half-blocks), and `kitty` on a terminal that never
    /// answered (a multiplexer that swallows the query).
    #[test]
    fn a_named_output_overrides_the_probe() {
        assert_eq!(
            drawing_vo(VideoOutput::Tct, Some(ProtocolType::Kitty)),
            Some(Vo::Tct)
        );
        assert_eq!(drawing_vo(VideoOutput::Kitty, None), Some(Vo::Kitty));
        assert_eq!(
            drawing_vo(VideoOutput::Kitty, Some(ProtocolType::Halfblocks)),
            Some(Vo::Kitty)
        );
    }

    /// `off` never draws, whatever the terminal can do.
    #[test]
    fn off_never_draws() {
        for probed in [None, Some(ProtocolType::Kitty), Some(ProtocolType::Sixel)] {
            assert_eq!(drawing_vo(VideoOutput::Off, probed), None, "{probed:?}");
        }
    }

    /// `null` is the name mpv wants for "stop drawing"; the audio-only
    /// default the app spawns with and returns to.
    #[test]
    fn vo_names_are_mpvs_own() {
        assert_eq!(Vo::Null.mpv_name(), "null");
        assert_eq!(Vo::Tct.mpv_name(), "tct");
        assert_eq!(Vo::Kitty.mpv_name(), "kitty");
    }

    /// The regression that matters most: wim is mostly a music player,
    /// and a drawing vo over a music track would blank the entire ui to
    /// render a picture that does not exist. Audio stays on `vo=null`
    /// however loudly the rest of the state asks for fullscreen.
    #[test]
    fn a_music_track_never_gets_a_drawing_vo() {
        let music = Screen {
            is_video: false,
            ..playing_video()
        };
        for want in [
            VideoOutput::Auto,
            VideoOutput::Tct,
            VideoOutput::Kitty,
            VideoOutput::Off,
        ] {
            assert_eq!(
                wanted(music, want, Some(ProtocolType::Kitty)),
                None,
                "{want:?} over audio"
            );
        }
    }

    /// Ascii is an opt-in, and `Auto` never picks it.
    ///
    /// A real protocol beats a cell grid wherever one exists, so
    /// probing into ascii would be a downgrade. It is for terminals
    /// with no image protocol at all, where the alternative is no
    /// video.
    #[test]
    fn auto_never_resolves_to_ascii() {
        for probed in [
            None,
            Some(ProtocolType::Kitty),
            Some(ProtocolType::Sixel),
            Some(ProtocolType::Iterm2),
        ] {
            let got = showing(playing_video(), VideoOutput::Auto, probed);
            assert!(
                !got.is_ascii(),
                "auto picked ascii with {probed:?} available: {got:?}"
            );
            assert!(
                matches!(got, Showing::Mpv(_)),
                "auto has to land on a real vo: {got:?}"
            );
        }
        assert!(showing(playing_video(), VideoOutput::Ascii, None).is_ascii());
    }

    /// The distinction the third variant exists for: "mpv draws
    /// nothing" is not "there is no video".
    ///
    /// Both answer `None` to "which vo", which is why a caller that
    /// only asked that could not tell an ascii frame pull from a
    /// paused music track.
    #[test]
    fn ascii_hands_mpv_no_vo_and_is_still_not_nothing() {
        let ascii = showing(playing_video(), VideoOutput::Ascii, None);
        let off = showing(playing_video(), VideoOutput::Off, None);
        let audio = showing(
            Screen {
                is_video: false,
                ..playing_video()
            },
            VideoOutput::Ascii,
            None,
        );

        assert_eq!(ascii.mpv_vo(), None, "wim paints it, so mpv must not");
        assert_eq!(off.mpv_vo(), None);
        assert_eq!(audio.mpv_vo(), None);

        assert!(ascii.is_ascii());
        assert!(!off.is_ascii(), "off means off, not painted by wim");
        assert!(
            !audio.is_ascii(),
            "a music track must not start a frame pull"
        );
        assert_eq!(off, Showing::Nothing);
        assert_eq!(audio, Showing::Nothing);
    }

    /// Ascii is behind the same screen gates as a real vo, so a menu
    /// over it stops the pull rather than being painted under frames.
    #[test]
    fn ascii_is_behind_the_same_screen_gates() {
        for (label, screen) in [
            (
                "not fullscreen",
                Screen {
                    fullscreen: false,
                    ..playing_video()
                },
            ),
            (
                "minimized",
                Screen {
                    minimized: true,
                    ..playing_video()
                },
            ),
            (
                "an overlay is up",
                Screen {
                    overlay: true,
                    ..playing_video()
                },
            ),
        ] {
            assert_eq!(
                showing(screen, VideoOutput::Ascii, None),
                Showing::Nothing,
                "{label}: the pull has to stop"
            );
        }
    }

    /// `wanted` is `showing(..).mpv_vo()`, so the two cannot drift;
    /// every existing caller asks the old question and gets the old
    /// answer.
    #[test]
    fn wanted_still_answers_exactly_what_showing_does() {
        for want in [
            VideoOutput::Auto,
            VideoOutput::Tct,
            VideoOutput::Kitty,
            VideoOutput::Ascii,
            VideoOutput::Off,
        ] {
            for probed in [None, Some(ProtocolType::Kitty)] {
                for screen in [
                    playing_video(),
                    Screen {
                        overlay: true,
                        ..playing_video()
                    },
                ] {
                    assert_eq!(
                        wanted(screen, want, probed),
                        showing(screen, want, probed).mpv_vo(),
                        "{want:?} {probed:?}"
                    );
                }
            }
        }
    }

    /// The screen goes back to wim whenever something of wim's needs to
    /// be on it: a menu, or a player collapsed to the mini bar.
    #[test]
    fn an_overlay_or_a_minimized_player_takes_the_screen_back() {
        assert_eq!(
            wanted(playing_video(), VideoOutput::Tct, None),
            Some(Vo::Tct),
            "the baseline has to draw, or the rest proves nothing"
        );
        for blocked in [
            Screen {
                overlay: true,
                ..playing_video()
            },
            Screen {
                minimized: true,
                ..playing_video()
            },
            Screen {
                fullscreen: false,
                ..playing_video()
            },
        ] {
            assert_eq!(wanted(blocked, VideoOutput::Tct, None), None, "{blocked:?}");
        }
    }

    // ---------------------------------------------------------- hole

    fn filled(area: Rect, ch: char) -> Buffer {
        let mut buf = Buffer::empty(area);
        for y in area.y..area.y + area.height {
            for x in area.x..area.x + area.width {
                if let Some(cell) = buf.cell_mut((x, y)) {
                    cell.set_char(ch);
                }
            }
        }
        buf
    }

    /// The claim the whole design rests on, proved against ratatui's
    /// own diff rather than argued: two consecutive frames that both
    /// leave the hole alone emit **nothing** for it, so mpv's picture
    /// there is never written over. `Terminal::flush` diffs previous
    /// buffer against current buffer, so it cannot restore cells it
    /// has never seen.
    #[test]
    fn an_untouched_hole_emits_nothing_on_the_next_frame() {
        let area = Rect::new(0, 0, 20, 6);
        let hole = Hole::new(Rect::new(4, 1, 10, 3));

        // two frames that differ outside the hole and are identical
        // inside it (both untouched, i.e. still empty).
        let mut prev = Buffer::empty(area);
        prev.set_string(0, 0, "sidebar one", ratatui::style::Style::default());
        let mut next = Buffer::empty(area);
        next.set_string(0, 0, "sidebar two", ratatui::style::Style::default());

        let updates = prev.diff(&next);
        assert!(!updates.is_empty(), "the row outside the hole did change");
        for (x, y, _) in &updates {
            assert!(
                !hole.area.contains(ratatui::layout::Position::new(*x, *y)),
                "cell ({x}, {y}) is inside the hole and would be written over"
            );
        }
    }

    /// The first frame after a hole opens is different and has to be:
    /// whatever was there is still in the previous buffer, so the diff
    /// emits blanks over it. mpv draws after that, and from then on the
    /// test above holds.
    #[test]
    fn the_frame_that_opens_a_hole_still_clears_what_was_there() {
        let area = Rect::new(0, 0, 8, 3);
        let hole = Hole::new(Rect::new(2, 1, 4, 1));

        let prev = filled(area, 'x');
        let next = Buffer::empty(area);

        let updates = prev.diff(&next);
        let inside = updates
            .iter()
            .filter(|(x, y, _)| hole.area.contains(ratatui::layout::Position::new(*x, *y)))
            .count();
        assert_eq!(
            inside,
            hole.area.area() as usize,
            "the opening frame must clear the hole before mpv draws into it"
        );
    }

    #[test]
    fn painted_is_false_while_the_promise_is_kept() {
        let area = Rect::new(0, 0, 10, 4);
        let hole = Hole::new(Rect::new(2, 1, 4, 2));
        let mut buf = Buffer::empty(area);
        // drawing outside the hole is exactly what is allowed.
        buf.set_string(0, 0, "chrome", ratatui::style::Style::default());
        assert!(!hole.painted(&buf));
    }

    /// An overlay over the hole is the case that breaks it: those cells
    /// will differ next frame, so the diff emits them and mpv's picture
    /// is gone. The host needs to know, which is what this reports.
    #[test]
    fn painted_notices_an_overlay_over_the_hole() {
        let area = Rect::new(0, 0, 10, 4);
        let hole = Hole::new(Rect::new(2, 1, 4, 2));
        let mut buf = Buffer::empty(area);
        buf.set_string(3, 2, "!", ratatui::style::Style::default());
        assert!(hole.painted(&buf), "a toast landed in the hole unnoticed");
    }

    /// A hole hanging off the edge of the buffer must not panic; the
    /// pane can be larger than the terminal mid-resize.
    #[test]
    fn a_hole_outside_the_buffer_is_clipped_not_a_panic() {
        let buf = Buffer::empty(Rect::new(0, 0, 4, 2));
        assert!(!Hole::new(Rect::new(10, 10, 5, 5)).painted(&buf));
        assert!(!Hole::new(Rect::new(2, 1, 50, 50)).painted(&buf));
    }
}
