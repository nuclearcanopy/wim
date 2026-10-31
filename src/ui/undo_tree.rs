//! The undo tree, drawn.
//!
//! `docs/undo.md` calls this "the part most likely to ship barebones",
//! so the bar it sets is written down there: one row per node with its
//! label and a relative time, the current node marked unambiguously,
//! branch structure in the box-drawing glyphs the rest of the ui uses
//! rather than ascii pipes, and barrier nodes marked so "why can I not
//! go further" is answerable from the screen instead of from a
//! notification.
//!
//! Monochrome, so brightness carries the hierarchy: the current node
//! is brightest, its ancestors (the path you are actually on) are mid,
//! and other branches are dim. That is the one thing a tree view
//! genuinely needs colour for, and it is the one thing this ui will
//! not spend.

use std::time::Duration;

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::widgets::Widget;

use crate::anim;
use crate::theme::{shade, BRIGHT, DIM, MID, NEAR_BLACK, NEAR_WHITE};
use crate::ui::draw;
use crate::ui::results_list::fill_row_bg;
use crate::ui::textfit::{truncate_with_ellipsis, width};
use crate::undo::Row;

/// Marks a node whose op cannot be reversed. Plain ascii on purpose:
/// the one thing `banner`'s glyph audit established is that nominally
/// geometric symbols can still carry an emoji presentation, and this
/// one sits inside a tree whose alignment depends on single cells.
const BARRIER_MARK: char = '!';

/// The view's own keys, as constants both the hint line and main.rs's
/// handler read.
///
/// Same reason `eq_menu` does it: a hint that spells its own letter
/// can describe a key the handler never acts on, which is exactly how
/// stale hints happened here before.
pub const UNDO_KEY: char = 'u';
pub const REDO_KEY: char = 'r';
pub const LEFT_KEY: char = 'h';
pub const RIGHT_KEY: char = 'l';

/// Tree glyphs, from the same box-drawing block the dividers and
/// junctions already use.
const TEE: &str = "├─";
const CORNER: &str = "└─";
const SPINE: &str = "│ ";
const CLEAR: &str = "  ";

/// "just now" / "12s" / "4m" / "2h", for the age column.
///
/// Deliberately not `progress::fmt_span`, which spells a span in full
/// ("1h30m") because someone chose it. An age is read at a glance and
/// only its largest unit matters; "1h30m ago" is noise where "1h ago"
/// is the answer.
pub fn fmt_age(age: Duration) -> String {
    let s = age.as_secs();
    match s {
        0..=2 => "just now".to_string(),
        3..=59 => format!("{s}s ago"),
        60..=3599 => format!("{}m ago", s / 60),
        _ => format!("{}h ago", s / 3600),
    }
}

/// The glyph column for a row, from its depth and whether each
/// ancestor's branch is still open below it.
///
/// `spine[d]` is "the ancestor at depth d has siblings after it", so
/// its line continues past this row. Built by the caller as it walks,
/// since a single row cannot know it.
fn prefix(row: &Row, spine: &[bool]) -> String {
    if row.depth == 0 {
        return String::new();
    }
    let mut out = String::with_capacity(row.depth * 2);
    // `1..depth`, not `0..depth-1`: the entries that matter are the
    // *ancestors* at depths 1 through depth-1, and depth 0 is the root,
    // which has no column of its own. indexing from 0 drew a child of a
    // forked node with a blank gap where its parent's line should
    // continue, which the box-drawing test did not catch because the
    // glyphs were all still box-drawing. found by printing it.
    for open in spine.iter().take(row.depth).skip(1) {
        out.push_str(if *open { SPINE } else { CLEAR });
    }
    out.push_str(if row.last_child { CORNER } else { TEE });
    out
}

pub struct UndoTreeView<'a> {
    pub rows: &'a [Row<'a>],
    /// Drives the current row's plate pulse, like every other
    /// selected row in the app (`anim::selection_bg`).
    pub elapsed: Duration,
    /// Whether this pane has focus, which is the difference between a
    /// breathing plate and a flat one.
    pub focused: bool,
}

