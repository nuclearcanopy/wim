use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Color;
use ratatui::widgets::Widget;

use crate::theme::{lerp, shade, DIM, NEAR_WHITE, WHITE};
use crate::ui::textfit::truncate_with_ellipsis;

/// How many of `lengths` (each segment's own character count, in
/// priority order) fit within `available_width` once a 2-char
/// separator between every pair is accounted for. Dropping whole
/// lowest-priority (rightmost) segments once the full line stops
/// fitting, rather than cutting the last one off mid-word ("cros"
/// from "crossfade: off", which is what a plain per-character
/// truncation at `available_width` used to produce).
fn segments_that_fit(lengths: &[usize], available_width: usize) -> usize {
    let mut used = 0usize;
    for (i, &len) in lengths.iter().enumerate() {
        let needed = len + if i > 0 { 2 } else { 0 };
        if used + needed > available_width {
            return i;
        }
        used += needed;
    }
    lengths.len()
}

/// Which state-line value last changed, for `ControlsBar::flash`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Field {
    Loop,
    Shuffle,
    Speed,
    Eq,
    Crossfade,
}

/// Same order the state line itself draws them in; `Focus::BottomBar`
/// navigation (main.rs) cycles through exactly this list.
pub const FIELDS: [Field; 5] = [
    Field::Loop,
    Field::Shuffle,
    Field::Speed,
    Field::Eq,
    Field::Crossfade,
];

impl Field {
    fn index(self) -> usize {
        FIELDS
            .iter()
            .position(|f| *f == self)
            .expect("every Field variant is listed in FIELDS")
    }

    pub fn next(self) -> Self {
        FIELDS[(self.index() + 1) % FIELDS.len()]
    }

    pub fn prev(self) -> Self {
        FIELDS[(self.index() + FIELDS.len() - 1) % FIELDS.len()]
    }
}

/// Bottom playback bar: as many lines as `area.height` allows, most
/// important first, rather than cramming everything onto one status
/// line. Resizable (bottom bar, vertical axis only); more rows reveal
/// more lines gracefully.
pub struct ControlsBar<'a> {
    pub status: &'a str,
    pub queue_len: usize,
    pub loop_label: &'a str,
    pub shuffle_on: bool,
    pub speed: f64,
    pub eq_preset: &'a str,
    pub crossfade_on: bool,
    /// Field that changed most recently and how "hot" it still is:
    /// `1.0` right after the toggle, decaying to `0.0` back at the
    /// line's normal DIM. Acknowledges a toggle by briefly
    /// brightening the value that already has a permanent spot on
    /// this line, instead of writing a one-off sentence to the status
    /// row above (which reads as the app typing a message at you for
    /// something the state line already shows).
    pub flash: Option<(Field, f32)>,
    /// `Focus::BottomBar`'s own selection cursor, if that pane
    /// currently has focus; renders the same brightened as a fully
    /// "hot" flash (see `draw_segments`), just steady instead of
    /// decaying, since a cursor position doesn't fade on its own.
    pub selected: Option<Field>,
    pub resize_hint: Option<&'a str>,
}

impl Widget for ControlsBar<'_> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        let width = area.width as usize;
        let mut y = area.y;

        if let Some(hint) = self.resize_hint {
            draw_line(buf, area, y, hint, width, shade(NEAR_WHITE));
            y += 1;
        }

        draw_line(buf, area, y, self.status, width, shade(NEAR_WHITE));
        y += 1;
        if y >= area.y + area.height {
            return;
        }

        let segments: [(Option<Field>, String); 6] = [
            (None, format!("queue: {}", self.queue_len)),
            (Some(Field::Loop), format!("loop: {}", self.loop_label)),
            (
                Some(Field::Shuffle),
                format!("shuffle: {}", if self.shuffle_on { "on" } else { "off" }),
            ),
            (Some(Field::Speed), format!("speed: {:.2}x", self.speed)),
            (Some(Field::Eq), format!("eq: {}", self.eq_preset)),
            (
                Some(Field::Crossfade),
                format!(
                    "crossfade: {}",
                    if self.crossfade_on { "on" } else { "off" }
                ),
            ),
        ];
        draw_segments(buf, area, y, width, &segments, self.flash, self.selected);
    }
}

