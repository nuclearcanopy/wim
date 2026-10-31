//! What this terminal can actually draw, and which album-art mode
//! follows from that.
//!
//! Pulled out of main.rs (see ROADMAP) as one seam: every decision in
//! here is "ask the terminal, or decide from what it answered". None
//! of it touches app state, panes or the input tiers, and the probe's
//! own tests came with it.

use std::sync::mpsc::{self, Receiver, TryRecvError};

use ratatui_image::picker::{Picker, ProtocolType};

use crate::{browse, config, trace};

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

/// Said once per session by `ArtProbe::cycle`, not once per keypress:
/// the full modes are not reachable here at all, so cycling past them
/// needs a reason.
const NO_GRAPHICS_ALERT: &str = "album art: this terminal reports no graphics protocol (sixel/kitty/iterm2), so the full-* modes are unavailable here";

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

/// One `ratatui-image` protocol instance plus the key naming what it
/// holds, so the two cannot drift apart.
///
/// A `struct Ui` slice (see ROADMAP), and it removes a duplication
/// rather than only grouping: the full player and the browse view
/// follow the same rule -- rebuild only when the art or the tone
/// changes, since `ratatui-image` owns the per-instance resize and
/// encode caching -- and each had its own copy. The browse view's copy
/// kept its key in a *separate* local from the protocol, which is
/// desync by construction: two fields updated at two sites, and
/// nothing making them agree.
#[derive(Default)]
pub struct ProtocolCache {
    held: Option<(String, ratatui_image::protocol::StatefulProtocol)>,
}

impl ProtocolCache {
    /// Rebuilds for `key` if that is not what is already held.
    ///
    /// `image` is a closure because building it is the expensive part
    /// (a greyscale copy for `full-bw`) and a hit must not pay it.
    pub fn ensure(
        &mut self,
        picker: Option<&mut Picker>,
        key: String,
        image: impl FnOnce() -> image::DynamicImage,
    ) {
        if self.held.as_ref().is_some_and(|(have, _)| *have == key) {
            return;
        }
        self.held = picker.map(|p| (key, p.new_resize_protocol(image())));
    }

    pub fn clear(&mut self) {
        self.held = None;
    }

    pub fn protocol(&mut self) -> Option<&mut ratatui_image::protocol::StatefulProtocol> {
        self.held.as_mut().map(|(_, p)| p)
    }

    /// What it currently holds, for a caller that needs to decide
    /// whether to clear.
    pub fn key(&self) -> Option<&str> {
        self.held.as_ref().map(|(k, _)| k.as_str())
    }
}

#[cfg(test)]
mod protocol_cache_tests {
    use super::*;

    /// With no picker there is no protocol, and asking must not latch
    /// a key the cache cannot actually honour -- otherwise the first
    /// frame after a probe lands would think it was already current.
    #[test]
    fn no_picker_holds_nothing() {
        let mut c = ProtocolCache::default();
        c.ensure(None, "a".into(), || image::DynamicImage::new_rgb8(1, 1));
        assert!(c.key().is_none());
        assert!(c.protocol().is_none());
    }

    /// The expensive half is the image, so a hit must not build it.
    /// This is what the closure is for.
    #[test]
    fn a_hit_does_not_build_the_image() {
        let mut picker = Picker::from_fontsize((10, 20));
        let mut c = ProtocolCache::default();
        c.ensure(Some(&mut picker), "a".into(), || {
            image::DynamicImage::new_rgb8(2, 2)
        });
        assert_eq!(c.key(), Some("a"));

        let mut built = false;
        c.ensure(Some(&mut picker), "a".into(), || {
            built = true;
            image::DynamicImage::new_rgb8(2, 2)
        });
        assert!(!built, "same key, so nothing should be rebuilt");

        c.ensure(Some(&mut picker), "b".into(), || {
            built = true;
            image::DynamicImage::new_rgb8(2, 2)
        });
        assert!(built, "a new key has to rebuild");
        assert_eq!(c.key(), Some("b"));
    }