impl Widget for UndoTreeView<'_> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        if area.width < 10 || area.height < 4 {
            // too small to say anything: better blank than a frame
            // full of clipped glyphs.
            return;
        }
        // `overlay`, not a bare frame: it scrims every cell first, so
        // the workspace underneath cannot show through the gaps
        // between rows. a frame alone left it visible between the
        // branch lines.
        let inner = draw::overlay(buf, area);
        if inner.width < 8 || inner.height < 2 {
            return;
        }

        draw::at(
            buf,
            inner.x,
            inner.y,
            "undo tree",
            shade(BRIGHT),
            inner.width as usize,
        );

        // the barrier footer, if the current node has one, owns the
        // last row: "why can I not go further" is the question this
        // view exists to answer, and a toast would have scrolled away
        // by the time anyone opened it.
        let barrier = self.rows.iter().find(|r| r.current).and_then(|r| r.barrier);
        let footer = barrier.filter(|_| inner.height >= 5);
        // the hint owns the bottom row whenever there is one to spare.
        // an undiscoverable modal is the failure mode this view was
        // most likely to ship with: `u`, `r` and `h`/`l` are not
        // guessable from a tree on screen.
        let hint = inner.height >= 4;
        let reserved = 2 + u16::from(footer.is_some()) + u16::from(hint);
        let body_h = inner.height.saturating_sub(reserved);
        let body_y = inner.y + 2;

        // the current row is what has to stay on screen, so the window
        // is scrolled around it rather than pinned to the top.
        let current = self.rows.iter().position(|r| r.current).unwrap_or(0);
        let first = crate::ui::scroll::offset_for(current, body_h as usize);

        let mut spine: Vec<bool> = Vec::new();
        for (i, row) in self.rows.iter().enumerate() {
            // the spine has to be maintained across every row, not
            // only the drawn ones, or a scrolled view draws the wrong
            // branch lines.
            if spine.len() <= row.depth {
                spine.resize(row.depth + 1, false);
            }
            spine.truncate(row.depth + 1);
            spine[row.depth] = !row.last_child;

            if i < first || i >= first + body_h as usize {
                continue;
            }
            let y = body_y + (i - first) as u16;
            if y >= inner.bottom() {
                break;
            }
            self.draw_row(buf, inner, y, row, &spine);
        }

        if hint {
            let text = format!(
                "{UNDO_KEY} undo · {REDO_KEY} redo · {LEFT_KEY}/{RIGHT_KEY} branch · esc close"
            );
            draw::at(
                buf,
                inner.x,
                inner.bottom() - 1,
                &truncate_with_ellipsis(&text, inner.width as usize),
                shade(DIM),
                inner.width as usize,
            );
        }

        if let Some(why) = footer {
            let y = inner.bottom() - 1 - u16::from(hint);
            let text = format!("{BARRIER_MARK} {why}");
            draw::at(
                buf,
                inner.x,
                y,
                &truncate_with_ellipsis(&text, inner.width as usize),
                shade(MID),
                inner.width as usize,
            );
        }
    }
}

