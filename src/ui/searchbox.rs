use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::widgets::Widget;

use crate::theme::{lerp, shade, DIM, MID, NEAR_BLACK, NEAR_WHITE};
use crate::ui::textfit::tail_fit;

/// A hand-drawn box around a search input, labelled in its own top
/// border so two of these on screen at once can't be confused for each
/// other. There are exactly two (the sidebar's library search and the
/// artist profile's song filter), and "i am trying to type in the song
/// search section but i am typing in the sidebar search" was the
/// result of them rendering identically.
pub struct SearchBox<'a> {
    pub query: &'a str,
    pub editing: bool,
    /// Short name drawn into the top border (`┤ search ├`). Says which
    /// box this is at all times, not only while it happens to be the
    /// one being typed into.
    pub label: &'a str,
    /// Blink phase for the terminal-style cursor block; only matters
    /// while `editing`. The caller toggles this on a timer so the
    /// widget itself stays a pure render of whatever state it's given.
    pub cursor_visible: bool,
    /// `false` for the sidebar's search box only: the sidebar's own
    /// vertical divider sits one column past `area`'s right edge
    /// already, so a second parallel line right next to it just
    /// looked like a disconnected box bolted onto the layout rather
    /// than part of it. Closing that side is main.rs's job instead,
    /// via the vdivider's own `junction_rows` at this box's top/bottom
    /// rows (┬ and ┤). Every other caller has nothing else to close
    /// it, so they need `true` or the box's right edge just trails
    /// off with nothing to meet it.
    pub right_border: bool,
}

impl Widget for SearchBox<'_> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        if area.width < 3 || area.height < 3 {
            return;
        }
        // the whole frame brightens while this box owns the keyboard.
        // the blinking cursor used to be the only signal, which meant
        // half of every blink cycle showed no indicator at all.
        let border_color = shade(if self.editing { MID } else { NEAR_BLACK });
        let w = area.width;
        let h = area.height;

        put(buf, area.x, area.y, '┌', border_color);
        put(buf, area.x, area.y + h - 1, '└', border_color);
        let top_bottom_end = if self.right_border { w - 1 } else { w };
        for x in 1..top_bottom_end {
            put(buf, area.x + x, area.y, '─', border_color);
            put(buf, area.x + x, area.y + h - 1, '─', border_color);
        }
        for y in 1..h - 1 {
            put(buf, area.x, area.y + y, '│', border_color);
        }
        if self.right_border {
            put(buf, area.x + w - 1, area.y, '┐', border_color);
            put(buf, area.x + w - 1, area.y + h - 1, '┘', border_color);
            for y in 1..h - 1 {
                put(buf, area.x + w - 1, area.y + y, '│', border_color);
            }
        }

        draw_border_label(buf, area, self.label, self.editing, border_color);

        let inner_width = if self.right_border {
            w.saturating_sub(2)
        } else {
            w.saturating_sub(1)
        } as usize;
        // the placeholder only makes sense when there's nothing typed
        // *and* nothing being typed; once editing starts on an empty
        // query, the cursor alone is the "you're in here" signal, not
        // placeholder text fighting it for the same cell.
        let (text, color) = if self.query.is_empty() && !self.editing {
            ("search...".to_string(), shade(DIM))
        } else {
            (tail_fit(self.query, inner_width), shade(NEAR_WHITE))
        };
        for (col, ch) in text.chars().enumerate() {
            if col >= inner_width {
                break;
            }
            put(buf, area.x + 1 + col as u16, area.y + 1, ch, color);
        }

        if self.editing {
            // never fully invisible: it dims on the off phase instead
            // of disappearing, so "is this box focused" is answerable
            // from any single frame.
            let cursor_col = text.chars().count().min(inner_width.saturating_sub(1));
            let level = if self.cursor_visible { 1.0 } else { 0.45 };
            put(
                buf,
                area.x + 1 + cursor_col as u16,
                area.y + 1,
                '█',
                shade(lerp(NEAR_BLACK, NEAR_WHITE, level)),
            );
        }
    }
}