    #[test]
    fn clearing_forgets_the_key_too() {
        let mut picker = Picker::from_fontsize((10, 20));
        let mut c = ProtocolCache::default();
        c.ensure(Some(&mut picker), "a".into(), || {
            image::DynamicImage::new_rgb8(1, 1)
        });
        c.clear();
        assert!(c.key().is_none(), "a stale key would suppress the rebuild");
    }
}

/// The graphics probe in flight, its answer, and the album-art mode
/// that follows from it.
///
/// A `struct Ui` slice (see ROADMAP), and the group shares one rule:
/// the terminal's answer is the only authority over which modes are
/// live, so each of the four locals it replaces (`picker`,
/// `picker_probed`, `art_mode`/`art_mode_pending`,
/// `warned_no_graphics`) only means anything next to the others. The
/// rule is three methods now: the answer is taken exactly once, it may
/// only move a mode nobody has cycled by hand since startup, and
/// cycling is judged against the protocol rather than against
/// `picker.is_some()`.
pub struct ArtProbe {
    /// What config.toml asked for, held once so the sites that used to
    /// pass `cfg.album_art` by hand cannot disagree about it.
    asked: config::AlbumArtMode,
    mode: browse::ArtMode,
    /// The mode the config implied with the probe still out; `mode`
    /// still equal to it means nobody has cycled since.
    pending: browse::ArtMode,
    picker: Option<Picker>,
    /// Dropped the moment the answer lands, so applying it twice is
    /// not representable rather than guarded by a separate flag.
    answer: Option<Receiver<Option<Picker>>>,
    warned: bool,
}

impl ArtProbe {
    /// Asks the terminal on its own thread: it may simply never
    /// answer, and `ratatui-image` then waits out a full second, which
    /// was the single largest thing the first frame sat behind. Call
    /// once raw mode is on, since the query saves and restores
    /// whatever termios state it finds.
    pub fn spawn(asked: config::AlbumArtMode) -> Self {
        let (tx, rx) = mpsc::channel();
        if graphics_plausible() {
            std::thread::spawn(move || {
                let _ = tx.send(Picker::from_query_stdio().ok());
                trace::mark("graphics probe answered");
            });
        } else {
            // skipped, not backgrounded: the probe reads stdin waiting
            // for an answer, so on a terminal that will never send one
            // it sits there for a second racing the event loop for
            // input and eating whatever arrives, mouse reports
            // included. not asking at all is both faster and the only
            // way the event loop is the single reader of stdin.
            let _ = tx.send(None);
            trace::mark("graphics probe skipped (terminal has no protocol)");
        }
        Self::waiting(asked, rx)
    }

    /// Pre-answer state: the config's mode as if no protocol existed,
    /// since a full mode with nothing to draw it is a blank pane and
    /// the first frames happen before the terminal has replied.
    fn waiting(asked: config::AlbumArtMode, answer: Receiver<Option<Picker>>) -> Self {
        let pending = art_mode_for_picker(asked, false);
        Self {
            asked,
            mode: pending,
            pending,
            picker: None,
            answer: Some(answer),
            warned: false,
        }
    }

    /// The mode to draw with this frame.
    pub fn mode(&self) -> browse::ArtMode {
        self.mode
    }

    /// For `ProtocolCache::ensure`, which is where the protocol
    /// instances live; this only owns the picker that builds them.
    pub fn picker_mut(&mut self) -> Option<&mut Picker> {
        self.picker.as_mut()
    }

    /// What the terminal answered, for the video output's own decision.
    pub fn protocol_type(&self) -> Option<ProtocolType> {
        self.picker.map(|p| p.protocol_type())
    }

    /// The capability test, asked of the protocol and never of the
    /// picker; see `has_real_protocol`.
    pub fn real_graphics(&self) -> bool {
        has_real_protocol(self.protocol_type())
    }

