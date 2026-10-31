use std::time::Duration;

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::widgets::Widget;

use crate::anim;
use crate::keymap::{key_label, KeyAction, Keymap};
use crate::search::SearchItem;
use crate::theme::{shade, BRIGHT, DIM, MID, NEAR_BLACK, NEAR_WHITE};
use crate::ui::header::draw_header;
use crate::ui::results_list::{fill_row_bg, label};
use crate::ui::scroll;
use crate::ui::textfit::truncate_with_ellipsis;

/// The queue, finally visible. It was previously a bare count on the
/// bottom bar, which for an app whose central design choice is an
/// explicit queue meant the queue itself was the one thing you could
/// never look at.
///
/// Laid out as three labelled runs in play order rather than one flat
/// list: what's playing, what's coming, and what's been. The ordering
/// is deliberate; "up next" is the part anyone opens this for, so it
/// sits directly under the current track instead of below a history
/// section that grows all session.
pub struct QueueView<'a> {
    /// mpv's whole playlist; `pos` indexes into it.
    pub entries: &'a [SearchItem],
    pub pos: Option<usize>,
    /// Cursor into `entries`, independent of `pos`: you scroll the
    /// queue without disturbing playback.
    pub selected: usize,
    /// Whether the workspace has focus, which decides whether the
    /// selected row pulses or just sits at its static level (same
    /// convention as every other list, see `anim::selection_bg`).
    pub focused: bool,
    pub elapsed: Duration,
    /// Read for the one remappable key this pane names: the empty
    /// queue told you to press `'a'`, baked in, which is exactly the
    /// lie `help::Key::Bound` exists to stop. Every other key named
    /// here is a fixed convention main.rs hardcodes.
    pub keymap: &'a Keymap,
}

/// Rows the view draws for one frame: either a dim section label or a
/// queue entry, carried as its index into `entries` rather than a
/// borrow, since `SearchItem` holds an `Arc<Client>` and so cannot be
/// `PartialEq` (see `SearchItem::identity`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Row {
    Section(&'static str),
    Entry(usize),
}

/// Flattens the playlist into the display order: current, then
/// upcoming, then already-played newest-first. History reads better
/// backwards (the thing you just heard is the thing you're most likely
/// looking for) and it keeps the two interesting sections adjacent.
fn rows(entries: &[SearchItem], pos: Option<usize>) -> Vec<Row> {
    let mut out = Vec::new();
    let played_end = match pos {
        Some(p) if p < entries.len() => {
            out.push(Row::Section("now playing"));
            out.push(Row::Entry(p));
            p
        }
        _ => 0,
    };
    let upcoming_start = match pos {
        Some(p) if p < entries.len() => p + 1,
        _ => 0,
    };
    if upcoming_start < entries.len() {
        out.push(Row::Section("up next"));
        for offset in 0..entries.len() - upcoming_start {
            out.push(Row::Entry(upcoming_start + offset));
        }
    }
    if played_end > 0 {
        out.push(Row::Section("played"));
        for index in (0..played_end).rev() {
            out.push(Row::Entry(index));
        }
    }
    out
}

/// Which display row holds `selected`, so the scroll offset can follow
/// the cursor through the section labels without the labels ever being
/// what's "selected".
fn row_of(rows: &[Row], selected: usize) -> usize {
    rows.iter()
        .position(|r| matches!(r, Row::Entry(i) if *i == selected))
        .unwrap_or(0)
}

/// The rect the entry rows are drawn into, so a hit map knows which
/// absolute row is row 0 of `row_items` (see `hit::List`). The header's
/// height comes from `ui::header`, which is also where it is checked
/// against what `draw_header` actually returns.
pub fn body_rect(area: Rect) -> Rect {
    crate::ui::header::body(area)
}

