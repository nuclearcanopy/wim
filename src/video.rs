//! Fullscreen video: mpv draws it, wim stops drawing.
//!
//! Nothing here rasterizes a frame. mpv has terminal video outputs of
//! its own (`tct`, `kitty`) and this module only decides which one to
//! ask for; see `docs/video.md` for why pulling frames out of mpv over
//! json ipc is not an option.

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
        VideoOutput::Off => None,
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
pub fn wanted(screen: Screen, want: VideoOutput, probed: Option<ProtocolType>) -> Option<Vo> {
    if !screen.fullscreen || screen.minimized || screen.overlay || !screen.is_video {
        return None;
    }
    drawing_vo(want, probed)
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
}
