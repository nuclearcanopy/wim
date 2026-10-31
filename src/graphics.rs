//! What this terminal can actually draw, and which album-art mode
//! follows from that.
//!
//! Pulled out of main.rs (see ROADMAP) as one seam: every decision in
//! here is "ask the terminal, or decide from what it answered". None
//! of it touches app state, panes or the input tiers, and the probe's
//! own tests came with it.

use ratatui_image::picker::{Picker, ProtocolType};

use crate::notify::{Notifications, Severity};
use crate::{browse, config};

/// Whether the config asks for a real graphics protocol outright, so
/// the host owes an explanation when the terminal has none; `auto`
/// isn't one of those, it is "whatever is available".
///
/// `fetch_cover_art` used to live here; it moved to `browse.rs` with
/// the async loader, which is the only thing that fetches art now.
pub fn wants_full_art(mode: config::AlbumArtMode) -> bool {
    matches!(
        mode,
        config::AlbumArtMode::FullBw | config::AlbumArtMode::FullColor
    )
}

/// Whether the terminal has an actual inline graphics protocol.
///
/// `Picker::from_query_stdio` hands back a `Picker` even when the
/// terminal answered nothing at all, carrying
/// `ProtocolType::Halfblocks`: ratatui-image's own unicode fallback,
/// coloured half-block glyphs and not real art. So `picker.is_some()`
/// is true on every terminal and was never a capability test; the
/// protocol it holds is. This is what made alacritty render "the old
/// ascii but with colour" while claiming to be in real-art mode.
pub fn has_real_protocol(protocol: Option<ProtocolType>) -> bool {
    matches!(
        protocol,
        Some(ProtocolType::Sixel | ProtocolType::Kitty | ProtocolType::Iterm2)
    )
}

/// `has_real_protocol` over whatever the probe handed back.
pub fn real_graphics(picker: Option<Picker>) -> bool {
    has_real_protocol(picker.map(|p| p.protocol_type()))
}

/// Which art mode to start in, once the graphics probe has answered.
///
/// `auto` means *detect*: the best available, i.e. `full-color` on a
/// terminal with a real protocol and `ascii-color` on one without.
/// The `ascii-*` modes never depend on the terminal. A `full-*` mode
/// waits for the probe and then degrades to its own ascii half if no
/// protocol turned up, since a full mode with nothing to draw it is a
/// blank pane rather than a fallback.
pub fn art_mode_for_picker(mode: config::AlbumArtMode, protocol_detected: bool) -> browse::ArtMode {
    let asked = match mode {
        config::AlbumArtMode::Off => browse::ArtMode::Off,
        config::AlbumArtMode::AsciiBw => browse::ArtMode::AsciiBw,
        config::AlbumArtMode::AsciiColor => browse::ArtMode::AsciiColor,
        config::AlbumArtMode::FullBw => browse::ArtMode::FullBw,
        config::AlbumArtMode::FullColor | config::AlbumArtMode::Auto => browse::ArtMode::FullColor,
    };
    if protocol_detected {
        asked
    } else {
        asked.without_graphics()
    }
}

/// Said once per session, not once per keypress: the full modes are
/// not reachable here at all, so cycling past them needs a reason.
pub fn warn_no_graphics(notifications: &mut Notifications, warned: &mut bool) {
    if *warned {
        return;
    }
    *warned = true;
    notifications.push(
        Severity::Alert,
        "album art: this terminal reports no graphics protocol (sixel/kitty/iterm2), so the full-* modes are unavailable here",
    );
}

/// The image handed to a graphics protocol: greyscaled for `full-bw`,
/// the source untouched otherwise. Converted here rather than at paint
/// time so the protocol's own resize/encode cache holds the already
/// greyscaled picture.
pub fn protocol_image(img: &image::DynamicImage, mode: browse::ArtMode) -> image::DynamicImage {
    if mode.is_bw() {
        image::DynamicImage::ImageRgb8(img.grayscale().into_rgb8())
    } else {
        img.clone()
    }
}