/// Entry index per visible body row, `None` where `render` drew a
/// section label or ran out of queue; always exactly `body_rect`'s
/// height long, so the caller can index it by row. Same `rows` and
/// offset the render itself uses, so a click resolves to the row the
/// user saw rather than to a second guess at where it landed.
pub fn row_items(
    entries: &[SearchItem],
    pos: Option<usize>,
    selected: usize,
    area: Rect,
) -> Vec<Option<usize>> {
    // the empty queue draws two message rows and no entries, so there
    // is nothing on it to click.
    if area.width == 0 || entries.is_empty() {
        return Vec::new();
    }
    let body_height = body_rect(area).height as usize;
    if body_height == 0 {
        return Vec::new();
    }
    let rows = rows(entries, pos);
    let offset = scroll::offset_for(row_of(&rows, selected), body_height);
    (0..body_height)
        .map(|row| match rows.get(offset + row) {
            Some(Row::Entry(index)) => Some(*index),
            Some(Row::Section(_)) | None => None,
        })
        .collect()
}

/// Entry indices in display order, which is what j/k step through;
/// the caller needs this to move the cursor past a section label in
/// one keypress rather than landing on it.
pub fn selectable(entries: &[SearchItem], pos: Option<usize>) -> Vec<usize> {
    rows(entries, pos)
        .into_iter()
        .filter_map(|r| match r {
            Row::Entry(index) => Some(index),
            Row::Section(_) => None,
        })
        .collect()
}

/// The header's key hint.
///
/// It read "J/K: move" while `K` moved nothing: main.rs tested
/// `Char('K')` for the remap menu unconditionally, *before* this
/// view's own handler, so the `Char('J') | Char('K')` arm here was
/// dead for `K`. That precedence is fixed (the remap check now skips
/// a focused queue view), so both directions are advertised again.
const HEADER_HINT: &str = "enter: jump, d: remove, J/K: move, esc: close";

impl Widget for QueueView<'_> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        if area.width == 0 || area.height == 0 {
            return;
        }
        let upcoming = match self.pos {
            Some(p) => self.entries.len().saturating_sub(p + 1),
            None => self.entries.len(),
        };
        let title = match upcoming {
            0 => "queue".to_string(),
            1 => "queue · 1 up next".to_string(),
            n => format!("queue · {n} up next"),
        };
        let body_top = draw_header(
            buf,
            area,
            area.y,
            &title,
            HEADER_HINT,
            shade(DIM),
            shade(NEAR_BLACK),
        );
        // one derivation of the body geometry, shared with `row_items`.
        let body = body_rect(area);
        debug_assert_eq!(body_top, body.y, "ui::header::ROWS is out of step");
        let body_height = body.height as usize;
        if body_height == 0 {
            return;
        }

        if self.entries.is_empty() {
            // never a blank pane: say what to do, since an empty queue
            // is the state a first-time user is actually in.
            draw_line(
                buf,
                area,
                body_top,
                "nothing queued",
                shade(MID),
                area.width as usize,
            );
            draw_line(
                buf,
                area,
                body_top.saturating_add(1),
                &format!(
                    "enter on a song to start, {} to add",
                    key_label(self.keymap.get(KeyAction::Enqueue))
                ),
                shade(DIM),
                area.width as usize,
            );
            return;
        }

        let rows = rows(self.entries, self.pos);
        let offset = scroll::offset_for(row_of(&rows, self.selected), body_height);
        // the index gutter is sized to the widest number actually on
        // screen, so a 9-track queue doesn't pay for three digits.
        let gutter = self.entries.len().to_string().len().max(2);

        for (row_i, row) in rows.iter().skip(offset).take(body_height).enumerate() {
            let y = body_top + row_i as u16;
            match row {
                Row::Section(name) => {
                    // one blank column of indent so section labels sit
                    // outside the rows they head, not in the same
                    // column as the entry text.
                    draw_line(
                        buf,
                        area,
                        y,
                        &format!(" {name}"),
                        shade(NEAR_BLACK),
                        area.width as usize,
                    );
                }
                Row::Entry(index) => {
                    let item = &self.entries[*index];
                    let is_current = self.pos == Some(*index);
                    let is_selected = *index == self.selected;
                    let is_played = self.pos.is_some_and(|p| *index < p);

                    if is_selected {
                        fill_row_bg(buf, area, y, anim::selection_bg(self.elapsed, self.focused));
                    }
                    // the current track is marked structurally, not by
                    // brightness alone: selection already owns the
                    // background and the two states have to be
                    // readable at the same time on the same row.
                    let marker = if is_current { '▌' } else { ' ' };
                    let level = if is_current {
                        NEAR_WHITE
                    } else if is_played {
                        NEAR_BLACK
                    } else if is_selected {
                        BRIGHT
                    } else {
                        DIM
                    };
                    let text = format!(
                        "{marker}{:>gutter$}. {}",
                        index + 1,
                        label(item),
                        gutter = gutter
                    );
                    draw_line(buf, area, y, &text, shade(level), area.width as usize);
                }
            }
        }
    }
}