impl UndoTreeView<'_> {
    fn draw_row(&self, buf: &mut Buffer, inner: Rect, y: u16, row: &Row, spine: &[bool]) {
        if row.current {
            fill_row_bg(
                buf,
                inner,
                y,
                anim::selection_bg(self.elapsed, self.focused),
            );
        }
        // brightness is the hierarchy: where you are, the path you are
        // on, and everything else.
        let level = match (row.current, row.on_path) {
            (true, _) => NEAR_WHITE,
            (_, true) => MID,
            _ => DIM,
        };

        let age = fmt_age(row.age);
        let age_w = width(&age) as u16;
        // the age column is reserved up front so labels cannot shove
        // it around; a row too narrow to hold both drops the age,
        // since the label is the part that identifies the node.
        let (age_room, gap) = if inner.width > age_w + 12 {
            (age_w, 1)
        } else {
            (0, 0)
        };

        let tree = prefix(row, spine);
        let mut x = draw::at(
            buf,
            inner.x,
            y,
            &tree,
            // the branch lines stay dim even on the current row: they
            // are structure, not content, and lighting them up made
            // the row read as two things.
            shade(if row.on_path { DIM } else { NEAR_BLACK }),
            inner.width as usize,
        );

        if row.barrier.is_some() {
            x = draw::at(buf, x, y, "! ", shade(level), 2);
        }

        let label_room = inner
            .width
            .saturating_sub(x - inner.x)
            .saturating_sub(age_room + gap) as usize;
        draw::at(
            buf,
            x,
            y,
            &truncate_with_ellipsis(row.label, label_room),
            shade(level),
            label_room,
        );

        if age_room > 0 {
            draw::at(
                buf,
                inner.right() - age_w,
                y,
                &age,
                shade(if row.current { MID } else { NEAR_BLACK }),
                age_w as usize,
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::testbuf::{buffer, grid};
    use crate::undo::Tree;

    fn sample() -> (Tree<&'static str>, Duration) {
        let t0 = Duration::ZERO;
        let mut t = Tree::new("start", t0);
        t.record("a", "played crest".into(), None, None, t0);
        t.record("b", "volume 60".into(), None, None, t0);
        t.undo();
        t.undo();
        t.record("c", "queued 12 tracks".into(), None, None, t0);
        t.record(
            "d",
            "download".into(),
            None,
            Some("a download wrote a file"),
            t0,
        );
        (t, t0)
    }

    fn render(w: u16, h: u16) -> String {
        let (t, t0) = sample();
        let rows = t.rows(t0 + Duration::from_secs(90));
        let mut buf = buffer(w, h);
        UndoTreeView {
            rows: &rows,
            elapsed: Duration::ZERO,
            focused: true,
        }
        .render(Rect::new(0, 0, w, h), &mut buf);
        grid(&buf)
    }

    #[test]
    fn ages_read_at_a_glance_rather_than_in_full() {
        assert_eq!(fmt_age(Duration::from_secs(0)), "just now");
        assert_eq!(fmt_age(Duration::from_secs(2)), "just now");
        assert_eq!(fmt_age(Duration::from_secs(12)), "12s ago");
        assert_eq!(fmt_age(Duration::from_secs(260)), "4m ago");
        // the whole point of not reusing `fmt_span`: an age wants its
        // largest unit, not "1h30m ago".
        assert_eq!(fmt_age(Duration::from_secs(5400)), "1h ago");
    }

    #[test]
    fn the_structure_is_box_drawing_not_ascii_pipes() {
        let out = render(50, 14);
        assert!(out.contains('├') || out.contains('└'), "{out}");
        assert!(!out.contains('|'), "no ascii pipes: {out}");
        assert!(!out.contains('+'), "{out}");
    }

    /// The bug printing this found: a node whose parent still has
    /// siblings below needs that parent's line continued through its
    /// own row, or the tree reads as if the branch ended.
    #[test]
    fn a_forked_parents_line_continues_through_its_children() {
        let out = render(46, 12);
        let deep = out
            .lines()
            .find(|l| l.contains("volume 60"))
            .expect("the nested row");
        assert!(
            deep.contains(&format!("{SPINE}{CORNER}")),
            "expected a continuing spine before the corner: {deep:?}"
        );
    }

    #[test]
    fn every_node_gets_a_row_with_its_label() {
        let out = render(50, 14);
        for label in ["start", "played crest", "volume 60", "queued 12 tracks"] {
            assert!(out.contains(label), "{label} missing from:\n{out}");
        }
    }

    /// The question this view exists to answer has to be answerable
    /// from the screen, not from a notification that has scrolled away.
    #[test]
    fn a_barrier_is_marked_and_its_reason_is_spelled_out() {
        let out = render(50, 14);
        assert!(out.contains("! download"), "the row is marked:\n{out}");
        assert!(
            out.contains("a download wrote a file"),
            "and the reason is given:\n{out}"
        );
    }

    /// Brightness is the only hierarchy available, so the three tiers
    /// have to actually differ.
    #[test]
    fn current_ancestors_and_other_branches_differ_in_brightness() {
        let (t, t0) = sample();
        let rows = t.rows(t0);
        let current = rows.iter().find(|r| r.current).expect("one current row");
        assert_eq!(current.label, "download");
        let ancestor = rows
            .iter()
            .find(|r| r.on_path && !r.current && r.depth > 0)
            .expect("an ancestor between root and current");
        assert_eq!(ancestor.label, "queued 12 tracks");
        let other = rows
            .iter()
            .find(|r| !r.on_path)
            .expect("the branch left behind");
        assert_eq!(other.label, "played crest");
    }

    #[test]
    fn the_current_row_is_plated_like_every_other_selection() {
        let (t, t0) = sample();
        let rows = t.rows(t0);
        let at = rows.iter().position(|r| r.current).unwrap();
        let mut buf = buffer(50, 14);
        UndoTreeView {
            rows: &rows,
            elapsed: Duration::ZERO,
            focused: true,
        }
        .render(Rect::new(0, 0, 50, 14), &mut buf);
        // body starts two rows below the frame's inner top.
        let y = 1 + 2 + at as u16;
        let bg = buf.cell((3, y)).unwrap().bg;
        assert_eq!(bg, shade(anim::selection_bg(Duration::ZERO, true)));
    }

    /// The bug the first draft shipped with: a bare frame left the
    /// workspace visible in the gaps between branch lines. `overlay`
    /// scrims every cell, so an overlay owns its whole rect.
    #[test]
    fn the_workspace_cannot_show_through_between_the_rows() {
        let (t, t0) = sample();
        let rows = t.rows(t0);
        let area = Rect::new(0, 0, 46, 12);
        let mut buf = crate::ui::testbuf::plated_buffer(46, 12);
        UndoTreeView {
            rows: &rows,
            elapsed: Duration::ZERO,
            focused: true,
        }
        .render(area, &mut buf);
        crate::ui::testbuf::assert_no_bleed_through(&buf, area);
    }

    /// `u`, `r` and `h`/`l` are not guessable from a tree on screen,
    /// and the hint has to name the keys the handler actually acts on;
    /// both read these constants.
    #[test]
    fn the_keys_are_advertised() {
        let out = render(46, 12);
        for key in [UNDO_KEY, REDO_KEY, LEFT_KEY, RIGHT_KEY] {
            assert!(out.contains(key), "{key} unadvertised:\n{out}");
        }
        assert!(out.contains("esc"), "{out}");
    }

    #[test]
    fn nothing_is_painted_outside_the_rect() {
        let (t, t0) = sample();
        let rows = t.rows(t0);
        let mut buf = buffer(60, 20);
        let area = Rect::new(5, 3, 40, 12);
        UndoTreeView {
            rows: &rows,
            elapsed: Duration::ZERO,
            focused: true,
        }
        .render(area, &mut buf);
        crate::ui::testbuf::assert_outside_untouched(&buf, area);
    }

    /// A pane too small for the tree must draw nothing rather than
    /// half a frame full of clipped glyphs.
    #[test]
    fn a_tiny_pane_declines_instead_of_drawing_rubble() {
        let out = render(6, 3);
        assert!(!out.contains('├'), "{out}");
        assert!(!out.contains("undo"), "{out}");
    }

    /// Prints the view so the glyph column, the age column and the
    /// footer can be read in place:
    /// `cargo test --lib undo_tree::tests::print -- --nocapture`.
    #[test]
    fn print_the_tree() {
        for (w, h) in [(46u16, 12u16), (30, 10)] {
            println!("--- {w}x{h}\n{}", render(w, h));
        }
    }
}