/// Real cell height:width ratio from the terminal's own reported
/// pixel size, rather than assuming a fixed 2:1; most terminals that
/// matter here (kitty, alacritty, foot, wezterm, ...) answer this via
/// the same ioctl crossterm uses, and a fixed guess is exactly what
/// made album art look slightly too tall at some sizes but not
/// others, depending how close the real font metrics happened to
/// land to "2". Falls back to 2.0 when a terminal reports zero pixel
/// dims (no pixel-size support at all).
pub fn cell_aspect() -> f32 {
    match crossterm::terminal::window_size() {
        Ok(ws) if ws.width > 0 && ws.height > 0 && ws.columns > 0 && ws.rows > 0 => {
            let cell_w = ws.width as f32 / ws.columns as f32;
            let cell_h = ws.height as f32 / ws.rows as f32;
            (cell_h / cell_w).clamp(1.0, 4.0)
        }
        _ => 2.0,
    }
}

/// Whether this terminal could plausibly answer a graphics-capability
/// query, so the probe is worth the stdin race it costs.
///
/// Deliberately a denylist of terminals known to implement no inline
/// graphics protocol at all, rather than an allowlist: a new or
/// unrecognised terminal should still get asked, since the probe is
/// the only authority. Alacritty is the one that matters here; it has
/// never shipped sixel or kitty graphics, so the query is guaranteed
/// to time out and steal a second of input on the way.
pub fn graphics_plausible() -> bool {
    let term = std::env::var("TERM").unwrap_or_default();
    let program = std::env::var("TERM_PROGRAM").unwrap_or_default();
    graphics_plausible_in(&term, &program)
}

/// The decision itself, split from reading the environment so it is
/// testable without mutating process-wide state.
pub fn graphics_plausible_in(term: &str, program: &str) -> bool {
    let known_blind = |s: &str| s.to_lowercase().contains("alacritty");
    !(known_blind(term) || known_blind(program))
}

#[cfg(test)]
mod graphics_probe_tests {
    use super::*;

    /// Alacritty never answers a graphics query, so probing it costs a
    /// second of stolen stdin for a guaranteed "no". The probe reads
    /// stdin on its own thread, which races the event loop, so this is
    /// an input-correctness guard as much as a speed one.
    #[test]
    fn a_terminal_with_no_graphics_protocol_is_not_probed() {
        assert!(!graphics_plausible_in("alacritty", ""));
        assert!(!graphics_plausible_in("xterm-256color", "Alacritty"));
    }

    /// Everything else is asked, including terminals this code has
    /// never heard of: the probe is the authority, and the denylist is
    /// only for cases where asking is known to be pointless.
    #[test]
    fn any_other_terminal_is_still_probed() {
        for (term, program) in [
            ("xterm-kitty", ""),
            ("xterm-256color", "ghostty"),
            ("foot", ""),
            ("something-nobody-has-shipped-yet", ""),
        ] {
            assert!(
                graphics_plausible_in(term, program),
                "{term}/{program} should still be probed"
            );
        }
    }
}

#[cfg(test)]
mod art_mode_tests {
    use super::*;

    /// The bug this whole mode system exists to fix.
    ///
    /// `Picker::from_query_stdio` falls back to
    /// `ProtocolType::Halfblocks` when the terminal answers nothing, so
    /// it hands back `Some(Picker)` on alacritty too. Treating that as
    /// "real art available" is what made `full-color` render coloured
    /// half-blocks at half the horizontal resolution of wim's own
    /// rasterizer and call it real art. Halfblocks is not a protocol.
    #[test]
    fn halfblocks_is_not_a_real_graphics_protocol() {
        assert!(
            !has_real_protocol(Some(ProtocolType::Halfblocks)),
            "halfblocks is ratatui-image's unicode fallback, not real art"
        );
        assert!(!has_real_protocol(None));
        for p in [
            ProtocolType::Sixel,
            ProtocolType::Kitty,
            ProtocolType::Iterm2,
        ] {
            assert!(has_real_protocol(Some(p)), "{p:?} is a real protocol");
        }
        // and the probe's own fallback picker, built the way
        // `from_query_stdio` builds it when nothing answers, must not
        // read as capable just because it exists.
        let mut silent = Picker::from_fontsize((10, 20));
        silent.set_protocol_type(ProtocolType::Halfblocks);
        assert!(
            !real_graphics(Some(silent)),
            "a picker is not a capability; the protocol in it is"
        );
    }

