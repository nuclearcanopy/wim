use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::widgets::Widget;

use crate::theme::{lerp, shade, DIM, MID, NEAR_BLACK, NEAR_WHITE};
use crate::ui::textfit::tail_fit;

/// A hand-drawn box around a search input: "search..." as a dim
/// placeholder when empty, replaced by the live query once typing
/// starts. Labelled in its own top border so the two instances on
/// screen (the sidebar's library search and the artist profile's song
/// filter) can't be confused for each other; "i am trying to type in
/// the song search section but i am typing in the sidebar search" was
/// the result of them rendering identically.
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
    /// `false` only for the sidebar instance, whose right edge is
    /// closed by the vertical divider sitting one column past `area`
    /// (see `ui::junctions`); a second parallel line right next to
    /// that divider read as a box bolted onto the layout. Anything
    /// else has nothing to its right, so it closes its own edge
    /// rather than trailing off into the terminal.
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

        // the border columns (the left one always, the right one only
        // when it's drawn) plus a cell of breathing room inside each:
        // the same inset `draw::framed_inner` gives every other box in
        // the ui, where the query used to sit flush against the `│`.
        let text_x = area.x + 2;
        let borders = if self.right_border { 2 } else { 1 };
        let inner_width = w.saturating_sub(borders + 2) as usize;
        // the placeholder only makes sense when there's nothing typed
        // *and* nothing being typed; once editing starts on an empty
        // query, the cursor alone is the "you're in here" signal, not
        // placeholder text fighting it for the same cell. just the
        // ellipsis, not the word: the label in the border already says
        // which box this is, so spelling it out again read as the word
        // "search" twice over.
        let (text, color) = if self.query.is_empty() && !self.editing {
            ("...".to_string(), shade(DIM))
        } else {
            (tail_fit(self.query, inner_width), shade(NEAR_WHITE))
        };
        // the writer returns where it stopped, which is exactly where
        // the cursor belongs; deriving it from a char count instead put
        // the block inside the last glyph for any wide character.
        let text_end = crate::ui::draw::at(buf, text_x, area.y + 1, &text, color, inner_width);

        // no room inside at all means no cell the cursor could take
        // that is not a border; it used to land on the right `│`.
        if self.editing && inner_width > 0 {
            // never fully invisible: it dims on the off phase instead
            // of disappearing, so "is this box focused" is answerable
            // from any single frame.
            let cursor_col = (text_end.saturating_sub(text_x) as usize).min(inner_width - 1);
            let level = if self.cursor_visible { 1.0 } else { 0.45 };
            put(
                buf,
                text_x + cursor_col as u16,
                area.y + 1,
                '█',
                shade(lerp(NEAR_BLACK, NEAR_WHITE, level)),
            );
        }
    }
}

/// `┤ label ├` set into the top border, after a `─` lead-in whenever
/// the box can afford one: `┌┤` butts two line-drawing glyphs straight
/// against each other, and a dash either side reads as a label set
/// into the line rather than wedged into the corner. Skipped entirely
/// when the box is too narrow to hold it without eating the whole
/// border, rather than truncating to a fragment.
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
    // "┤ " + label + " ├", measured in cells rather than chars so a
    // wide-glyph label is sized by what it occupies.
    let bare = crate::ui::textfit::width(label) + 4;
    let room = area.width as usize;
    if bare + 2 > room {
        return;
    }
    // a spare cell each side of the bare form is what buys the lead-in;
    // without it the label keeps its old flush-to-the-corner spot
    // rather than being dropped.
    let lead = u16::from(bare + 4 <= room);
    let x = area.x + 1 + lead;
    let label_color = shade(if editing { NEAR_WHITE } else { DIM });
    let x = crate::ui::draw::at(buf, x, area.y, "┤ ", border_color, 2);
    let x = crate::ui::draw::at(buf, x, area.y, label, label_color, bare - 4);
    crate::ui::draw::at(buf, x, area.y, " ├", border_color, 2);
}

