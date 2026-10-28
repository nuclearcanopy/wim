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