    /// A `full-*` mode on a terminal with no protocol degrades to the
    /// matching ascii mode rather than to a blank pane, and keeps the
    /// tone that was asked for.
    #[test]
    fn a_full_mode_waits_for_the_probe_and_then_degrades_in_kind() {
        assert_eq!(
            art_mode_for_picker(config::AlbumArtMode::FullColor, true),
            browse::ArtMode::FullColor
        );
        assert_eq!(
            art_mode_for_picker(config::AlbumArtMode::FullColor, false),
            browse::ArtMode::AsciiColor,
            "a full mode with no protocol renders nothing at all"
        );
        assert_eq!(
            art_mode_for_picker(config::AlbumArtMode::FullBw, true),
            browse::ArtMode::FullBw
        );
        assert_eq!(
            art_mode_for_picker(config::AlbumArtMode::FullBw, false),
            browse::ArtMode::AsciiBw,
            "asking for greyscale must not fall back into colour"
        );
    }

    /// `auto` is a detect: the best available, which is full colour on
    /// a terminal with a protocol and ascii colour on one without.
    #[test]
    fn auto_follows_the_probe() {
        assert_eq!(
            art_mode_for_picker(config::AlbumArtMode::Auto, true),
            browse::ArtMode::FullColor,
            "a terminal with a real protocol should get real art on auto"
        );
        assert_eq!(
            art_mode_for_picker(config::AlbumArtMode::Auto, false),
            browse::ArtMode::AsciiColor,
            "no protocol means the rasterizer, which still does colour"
        );
    }

    /// The `ascii-*` modes are an explicit ask for wim's own
    /// rasterizer, so a capable terminal must not override them.
    #[test]
    fn the_ascii_modes_stay_ascii_even_when_the_terminal_can_do_better() {
        for detected in [true, false] {
            assert_eq!(
                art_mode_for_picker(config::AlbumArtMode::AsciiBw, detected),
                browse::ArtMode::AsciiBw
            );
            assert_eq!(
                art_mode_for_picker(config::AlbumArtMode::AsciiColor, detected),
                browse::ArtMode::AsciiColor
            );
        }
    }

    #[test]
    fn off_stays_off_whatever_the_probe_says() {
        for detected in [true, false] {
            assert_eq!(
                art_mode_for_picker(config::AlbumArtMode::Off, detected),
                browse::ArtMode::Off
            );
        }
    }

    /// Only a `full-*` config is owed the "no graphics protocol here"
    /// alert; `auto` asked for whatever was available and got it.
    #[test]
    fn only_a_full_mode_counts_as_asking_for_a_graphics_protocol() {
        assert!(wants_full_art(config::AlbumArtMode::FullBw));
        assert!(wants_full_art(config::AlbumArtMode::FullColor));
        assert!(!wants_full_art(config::AlbumArtMode::Auto));
        assert!(!wants_full_art(config::AlbumArtMode::AsciiBw));
        assert!(!wants_full_art(config::AlbumArtMode::AsciiColor));
        assert!(!wants_full_art(config::AlbumArtMode::Off));
    }

    /// `full-bw` is greyscale because the image is converted before the
    /// protocol ever encodes it; `full-color` hands the source over
    /// untouched.
    #[test]
    fn full_bw_greyscales_the_image_before_the_protocol_sees_it() {
        let hued = image::DynamicImage::ImageRgb8(image::RgbImage::from_fn(4, 4, |x, y| {
            image::Rgb([(x * 60) as u8, (y * 50) as u8, 220])
        }));
        let grey = protocol_image(&hued, browse::ArtMode::FullBw).to_rgb8();
        for p in grey.pixels() {
            assert!(
                p[0] == p[1] && p[1] == p[2],
                "full-bw handed the protocol a hue: {p:?}"
            );
        }
        let kept = protocol_image(&hued, browse::ArtMode::FullColor).to_rgb8();
        assert_eq!(kept, hued.to_rgb8(), "full-color must not touch the source");
    }
}