fn put(buf: &mut Buffer, x: u16, y: u16, ch: char, color: ratatui::style::Color) {
    crate::ui::draw::put(buf, x, y, ch, color);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::testbuf::{buffer, grid};

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
        assert!(out.contains("..."));
        assert!(!out.contains('█'));
    }

    fn render(query: &str, right_border: bool) -> String {
        let mut buf = buffer(10, 3);
        SearchBox {
            query,
            editing: false,
            label: "search",
            cursor_visible: false,
            right_border,
        }
        .render(Rect::new(0, 0, 10, 3), &mut buf);
        grid(&buf)
    }

    /// The sidebar instance: open on the right on purpose, the
    /// vertical divider one column over closes it.
    #[test]
    fn open_right_edge_leaves_the_border_unterminated() {
        assert_eq!(
            render("", false),
            ["┌─────────", "│.........", "└─────────"].join("\n")
        );
    }

    /// Standalone instance (the artist profile's song filter): nothing
    /// is to its right, so it closes its own corners.
    #[test]
    fn closed_right_edge_draws_its_own_corners() {
        assert_eq!(
            render("", true),
            ["┌────────┐", "│........│", "└────────┘"].join("\n")
        );
    }

    /// A cell of breathing room each side, so the tail of the query
    /// shown is two cells shorter than the box is wide; it used to run
    /// straight into both `│`.
    #[test]
    fn closed_right_edge_keeps_the_query_inside_the_box() {
        assert_eq!(
            render("abcdefghij", true),
            ["┌────────┐", "│.efghij.│", "└────────┘"].join("\n")
        );
    }

    /// `│bladee` had the query touching the border it was inside, the
    /// one box in the ui without the inset `draw::framed_inner` gives
    /// every other.
    #[test]
    fn the_query_keeps_a_cell_off_both_borders() {
        for w in 5..=40u16 {
            for right in [false, true] {
                let mut buf = buffer(w, 3);
                SearchBox {
                    query: "a query long enough to fill any of these boxes",
                    editing: true,
                    label: "search",
                    cursor_visible: true,
                    right_border: right,
                }
                .render(Rect::new(0, 0, w, 3), &mut buf);
                let row: Vec<char> = grid(&buf).lines().nth(1).unwrap().chars().collect();
                assert_eq!(row[1], '.', "{w} cols: text against the left border");
                let last = w as usize - 1;
                assert_eq!(
                    row[if right { last - 1 } else { last }],
                    '.',
                    "{w} cols (right_border={right}): text against the right edge"
                );
                // and the cursor is one of the cells between them.
                assert!(
                    row[2..last].contains(&'█'),
                    "{w} cols: the cursor is not inside the box: {row:?}"
                );
            }
        }
    }

    /// `┌┤ search ├` butted the label's own line-drawing glyph against
    /// the corner's; a dash either side sets it into the border.
    #[test]
    fn a_box_with_room_sets_the_label_into_the_line() {
        for w in 14..=40u16 {
            let out = render_to_string(
                SearchBox {
                    query: "",
                    editing: false,
                    label: "search",
                    cursor_visible: false,
                    right_border: true,
                },
                w,
                3,
            );
            let top = out.lines().next().unwrap();
            assert!(
                top.starts_with("┌─┤ search ├"),
                "{w} cols wedged the label into the corner: {top}"
            );
        }
    }
}

#[cfg(test)]
mod eyeball {
    use super::*;
    use crate::ui::testbuf::{buffer, grid};

    /// Prints the box at the sidebar widths a resize actually produces
    /// plus the degenerate ones:
    /// `cargo test --lib ui::searchbox::eyeball -- --nocapture`.
    /// Asserts the floor too, so it is a test and not just a printer.
    #[test]
    fn print_the_box() {
        for (w, h, query, editing, label, right) in [
            (28u16, 3u16, "", false, "search", false),
            (28, 3, "bladee", true, "search", false),
            (28, 3, "", true, "search", false),
            (
                28,
                3,
                "a query long enough to scroll past the box",
                true,
                "search",
                false,
            ),
            (24, 3, "", false, "filter songs", true),
            (18, 3, "", false, "filter songs", true),
            (12, 3, "", false, "filter songs", true),
            (6, 3, "ab", true, "search", true),
            (28, 5, "bladee", false, "search", true),
        ] {
            let area = Rect::new(0, 0, w, h);
            let mut buf = buffer(w, h);
            SearchBox {
                query,
                editing,
                label,
                cursor_visible: true,
                right_border: right,
            }
            .render(area, &mut buf);
            let text = grid(&buf);
            println!("--- {w}x{h} {label:?} query={query:?} editing={editing}\n{text}");
            assert!(
                text.chars().any(|c| c != '.' && c != '\n'),
                "{w}x{h} drew nothing at all"
            );
        }
    }
}
