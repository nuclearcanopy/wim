//! Test-only helpers for rendering widgets into a bare `Buffer`. Every
//! widget here is a plain `Widget::render(self, area, buf)`, so none of
//! these tests need a terminal or a `TestBackend`: draw into a buffer
//! the size of the layout under test and read the characters back.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;

pub fn buffer(width: u16, height: u16) -> Buffer {
    let mut buf = Buffer::empty(Rect::new(0, 0, width, height));
    // Buffer::empty fills with ' '; '.' makes cells nothing drew into
    // readable in snapshot output instead of invisible.
    for cell in buf.content.iter_mut() {
        cell.set_char('.');
    }
    buf
}

/// The buffer's characters as one row-per-line string, for snapshot
/// asserts where a wrong glyph has to be *visible* in the failure
/// output rather than hidden behind a coordinate.
pub fn grid(buf: &Buffer) -> String {
    let area = buf.area;
    (area.y..area.y + area.height)
        .map(|y| {
            (area.x..area.x + area.width)
                .map(|x| char_at(buf, x, y))
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

pub fn char_at(buf: &Buffer, x: u16, y: u16) -> char {
    buf.cell((x, y))
        .and_then(|c| c.symbol().chars().next())
        .unwrap_or(' ')
}

/// The sentinel background `plated_buffer` lays down: a 256-colour
/// index no `shade()` level produces, so finding it after a render
/// means it came from here.
pub const PLATE: ratatui::style::Color = ratatui::style::Color::Indexed(99);
/// The sentinel char, for the same reason. Not `buffer`'s '.', which
/// a widget can legitimately draw ("-12..12" in the eq menu).
pub const UNDERNEATH: char = '¤';

/// A buffer standing in for what an overlay gets drawn on top of:
/// every cell carries a sentinel char *and* a sentinel background, the
/// latter for the selection plate `results_list::fill_row_bg` leaves
/// behind. An overlay has to reset the background too, not just
/// overwrite the char.
pub fn plated_buffer(width: u16, height: u16) -> Buffer {
    let mut buf = Buffer::empty(Rect::new(0, 0, width, height));
    for cell in buf.content.iter_mut() {
        cell.set_char(UNDERNEATH);
        cell.set_bg(PLATE);
    }
    buf
}

/// Not one cell inside `rect` still shows what was underneath. This is
/// the regression for the "settings menu has no border and its fucky"
/// report: the full-screen menus drew their rows and nothing else, so
/// every cell between them kept the sidebar/track list/banner that was
/// already there.
pub fn assert_no_bleed_through(buf: &Buffer, rect: Rect) {
    for y in rect.y..rect.y + rect.height {
        for x in rect.x..rect.x + rect.width {
            assert_ne!(
                char_at(buf, x, y),
                UNDERNEATH,
                "cell ({x}, {y}) still shows the char underneath:\n{}",
                grid(buf)
            );
            assert_ne!(
                buf.cell((x, y)).unwrap().bg,
                PLATE,
                "cell ({x}, {y}) still shows the plate underneath:\n{}",
                grid(buf)
            );
        }
    }
}

/// Every cell outside `rect` still holds the fill char `buffer()` laid
/// down, i.e. the widget stayed inside the rect it was given. The
/// usual failure is a writer with no bounds guard, or one advancing a
/// cell per char through wide glyphs and running past the right edge.
pub fn assert_outside_untouched(buf: &Buffer, rect: Rect) {
    for y in buf.area.y..buf.area.y + buf.area.height {
        for x in buf.area.x..buf.area.x + buf.area.width {
            let inside = (rect.x..rect.x + rect.width).contains(&x)
                && (rect.y..rect.y + rect.height).contains(&y);
            if !inside {
                assert_eq!(
                    char_at(buf, x, y),
                    '.',
                    "wrote outside its rect at ({x}, {y}):\n{}",
                    grid(buf)
                );
            }
        }
    }
}