    /// Takes the terminal's answer if it has arrived, and only ever
    /// once. Returns the alert the host owes the user.
    ///
    /// A `full-*` config on a terminal with no protocol is told so out
    /// loud rather than left with an art pane that never fills; `auto`
    /// asked for whatever was available and got it. A dropped channel
    /// is an answer too (no protocol), where the loose version marked
    /// the probe done and skipped the mode decision and the alert with
    /// it.
    pub fn poll(&mut self) -> Option<String> {
        let answered = match self.answer.as_ref()?.try_recv() {
            Ok(answer) => answer,
            Err(TryRecvError::Empty) => return None,
            Err(TryRecvError::Disconnected) => None,
        };
        self.answer = None;
        self.picker = answered;
        // only a mode the user has not touched: the probe can take a
        // second to land, which is long enough to cycle 'i' first, and
        // upgrading then would undo a deliberate keypress.
        if self.mode == self.pending {
            self.mode = art_mode_for_picker(self.asked, self.real_graphics());
        }
        (!self.real_graphics() && wants_full_art(self.asked)).then(|| {
            format!(
                "album art: terminal reported no graphics protocol, using {}",
                art_mode_for_picker(self.asked, false).label()
            )
        })
    }

    /// One step of the cycle `i` walks (and the settings menu's art
    /// row), stepping past the full modes this terminal cannot draw.
    /// Returns the alert for that, at most once per session.
    pub fn cycle(&mut self) -> Option<&'static str> {
        let (next, skipped) = self.mode.cycle(self.real_graphics());
        self.mode = next;
        if !skipped || self.warned {
            return None;
        }
        self.warned = true;
        Some(NO_GRAPHICS_ALERT)
    }
}

#[cfg(test)]
mod art_probe_tests {
    use super::*;

    /// A picker carrying a real protocol, the way a kitty-class
    /// terminal's answer arrives.
    fn capable() -> Picker {
        let mut p = Picker::from_fontsize((10, 20));
        p.set_protocol_type(ProtocolType::Kitty);
        p
    }

    /// What `from_query_stdio` hands back when nothing answered at
    /// all: a picker, and not a capability.
    fn silent() -> Picker {
        let mut p = Picker::from_fontsize((10, 20));
        p.set_protocol_type(ProtocolType::Halfblocks);
        p
    }

    fn waiting(asked: config::AlbumArtMode) -> (ArtProbe, mpsc::Sender<Option<Picker>>) {
        let (tx, rx) = mpsc::channel();
        (ArtProbe::waiting(asked, rx), tx)
    }

    /// The probe takes about a second on a terminal that answers
    /// slowly, which is long enough to press 'i' first. An upgrade
    /// then would silently undo that keypress.
    #[test]
    fn the_answer_only_moves_a_mode_nobody_has_cycled() {
        let (mut art, tx) = waiting(config::AlbumArtMode::Auto);
        assert_eq!(art.mode(), browse::ArtMode::AsciiColor);
        // 'i' pressed before the answer lands, which on a terminal
        // that answers slowly is a whole second of real time.
        art.cycle();
        assert_eq!(art.mode(), browse::ArtMode::Off);

        tx.send(Some(capable())).unwrap();
        assert_eq!(art.poll(), None);
        assert!(art.real_graphics());
        assert_eq!(
            art.mode(),
            browse::ArtMode::Off,
            "the probe landed on a mode the user chose by hand"
        );

        // and the same answer on an untouched mode does upgrade it,
        // which is the whole reason the probe is waited for.
        let (mut fresh, tx) = waiting(config::AlbumArtMode::Auto);
        tx.send(Some(capable())).unwrap();
        assert_eq!(fresh.poll(), None);
        assert_eq!(fresh.mode(), browse::ArtMode::FullColor);
    }

