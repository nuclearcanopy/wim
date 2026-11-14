//! Strictly monochrome palette: every color used anywhere in the UI is a
//! shade of gray between `#000000` and `#ffffff`. No hues, ever; state
//! (errors, selection, emphasis) is conveyed via brightness, weight, and
//! modifiers instead of color.

use ratatui::style::Color;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColorMode {
    Mono,
    Semantic,
}

/// `WIM_COLOR=semantic` enables state hues; `mono` (or `NO_COLOR`) keeps the
/// original brightness-only palette. Shapes, labels, and glyphs remain the
/// primary state cues in either mode.
pub fn color_mode() -> ColorMode {
    if std::env::var_os("NO_COLOR").is_some()
        || std::env::var("WIM_COLOR").is_ok_and(|value| value.eq_ignore_ascii_case("mono"))
    {
        ColorMode::Mono
    } else if std::env::var("WIM_COLOR").is_ok_and(|value| value.eq_ignore_ascii_case("semantic")) {
        ColorMode::Semantic
    } else {
        ColorMode::Mono
    }
}

pub const ACCENT: Color = Color::Rgb(100, 190, 255);
pub const PLAYED: Color = Color::Rgb(100, 210, 170);
pub const BUFFERED: Color = Color::Rgb(105, 165, 230);
pub const UNPLAYED: Color = Color::Rgb(90, 100, 120);
pub const SUCCESS: Color = Color::Rgb(105, 210, 135);
pub const WARNING: Color = Color::Rgb(245, 190, 90);
pub const ERROR: Color = Color::Rgb(245, 105, 105);

/// How many colours the terminal can show, and whether its background is
/// light. Greys survive a 256-colour ramp almost unchanged, so the only
/// loss on a lesser terminal is the level steps; the Linux console and
/// `dumb` get four named greys so nothing renders as a wrong hue.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Palette {
    pub depth: Depth,
    pub light_background: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Depth {
    TrueColor,
    Ansi256,
    Ansi16,
}

pub fn detect_palette(env: impl Fn(&str) -> Option<String>) -> Palette {
    let term = env("TERM").unwrap_or_default();
    let colorterm = env("COLORTERM").unwrap_or_default().to_ascii_lowercase();
    let depth = if env("WIM_TRUECOLOR").is_some_and(|v| v != "0")
        || colorterm.contains("truecolor")
        || colorterm.contains("24bit")
    {
        Depth::TrueColor
    } else if matches!(
        term.as_str(),
        "linux" | "dumb" | "ansi" | "vt52" | "vt100" | "vt102" | "vt220" | "vt320"
    ) {
        Depth::Ansi16
    } else if term.contains("256color") {
        Depth::Ansi256
    } else {
        // unknown terminals and the modern ones that skip COLORTERM
        // (kitty, foot, wezterm, alacritty set it; xterm-direct does not)
        Depth::TrueColor
    };
    // `COLORFGBG` is "fg;bg" with ANSI indices; 7 and 15 are the light ones.
    let light_background = match env("WIM_BACKGROUND").as_deref() {
        Some("light") => true,
        Some(_) => false,
        None => env("COLORFGBG")
            .and_then(|v| v.rsplit(';').next().map(str::to_owned))
            .is_some_and(|bg| bg == "7" || bg == "15"),
    };
    Palette {
        depth,
        light_background,
    }
}

fn palette() -> Palette {
    // tests compare exact colours; the machine running them must not matter
    #[cfg(test)]
    return Palette {
        depth: Depth::TrueColor,
        light_background: false,
    };
    #[cfg(not(test))]
    static PALETTE: std::sync::OnceLock<Palette> = std::sync::OnceLock::new();
    #[cfg(not(test))]
    *PALETTE.get_or_init(|| detect_palette(|name| std::env::var(name).ok()))
}

