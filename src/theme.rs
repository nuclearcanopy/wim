//! Strictly monochrome palette: every color used anywhere in the UI is a
//! shade of gray between `#000000` and `#ffffff`. No hues, ever; state
//! (errors, selection, emphasis) is conveyed via brightness, weight, and
//! modifiers instead of color.

use ratatui::style::Color;

/// `level` in `0.0..=1.0`, where 0 is black and 1 is white.
pub fn shade(level: f32) -> Color {
    let v = (level.clamp(0.0, 1.0) * 255.0).round() as u8;
    Color::Rgb(v, v, v)
}

/// Linearly interpolates a shade between `from` and `to` at position `t`
/// (`0.0..=1.0`), for gradient text/banners.
pub fn lerp_shade(from: f32, to: f32, t: f32) -> Color {
    shade(from + (to - from) * t.clamp(0.0, 1.0))
}

pub const BLACK: f32 = 0.0;
pub const NEAR_BLACK: f32 = 0.08;
pub const DIM: f32 = 0.35;
pub const MID: f32 = 0.55;
pub const BRIGHT: f32 = 0.78;
pub const NEAR_WHITE: f32 = 0.92;
pub const WHITE: f32 = 1.0;
