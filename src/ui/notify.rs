use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::widgets::Widget;

use crate::notify::{Notification, Severity};
use crate::theme::{shade, BRIGHT, MID, WHITE};
use crate::ui::draw;
use crate::ui::textfit::{truncate_with_ellipsis, width as text_width};

/// Bordered boxes stacked in the top-right corner, newest on top,
/// rendered last so they sit over everything else (sidebar, main
/// content, bottom bar) without disturbing their layout; each only
/// touches the cells inside its own box.
pub struct NotificationOverlay<'a> {
    pub items: &'a [Notification],
}

/// One placement pass: the rect and the fitted text per toast that
/// fits, newest first, in the order they are painted. Both the render
/// and `boxes` walk this, so the cells a click is tested against are
/// the cells that were drawn.
fn placed(items: &[Notification], area: Rect) -> Vec<(Rect, String, &Notification)> {
    let mut placed = Vec::new();
    let mut y = area.y;
    for n in items.iter().rev() {
        if y + 3 > area.y + area.height {
            break;
        }

        let text = format!("[{}] {}", n.severity.icon(), n.message);
        let max_inner = (area.width as usize).saturating_sub(4);
        let text = truncate_with_ellipsis(&text, max_inner.max(1));
        // cells, not chars: a cjk or emoji track label in the
        // message measures wider than its char count, and sizing
        // the box by chars left the text overrunning its own right
        // border.
        let inner_width = (text_width(&text) as u16).max(4);
        let box_width = inner_width + 2;
        let start_x = area.x + area.width.saturating_sub(box_width);

        placed.push((Rect::new(start_x, y, box_width, 3), text, n));
        y += 4; // box height (3) + one row of gap before the next
    }
    placed
}

/// The cells the toasts own, for the frame's hit map. These are
/// painted over the panes after the draw pass recorded their lists, so
/// without this a click on a toast reaches whatever row is hidden
/// behind it (see `hit::HitMap::toasts`).
pub fn boxes(items: &[Notification], area: Rect) -> Vec<Rect> {
    placed(items, area)
        .into_iter()
        .map(|(rect, _, _)| rect)
        .collect()
}

impl Widget for NotificationOverlay<'_> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        for (rect, text, n) in placed(self.items, area) {
            // brightness carries severity, not color (monochrome
            // rule): error is the most prominent, info the least.
            let color = shade(match n.severity {
                Severity::Info => MID,
                Severity::Alert => BRIGHT,
                Severity::Error => WHITE,
            });
            draw_box(buf, rect.x, rect.y, rect.width, &text, &n.footer(), color);
        }
    }
}

fn draw_box(
    buf: &mut Buffer,
    x: u16,
    y: u16,
    width: u16,
    text: &str,
    footer: &str,
    color: ratatui::style::Color,
) {
    if width < 2 {
        return;
    }
    // own every cell of the box: these sit over the sidebar and the
    // main content, so an interior cell left unwritten shows whatever
    // was painted underneath (the box can be wider than its text, see
    // the `max(4)` on `inner_width`).
    draw::clear(buf, Rect::new(x, y, width, 3));

    let right = x + width - 1;
    let inner = (width - 2) as usize;

    put(buf, x, y, '┌', color);
    put(buf, right, y, '┐', color);
    for col in 1..width - 1 {
        put(buf, x + col, y, '─', color);
    }

    put(buf, x, y + 1, '│', color);
    put(buf, right, y + 1, '│', color);
    // the writer advances by display width and blanks the trailing
    // cell of a wide glyph; a per-char `x + 1 + i` put every glyph
    // after a wide one a cell left of where it belonged.
    draw::at(buf, x + 1, y + 1, text, color, inner);

    // the countdown (or, for a download in progress, a live percentage)
    // lives in the bottom-right corner of the border itself, not a
    // separate line, so the box stays exactly 3 rows regardless of how
    // much time is left / how far along the download is. drawn as a
    // full rule first and then overwritten, rather than interleaved
    // per cell, so a multi-cell glyph in the footer cannot desync the
    // dashes around it.
    put(buf, x, y + 2, '└', color);
    put(buf, right, y + 2, '┘', color);
    for col in 1..width - 1 {
        put(buf, x + col, y + 2, '─', color);
    }
    let footer = truncate_with_ellipsis(footer, inner);
    let footer_cells = text_width(&footer) as u16;
    if footer_cells > 0 {
        draw::at(
            buf,
            right.saturating_sub(footer_cells),
            y + 2,
            &footer,
            color,
            footer_cells as usize,
        );
    }
}