fn draw_line(
    buf: &mut Buffer,
    area: Rect,
    y: u16,
    text: &str,
    color: ratatui::style::Color,
    max_width: usize,
) {
    // ellipsis first (it is what signals "there is more"), then the
    // width-correct writer does the clipping and cell advance.
    let fitted = truncate_with_ellipsis(text, max_width);
    crate::ui::draw::line(buf, area, y, &fitted, color);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::testbuf::{buffer, char_at, grid};

    fn song(title: &str) -> SearchItem {
        SearchItem::LocalMusic {
            path: format!("/music/{title}.flac").into(),
            title: title.to_string(),
            artist: Some("Artist".to_string()),
            album: None,
        }
    }

    fn render(entries: &[SearchItem], pos: Option<usize>, selected: usize) -> String {
        let area = Rect::new(0, 0, 44, 12);
        let mut buf = buffer(44, 12);
        QueueView {
            entries,
            pos,
            selected,
            focused: true,
            elapsed: Duration::ZERO,
            keymap: &Keymap::default(),
        }
        .render(area, &mut buf);
        grid(&buf)
    }

    #[test]
    fn sections_run_current_then_upcoming_then_played_backwards() {
        let e: Vec<_> = ["a", "b", "c", "d"].iter().map(|t| song(t)).collect();
        let r = rows(&e, Some(1));
        assert_eq!(
            r,
            vec![
                Row::Section("now playing"),
                Row::Entry(1),
                Row::Section("up next"),
                Row::Entry(2),
                Row::Entry(3),
                Row::Section("played"),
                Row::Entry(0),
            ]
        );
    }

    #[test]
    fn nothing_playing_yet_is_all_upcoming_with_no_empty_sections() {
        let e: Vec<_> = ["a", "b"].iter().map(|t| song(t)).collect();
        let r = rows(&e, None);
        assert_eq!(r.len(), 3, "one label plus two entries: {r:?}");
        assert_eq!(r[0], Row::Section("up next"));
    }

    #[test]
    fn the_last_track_playing_has_no_up_next_section() {
        let e: Vec<_> = ["a", "b"].iter().map(|t| song(t)).collect();
        let r = rows(&e, Some(1));
        assert!(
            !r.contains(&Row::Section("up next")),
            "an empty section must not be labelled: {r:?}"
        );
        assert!(r.contains(&Row::Section("played")));
    }

    #[test]
    fn selectable_skips_the_section_labels() {
        let e: Vec<_> = ["a", "b", "c"].iter().map(|t| song(t)).collect();
        // display order is current (1), upcoming (2), then played (0).
        assert_eq!(selectable(&e, Some(1)), vec![1, 2, 0]);
    }

    #[test]
    fn the_current_track_is_marked_and_the_count_is_in_the_title() {
        let e: Vec<_> = ["aaa", "bbb", "ccc"].iter().map(|t| song(t)).collect();
        let out = render(&e, Some(0), 0);
        assert!(out.contains("queue · 2 up next"), "{out}");
        assert!(out.contains("▌ 1. aaa"), "current track marker:\n{out}");
        assert!(out.contains("  2. bbb"), "{out}");
    }

    #[test]
    fn an_empty_queue_explains_itself_instead_of_rendering_blank() {
        let out = render(&[], None, 0);
        assert!(out.contains("nothing queued"), "{out}");
        assert!(out.contains("a to add"), "{out}");
    }

    #[test]
    fn the_selected_row_gets_a_background_and_others_do_not() {
        let e: Vec<_> = ["aaa", "bbb"].iter().map(|t| song(t)).collect();
        let area = Rect::new(0, 0, 44, 12);
        let mut buf = buffer(44, 12);
        QueueView {
            entries: &e,
            pos: Some(0),
            selected: 1,
            focused: true,
            elapsed: Duration::ZERO,
            keymap: &Keymap::default(),
        }
        .render(area, &mut buf);
        // find the row holding "bbb" and assert it, and only it, is plated
        let plated: Vec<u16> = (0..12)
            .filter(|y| {
                buf.cell((0, *y))
                    .is_some_and(|c| c.bg != ratatui::style::Color::Reset)
            })
            .collect();
        assert_eq!(plated.len(), 1, "exactly one row should be plated");
        let row: String = (0..44)
            .map(|x| buf.cell((x, plated[0])).unwrap().symbol().to_string())
            .collect();
        assert!(
            row.contains("bbb"),
            "the plated row is the selected one: {row}"
        );
    }

    #[test]
    fn scrolls_to_keep_a_far_selection_on_screen() {
        let e: Vec<_> = (0..40).map(|i| song(&format!("t{i}"))).collect();
        let out = render(&e, Some(0), 35);
        assert!(out.contains("36. t35"), "selection must be visible:\n{out}");
    }

    /// What `row_items` claims for each body row, paired with the text
    /// `render` actually painted on that same row.
    fn painted(
        entries: &[SearchItem],
        pos: Option<usize>,
        selected: usize,
        w: u16,
        h: u16,
    ) -> Vec<(Option<usize>, String)> {
        let area = Rect::new(0, 0, w, h);
        let mut buf = buffer(w.max(1), h.max(1));
        QueueView {
            entries,
            pos,
            selected,
            focused: true,
            elapsed: Duration::ZERO,
            keymap: &Keymap::default(),
        }
        .render(area, &mut buf);
        let lines: Vec<String> = grid(&buf).lines().map(str::to_string).collect();
        row_items(entries, pos, selected, area)
            .into_iter()
            .enumerate()
            .map(|(i, item)| (item, lines[crate::ui::header::ROWS as usize + i].clone()))
            .collect()
    }

    /// Every body row's answer is the entry whose text is on it; a
    /// `None` row has to be a section label or untouched fill, never an
    /// entry the map failed to name.
    fn assert_agrees(entries: &[SearchItem], pos: Option<usize>, selected: usize, w: u16, h: u16) {
        for (i, (item, text)) in painted(entries, pos, selected, w, h).iter().enumerate() {
            match item {
                Some(index) => {
                    let want = format!("{}. {}", index + 1, label(&entries[*index]));
                    assert!(
                        text.contains(&want),
                        "body row {i} claims entry {index} but painted {text:?}"
                    );
                }
                None => {
                    let blank = text.chars().all(|c| c == '.');
                    let section = ["now playing", "up next", "played"]
                        .iter()
                        .any(|s| text.contains(s));
                    assert!(
                        blank || section,
                        "body row {i} claims no entry but painted {text:?}"
                    );
                }
            }
        }
    }

    #[test]
    fn row_items_names_the_entry_render_painted_on_each_row() {
        let e: Vec<_> = ["a", "b", "c", "d"].iter().map(|t| song(t)).collect();
        assert_eq!(
            row_items(&e, Some(1), 1, Rect::new(0, 0, 44, 12)),
            vec![
                None, // now playing
                Some(1),
                None, // up next
                Some(2),
                Some(3),
                None, // played
                Some(0),
                // past the end of the list
                None,
                None,
                None,
            ],
            "ten body rows under a two-row header"
        );
        assert_agrees(&e, Some(1), 1, 44, 12);
    }

    #[test]
    fn row_items_follows_the_scroll_offset() {
        let e: Vec<_> = (0..40).map(|i| song(&format!("t{i}"))).collect();
        // a window this far down holds no label and no blank row, so
        // every answer has to be an entry and an offset of 0 would name
        // the wrong one.
        let map = row_items(&e, Some(0), 35, Rect::new(0, 0, 60, 12));
        assert_eq!(map, (26..=35).map(Some).collect::<Vec<_>>());
        assert_agrees(&e, Some(0), 35, 60, 12);

        // and with the selection on the very last entry.
        let map = row_items(&e, Some(0), 39, Rect::new(0, 0, 60, 12));
        assert_eq!(map, (30..=39).map(Some).collect::<Vec<_>>());
        assert_agrees(&e, Some(0), 39, 60, 12);
    }

    #[test]
    fn the_body_rect_is_where_render_starts_drawing_rows() {
        let e: Vec<_> = ["aaa", "bbb"].iter().map(|t| song(t)).collect();
        let area = Rect::new(3, 1, 40, 10);
        let mut buf = buffer(50, 12);
        QueueView {
            entries: &e,
            pos: Some(0),
            selected: 0,
            focused: true,
            elapsed: Duration::ZERO,
            keymap: &Keymap::default(),
        }
        .render(area, &mut buf);
        let body = body_rect(area);
        assert_eq!((body.x, body.y, body.width, body.height), (3, 3, 40, 8));
        let row: String = (body.x..body.x + body.width)
            .map(|x| char_at(&buf, x, body.y))
            .collect();
        assert!(
            row.contains("now playing"),
            "row 0 of the body rect is the first row render drew: {row}"
        );
    }

    #[test]
    fn row_items_at_degenerate_sizes_is_empty_or_exactly_the_body() {
        let e: Vec<_> = ["a", "b"].iter().map(|t| song(t)).collect();
        for (w, h) in [(1, 1), (0, 5), (5, 0), (3, 2), (80, 1), (80, 2), (80, 3)] {
            let area = Rect::new(0, 0, w, h);
            let map = row_items(&e, Some(0), 0, area);
            let body = body_rect(area).height as usize;
            assert!(
                map.is_empty() || map.len() == body,
                "{w}x{h}: {} answers for a {body}-row body",
                map.len()
            );
            assert_agrees(&e, Some(0), 0, w, h);
        }
        // the empty queue draws messages, not entries, so no row on it
        // is clickable.
        assert!(row_items(&[], None, 0, Rect::new(0, 0, 44, 12)).is_empty());
    }

    #[test]
    fn renders_at_degenerate_sizes_without_panicking() {
        let e: Vec<_> = ["a"].iter().map(|t| song(t)).collect();
        for (w, h) in [(1, 1), (0, 5), (5, 0), (3, 2), (80, 1), (80, 2)] {
            let area = Rect::new(0, 0, w, h);
            let mut buf = buffer(w.max(1), h.max(1));
            QueueView {
                entries: &e,
                pos: Some(0),
                selected: 0,
                focused: false,
                elapsed: Duration::ZERO,
                keymap: &Keymap::default(),
            }
            .render(area, &mut buf);
        }
    }

    /// The pane named one remappable key and baked its default in, so
    /// anyone who had rebound enqueue was told to press a key that
    /// does something else.
    #[test]
    fn the_empty_queue_hint_names_the_key_enqueue_is_actually_bound_to() {
        use crossterm::event::KeyCode;
        let mut km = Keymap::default();
        km.set(KeyAction::Enqueue, KeyCode::F(5));
        let area = Rect::new(0, 0, 44, 12);
        let mut buf = buffer(44, 12);
        QueueView {
            entries: &[],
            pos: None,
            selected: 0,
            focused: true,
            elapsed: Duration::ZERO,
            keymap: &km,
        }
        .render(area, &mut buf);
        let g = grid(&buf);
        assert!(g.contains("f5 to add"), "the live binding is missing:\n{g}");
        assert!(
            !g.contains("'a' to add") && !g.contains("a to add"),
            "still teaching the default:\n{g}"
        );
    }

    /// The hint must not advertise a key the app never delivers here.
    /// Both reorder directions are advertised, and the host guard that
    /// makes `K` reach this view at all is pinned here.
    ///
    /// `K` is reserved for the remap menu, and main.rs used to claim
    /// it unconditionally *before* this view's handler, so the
    /// `Char('J') | Char('K')` arm was dead and the hint taught a key
    /// that did nothing. Reading main.rs is crude, but the alternative
    /// is a hint that silently goes back to lying: the hint lives
    /// here and the precedence lives there, and nothing else ties the
    /// two together.
    #[test]
    fn the_header_hint_advertises_both_reorder_directions() {
        assert_eq!(
            crate::keymap::reserved_key_purpose(crossterm::event::KeyCode::Char('K')),
            Some("the remap menu itself"),
            "K stopped opening the remap menu; see HEADER_HINT"
        );
        let host = include_str!("../main.rs");
        let at = host
            .find("if key.code == KeyCode::Char('K')")
            .expect("main.rs no longer claims K for the remap menu");
        let check = &host[at..at + 200];
        assert!(
            check.contains("queue_view_open"),
            "the remap menu claims K unconditionally again, so this \
             view never receives it and the hint below is a lie:\n{check}"
        );

        assert!(HEADER_HINT.contains("J/K: move"), "{HEADER_HINT}");
        // and on a pane wide enough to hold the whole hint, so this is
        // the drawn text and not just the constant.
        let e: Vec<_> = ["aaa"].iter().map(|t| song(t)).collect();
        let mut buf = buffer(80, 8);
        QueueView {
            entries: &e,
            pos: Some(0),
            selected: 0,
            focused: true,
            elapsed: Duration::ZERO,
            keymap: &Keymap::default(),
        }
        .render(Rect::new(0, 0, 80, 8), &mut buf);
        let g = grid(&buf);
        assert!(g.contains("J/K: move"), "{g}");
    }
}

