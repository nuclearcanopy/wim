use std::time::Duration;

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::widgets::Widget;

use crate::anim;
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
            "enter: jump here, esc: close",
            shade(DIM),
            shade(NEAR_BLACK),
        );
        let body_height = (area.y + area.height).saturating_sub(body_top) as usize;
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
                "enter on a song to start, 'a' to add",
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
    if y >= area.y + area.height {
        return;
    }
    let fitted = truncate_with_ellipsis(text, max_width);
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
    use crate::ui::testbuf::{buffer, grid};

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
        assert!(out.contains("'a' to add"), "{out}");
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
            }
            .render(area, &mut buf);
        }
    }
}