    /// Two answers must not both be applied: the second would find
    /// `mode` back at `pending` (the first one degraded it to exactly
    /// that) and read as a free upgrade, resurrecting a full mode the
    /// terminal had already said it cannot draw.
    #[test]
    fn the_answer_is_taken_exactly_once() {
        let (mut art, tx) = waiting(config::AlbumArtMode::FullColor);
        tx.send(None).unwrap();
        tx.send(Some(capable())).unwrap();

        assert!(art.poll().is_some(), "a full-* config is owed the alert");
        assert_eq!(art.mode(), browse::ArtMode::AsciiColor);

        assert_eq!(art.poll(), None);
        assert!(!art.real_graphics(), "a second answer was applied");
        assert_eq!(art.mode(), browse::ArtMode::AsciiColor);
    }

    /// Nothing on the channel yet is not an answer: the mode stays put
    /// and the next frame asks again.
    #[test]
    fn an_empty_channel_leaves_the_probe_in_flight() {
        let (mut art, tx) = waiting(config::AlbumArtMode::FullColor);
        assert_eq!(art.poll(), None);
        assert_eq!(art.mode(), browse::ArtMode::AsciiColor);

        tx.send(Some(capable())).unwrap();
        assert_eq!(art.poll(), None);
        assert_eq!(
            art.mode(),
            browse::ArtMode::FullColor,
            "the answer still has to land once it arrives"
        );
    }

    /// A sender dropped without sending (the probe thread panicking in
    /// `from_query_stdio`) is an answer of "no protocol", alert
    /// included. The loose version marked the probe done on
    /// `Disconnected` and skipped the decision, so a `full-*` config
    /// silently got ascii with no explanation.
    #[test]
    fn a_dropped_probe_answers_no_protocol_out_loud() {
        let (mut art, tx) = waiting(config::AlbumArtMode::FullBw);
        drop(tx);
        let alert = art.poll().expect("a full-* config is owed the alert");
        assert!(alert.contains("ascii-bw"), "{alert}");
        assert_eq!(art.mode(), browse::ArtMode::AsciiBw);
        assert_eq!(art.poll(), None, "and it is still answered only once");
    }

    /// `auto` is a detect, so landing on ascii is what it asked for;
    /// an alert there would be noise on every terminal without sixel.
    #[test]
    fn auto_is_not_owed_an_explanation() {
        let (mut art, tx) = waiting(config::AlbumArtMode::Auto);
        tx.send(Some(silent())).unwrap();
        assert_eq!(art.poll(), None);
        assert_eq!(art.mode(), browse::ArtMode::AsciiColor);
    }

    /// Halfblocks is ratatui-image's unicode fallback, so a picker
    /// holding it must not make the full modes reachable; cycling has
    /// to walk straight past them.
    #[test]
    fn a_picker_without_a_protocol_is_not_a_capability() {
        let (mut art, tx) = waiting(config::AlbumArtMode::AsciiBw);
        tx.send(Some(silent())).unwrap();
        assert_eq!(art.poll(), None);
        assert!(!art.real_graphics());
        for _ in 0..8 {
            art.cycle();
            assert!(!art.mode().is_full(), "cycled into {:?}", art.mode());
        }
    }

    /// One alert per session, not one per keypress: 'i' can be held
    /// down, and the full modes are unreachable for the whole run.
    #[test]
    fn the_unreachable_modes_are_announced_once_per_session() {
        let (mut art, _tx) = waiting(config::AlbumArtMode::AsciiBw);
        assert_eq!(art.mode(), browse::ArtMode::AsciiBw);
        assert_eq!(art.cycle(), None, "ascii-bw -> ascii-color skips nothing");
        assert!(
            art.cycle().is_some(),
            "ascii-color steps past full-bw/full-color to off, which is worth saying"
        );
        for _ in 0..12 {
            assert_eq!(art.cycle(), None, "said twice");
        }
    }
}
