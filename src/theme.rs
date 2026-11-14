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

/// `level` in `0.0..=1.0`, where 0 is black and 1 is white.
pub fn shade(level: f32) -> Color {
    let v = (level.clamp(0.0, 1.0) * 255.0).round() as u8;
    Color::Rgb(v, v, v)
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