/// The colour for a grey `level` on a given palette.
pub fn shade_for(level: f32, palette: Palette) -> Color {
    let level = level.clamp(0.0, 1.0);
    let level = if palette.light_background {
        1.0 - level
    } else {
        level
    };
    match palette.depth {
        Depth::TrueColor => {
            let v = (level * 255.0).round() as u8;
            Color::Rgb(v, v, v)
        }
        // black, the 24-step grey ramp (232..=255), white
        Depth::Ansi256 => match (level * 25.0).round() as u8 {
            0 => Color::Indexed(16),
            25 => Color::Indexed(231),
            step => Color::Indexed(231 + step),
        },
        Depth::Ansi16 => match (level * 4.0).round() as u8 {
            0 => Color::Black,
            1 => Color::DarkGray,
            2 | 3 => Color::Gray,
            _ => Color::White,
        },
    }
}

/// `level` in `0.0..=1.0`, where 0 is black and 1 is white (swapped on a
/// light background).
pub fn shade(level: f32) -> Color {
    shade_for(level, palette())
}

/// Linearly interpolates a level between `from` and `to` at position
/// `t` (`0.0..=1.0`), for gradient text/banners; stays a plain level
/// rather than converting straight to a `Color` since callers (e.g.
/// the banner's sweep) usually need to combine it with another factor
/// (a pulse, a fade) before the one conversion to `Color` at the end.
pub fn lerp(from: f32, to: f32, t: f32) -> f32 {
    from + (to - from) * t.clamp(0.0, 1.0)
}

pub const NEAR_BLACK: f32 = 0.08;
pub const DIM: f32 = 0.35;
pub const MID: f32 = 0.55;
pub const BRIGHT: f32 = 0.78;
pub const NEAR_WHITE: f32 = 0.92;
pub const WHITE: f32 = 1.0;

#[cfg(test)]
mod palette_tests {
    use super::*;

    fn env<'a>(pairs: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<String> + 'a {
        move |name| {
            pairs
                .iter()
                .find(|(k, _)| *k == name)
                .map(|(_, v)| (*v).to_owned())
        }
    }

    #[test]
    fn colorterm_wins_and_unknown_terminals_get_truecolor() {
        let p = detect_palette(env(&[
            ("TERM", "xterm-256color"),
            ("COLORTERM", "truecolor"),
        ]));
        assert_eq!(p.depth, Depth::TrueColor);
        assert_eq!(detect_palette(env(&[])).depth, Depth::TrueColor);
        assert_eq!(
            detect_palette(env(&[("TERM", "screen-256color")])).depth,
            Depth::Ansi256
        );
        assert_eq!(
            detect_palette(env(&[("TERM", "linux")])).depth,
            Depth::Ansi16
        );
        // vte-based terminals are not vt100s
        assert_eq!(
            detect_palette(env(&[("TERM", "vte-256color")])).depth,
            Depth::Ansi256
        );
    }

    #[test]
    fn light_backgrounds_are_detected_and_invert_levels() {
        let light = detect_palette(env(&[("COLORFGBG", "0;15")]));
        assert!(light.light_background);
        assert_eq!(shade_for(0.0, light), Color::Rgb(255, 255, 255));
        assert!(!detect_palette(env(&[("COLORFGBG", "15;0")])).light_background);
        assert!(
            !detect_palette(env(&[("WIM_BACKGROUND", "dark"), ("COLORFGBG", "0;15")]))
                .light_background
        );
    }

    #[test]
    fn the_256_ramp_is_monotonic_and_stays_grey() {
        let p = Palette {
            depth: Depth::Ansi256,
            light_background: false,
        };
        let mut last = 0u8;
        for i in 0..=100 {
            match shade_for(i as f32 / 100.0, p) {
                Color::Indexed(n) => {
                    let rank = match n {
                        16 => 0,
                        231 => 255,
                        n => n,
                    };
                    assert!((232..=255).contains(&n) || n == 16 || n == 231, "{n}");
                    assert!(rank >= last || n == 231, "ramp went backwards at {i}");
                    last = rank;
                }
                other => panic!("{other:?}"),
            }
        }
    }
}