/// `┤ label ├` set into the top border, left-aligned after the corner.
/// Skipped entirely when the box is too narrow to hold it without
/// eating the whole border, rather than truncating to a fragment.
fn draw_border_label(
    buf: &mut Buffer,
    area: Rect,
    label: &str,
    editing: bool,
    border_color: ratatui::style::Color,
) {
    if label.is_empty() {
        return;
    }
    let text: Vec<char> = label.chars().collect();
    // "┤ " + label + " ├", starting one cell in from the corner.
    let needed = text.len() + 4;
    if (needed + 2) > area.width as usize {
        return;
    }
    let mut x = area.x + 1;
    put(buf, x, area.y, '┤', border_color);
    x += 1;
    put(buf, x, area.y, ' ', border_color);
    x += 1;
    let label_color = shade(if editing { NEAR_WHITE } else { DIM });
    for ch in text {
        put(buf, x, area.y, ch, label_color);
        x += 1;
    }
    put(buf, x, area.y, ' ', border_color);
    x += 1;
    put(buf, x, area.y, '├', border_color);
}

fn put(buf: &mut Buffer, x: u16, y: u16, ch: char, color: ratatui::style::Color) {
    if let Some(cell) = buf.cell_mut((x, y)) {
        cell.set_char(ch);
        cell.set_fg(color);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn render_to_string(sb: SearchBox, width: u16, height: u16) -> String {
        let area = Rect::new(0, 0, width, height);
        let mut buf = Buffer::empty(area);
        sb.render(area, &mut buf);
        (0..height)
            .map(|y| {
                (0..width)
                    .map(|x| buf.cell((x, y)).unwrap().symbol().to_string())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn the_label_is_set_into_the_top_border() {
        let out = render_to_string(
            SearchBox {
                query: "",
                editing: false,
                label: "search",
                cursor_visible: false,
                right_border: true,
            },
            24,
            3,
        );
        assert!(
            out.lines().next().unwrap().contains("┤ search ├"),
            "got:\n{out}"
        );
    }

    #[test]
    fn two_boxes_are_distinguishable_by_their_labels() {
        let sidebar = render_to_string(
            SearchBox {
                query: "",
                editing: false,
                label: "search",
                cursor_visible: false,
                right_border: true,
            },
            24,
            3,
        );
        let filter = render_to_string(
            SearchBox {
                query: "",
                editing: false,
                label: "filter songs",
                cursor_visible: false,
                right_border: true,
            },
            24,
            3,
        );
        assert_ne!(sidebar, filter, "the two boxes rendered identically");
    }

    #[test]
    fn a_narrow_box_drops_the_label_instead_of_a_fragment() {
        let out = render_to_string(
            SearchBox {
                query: "",
                editing: false,
                label: "filter songs",
                cursor_visible: false,
                right_border: true,
            },
            10,
            3,
        );
        let top = out.lines().next().unwrap();
        assert!(!top.contains('┤'), "half a label is worse than none: {top}");
        assert_eq!(top, "┌────────┐");
    }

    #[test]
    fn the_cursor_is_drawn_on_both_blink_phases_while_editing() {
        for visible in [true, false] {
            let out = render_to_string(
                SearchBox {
                    query: "bla",
                    editing: true,
                    label: "search",
                    cursor_visible: visible,
                    right_border: true,
                },
                24,
                3,
            );
            assert!(
                out.contains('█'),
                "cursor vanished on blink phase {visible}, leaving no focus signal"
            );
        }
    }

    #[test]
    fn an_unfocused_box_shows_the_placeholder_and_no_cursor() {
        let out = render_to_string(
            SearchBox {
                query: "",
                editing: false,
                label: "search",
                cursor_visible: true,
                right_border: true,
            },
            24,
            3,
        );
        assert!(out.contains("search..."));
        assert!(!out.contains('█'));
    }
}