#[cfg(test)]
mod eyeball {
    use super::*;
    use crate::ui::testbuf::{buffer, grid};
    use crossterm::event::KeyCode;

    fn song(title: &str) -> SearchItem {
        SearchItem::LocalMusic {
            path: format!("/music/{title}.flac").into(),
            title: title.to_string(),
            artist: Some("Artist".to_string()),
            album: None,
        }
    }

    /// What to draw and at what size: a label, the queue, the play
    /// position, the cursor, the keymap the hint resolves against, and
    /// the pane's cells.
    type Case<'a> = (
        &'a str,
        &'a [SearchItem],
        Option<usize>,
        usize,
        &'a Keymap,
        u16,
        u16,
    );

    /// The pane at the widths the workspace actually hands it, plus the
    /// empty state a first-time user lands on and a rebound enqueue:
    /// `cargo test --lib ui::queue::eyeball -- --nocapture`.
    /// Asserts the floor too, so it is a test and not just a printer.
    #[test]
    fn print_the_queue_pane_and_its_empty_state() {
        let mut rebound = Keymap::default();
        rebound.set(KeyAction::Enqueue, KeyCode::F(5));
        let full: Vec<SearchItem> = (0..12).map(|i| song(&format!("track {i}"))).collect();
        let cases: [Case; 6] = [
            (
                "empty, default binding",
                &[],
                None,
                0,
                &Keymap::default(),
                60,
                10,
            ),
            (
                "empty, enqueue rebound to f5",
                &[],
                None,
                0,
                &rebound,
                60,
                10,
            ),
            ("empty, narrow", &[], None, 0, &Keymap::default(), 30, 6),
            ("mid queue", &full, Some(3), 3, &Keymap::default(), 60, 14),
            (
                "scrolled to the end",
                &full,
                Some(0),
                11,
                &Keymap::default(),
                60,
                10,
            ),
            (
                "the hint's own width",
                &full,
                Some(3),
                3,
                &Keymap::default(),
                48,
                8,
            ),
        ];
        for (what, entries, pos, selected, keymap, w, h) in cases {
            let mut buf = buffer(w, h);
            QueueView {
                entries,
                pos,
                selected,
                focused: true,
                elapsed: Duration::ZERO,
                keymap,
            }
            .render(Rect::new(0, 0, w, h), &mut buf);
            let text = grid(&buf);
            println!("--- {w}x{h} {what}\n{text}");
            assert!(text.contains("queue"), "{what} lost its title:\n{text}");
        }
    }
}