fn draw_segments(
    buf: &mut Buffer,
    area: Rect,
    y: u16,
    width: usize,
    segments: &[(Option<Field>, String)],
    flash: Option<(Field, f32)>,
    selected: Option<Field>,
) {
    let limit = area.x + width.min(area.width as usize) as u16;
    let lengths: Vec<usize> = segments
        .iter()
        .map(|(_, text)| text.chars().count())
        .collect();
    let fits = segments_that_fit(&lengths, limit.saturating_sub(area.x) as usize);
    let mut x = area.x;
    for (i, (field, text)) in segments.iter().take(fits).enumerate() {
        if x >= limit {
            break;
        }
        if i > 0 {
            x = draw_chars(buf, x, limit, y, "  ", shade(DIM));
        }
        let flash_level = match (field, flash) {
            (Some(f), Some((hot_field, level))) if *f == hot_field => level,
            _ => 0.0,
        };
        // the selection cursor reads as a fully "hot" flash rather
        // than a third brightness tier of its own; unlike flash it
        // never decays, so it stays lit the whole time this field is
        // `Focus::BottomBar`'s current pick.
        let level = if *field == selected {
            flash_level.max(1.0)
        } else {
            flash_level
        };
        let color = if level > 0.0 {
            shade(lerp(DIM, WHITE, level))
        } else {
            shade(DIM)
        };
        x = draw_chars(buf, x, limit, y, text, color);
    }
}

fn draw_chars(buf: &mut Buffer, start_x: u16, limit: u16, y: u16, text: &str, color: Color) -> u16 {
    let mut x = start_x;
    for ch in text.chars() {
        if x >= limit {
            break;
        }
        if let Some(cell) = buf.cell_mut((x, y)) {
            cell.set_char(ch);
            cell.set_fg(color);
        }
        x += 1;
    }
    x
}

fn draw_line(
    buf: &mut Buffer,
    area: Rect,
    y: u16,
    text: &str,
    width: usize,
    color: ratatui::style::Color,
) {
    if y >= area.y + area.height {
        return;
    }
    let fitted = truncate_with_ellipsis(text, width);
    for (col, ch) in fitted.chars().enumerate() {
        let x = area.x + col as u16;
        if x >= area.x + area.width {
            break;
        }
        if let Some(cell) = buf.cell_mut((x, y)) {
            cell.set_char(ch);
            cell.set_fg(color);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_segment_fits_when_there_is_room() {
        let lengths = [5, 10, 3]; // + 2*2 separators = 22
        assert_eq!(segments_that_fit(&lengths, 22), 3);
        assert_eq!(segments_that_fit(&lengths, 100), 3);
    }

    #[test]
    fn drops_whole_trailing_segments_instead_of_cutting_one_mid_word() {
        // "queue: 0" (8) + "  " + "crossfade: off" (14) = 24; a width
        // of 20 can't fit the second segment at all, so it should be
        // dropped entirely rather than showing "crossfade: o".
        let lengths = [8, 14];
        assert_eq!(segments_that_fit(&lengths, 20), 1);
    }

    #[test]
    fn keeps_the_first_segment_even_if_nothing_else_fits() {
        let lengths = [8, 14];
        assert_eq!(segments_that_fit(&lengths, 10), 1);
    }

    #[test]
    fn returns_zero_when_not_even_the_first_segment_fits() {
        let lengths = [8, 14];
        assert_eq!(segments_that_fit(&lengths, 5), 0);
    }

    #[test]
    fn empty_segments_list_fits_trivially() {
        assert_eq!(segments_that_fit(&[], 0), 0);
    }
}