fn put(buf: &mut Buffer, x: u16, y: u16, ch: char, color: ratatui::style::Color) {
    crate::ui::draw::put(buf, x, y, ch, color);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::notify::{Notifications, Severity};
    use crate::ui::testbuf::{buffer, char_at, grid, plated_buffer};

    fn one(severity: Severity, message: &str) -> Notifications {
        let mut n = Notifications::default();
        n.push(severity, message);
        n
    }

    fn render(n: &Notifications, w: u16, h: u16) -> Buffer {
        let area = Rect::new(0, 0, w, h);
        let mut buf = buffer(w, h);
        NotificationOverlay { items: n.visible() }.render(area, &mut buf);
        buf
    }

    #[test]
    fn the_box_is_three_rows_with_a_closed_border() {
        let n = one(Severity::Info, "queued 3 tracks");
        let buf = render(&n, 40, 8);
        let g = grid(&buf);
        assert!(g.contains('┌') && g.contains('┐'), "{g}");
        assert!(g.contains('└') && g.contains('┘'), "{g}");
        assert!(g.contains("queued 3 tracks"), "{g}");
    }

    #[test]
    fn a_wide_character_message_is_not_silently_clipped() {
        // the box is sized in cells now. sizing it by char count made
        // it too narrow for a cjk title, and since the text writer
        // budgets correctly the symptom was not an overrun but a
        // silently truncated message: the box simply cut the title off
        // with plenty of terminal left.
        let n = one(Severity::Info, "queued: 日本語のタイトル");
        let buf = render(&n, 60, 8);
        let g = grid(&buf);
        // asserted on the last glyph rather than the whole string:
        // `grid` reads one char per cell and a wide glyph owns two, so
        // the rendered row carries a blank after each one.
        assert!(
            g.contains('ル'),
            "the title's last glyph was clipped by an undersized box:\n{g}"
        );
    }

    #[test]
    fn the_countdown_sits_in_the_bottom_border() {
        let n = one(Severity::Error, "error loading file");
        let buf = render(&n, 40, 8);
        let bottom = grid(&buf).lines().nth(2).unwrap().to_string();
        assert!(bottom.contains('s'), "a countdown belongs here: {bottom:?}");
        assert!(
            bottom.contains('└'),
            "and the border still closes: {bottom:?}"
        );
    }

    #[test]
    fn the_box_covers_everything_underneath_it() {
        // these sit over the sidebar and main content, so an interior
        // cell left unwritten shows whatever was painted there.
        let n = one(Severity::Info, "ok");
        let area = Rect::new(0, 0, 40, 8);
        let mut buf = plated_buffer(40, 8);
        NotificationOverlay { items: n.visible() }.render(area, &mut buf);
        // find the box's left edge on its middle row
        let left = (0..40)
            .find(|x| char_at(&buf, *x, 1) == '│')
            .expect("a left border");
        for x in left..40 {
            let c = char_at(&buf, x, 1);
            assert_ne!(c, '¤', "bleed-through at x={x} on the box's row");
        }
    }

    /// The rects the hit map is given have to be the cells the boxes
    /// were painted on, in both directions: every reported rect holds
    /// a box, and every box drawn is inside a reported rect. A click
    /// resolves against these, so a rect that drifts off the paint
    /// either swallows a row the toast is not covering or lets one
    /// through that it is.
    #[test]
    fn the_reported_boxes_are_the_cells_the_toasts_were_painted_on() {
        let mut n = Notifications::default();
        n.push(Severity::Info, "queued 3 tracks");
        n.push(Severity::Alert, "server went offline");
        n.push(Severity::Error, "error loading file");
        // a non-zero origin: the overlay is given the whole frame, but
        // nothing here may assume that frame starts at 0,0.
        let area = Rect::new(2, 1, 44, 14);
        let mut buf = buffer(area.x + area.width, area.y + area.height);
        NotificationOverlay { items: n.visible() }.render(area, &mut buf);

        let rects = boxes(n.visible(), area);
        assert_eq!(rects.len(), 3, "all three fit in 14 rows");
        for r in &rects {
            assert_eq!(r.height, 3, "a toast is three rows: {r:?}");
            assert_eq!(char_at(&buf, r.x, r.y), '┌', "{r:?}");
            assert_eq!(char_at(&buf, r.x + r.width - 1, r.y), '┐', "{r:?}");
            assert_eq!(char_at(&buf, r.x, r.y + 2), '└', "{r:?}");
            assert_eq!(char_at(&buf, r.x + r.width - 1, r.y + 2), '┘', "{r:?}");
        }
        // the converse: no box was painted outside what was reported.
        for y in area.y..area.y + area.height {
            for x in area.x..area.x + area.width {
                if char_at(&buf, x, y) == '┌' {
                    assert!(
                        rects.iter().any(|r| r.x == x && r.y == y),
                        "a box at ({x},{y}) is in no reported rect"
                    );
                }
            }
        }
    }

    #[test]
    fn renders_at_degenerate_sizes_without_panicking() {
        let n = one(Severity::Alert, "something happened");
        for (w, h) in [(1, 1), (2, 3), (0, 8), (40, 0), (5, 2), (6, 3)] {
            let area = Rect::new(0, 0, w, h);
            let mut buf = buffer(w.max(1), h.max(1));
            NotificationOverlay { items: n.visible() }.render(area, &mut buf);
        }
    }
}
