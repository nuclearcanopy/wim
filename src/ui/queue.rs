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
use crate::ui::textfit::truncate_into;

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

/// Where every display row of the queue lands, as an answer per row
/// rather than as a sequence.
///
/// The display order is current, then upcoming, then already-played
/// newest-first. History reads better backwards (the thing you just
/// heard is the thing you're most likely looking for) and it keeps the
/// two interesting sections adjacent.
///
/// That whole sequence is a function of the queue's length and play
/// position alone: three labelled runs whose lengths both of those
/// fix, so a couple of comparisons place any row and the cursor's own
/// row is arithmetic rather than a scan. The `Vec<Row>` this replaces
/// was one entry per queue entry, built twice a frame (`row_items` for
/// the click map, then the paint) and scanned linearly for the cursor
/// each time; see `frame_budget`.
#[derive(Debug, Clone, Copy)]
struct Layout {
    len: usize,
    /// `pos` once it is known to name an entry, which is the only form
    /// the arithmetic below uses: a position past the end has no "now
    /// playing" row and reads as a queue that has not started.
    playing: Option<usize>,
}

impl Layout {
    fn new(len: usize, pos: Option<usize>) -> Self {
        Layout {
            len,
            playing: pos.filter(|p| *p < len),
        }
    }

    /// Display row the "played" label sits on, i.e. how many rows the
    /// now-playing and up-next runs take: the label and the current
    /// entry, plus the up-next label and its entries when any remain.
    /// Unconditional, so the row is named even when `p` is 0 and there
    /// is no played run; the callers check that.
    fn played_label(&self, p: usize) -> usize {
        match self.len - p - 1 {
            0 => 2,
            upcoming => 3 + upcoming,
        }
    }

    /// Display rows in total: one per entry plus one per section label.
    fn total(&self) -> usize {
        match self.playing {
            None if self.len == 0 => 0,
            None => self.len + 1,
            Some(0) => self.played_label(0),
            Some(p) => self.played_label(p) + 1 + p,
        }
    }

    /// The display row at `at`, or `None` past the end.
    fn get(&self, at: usize) -> Option<Row> {
        if at >= self.total() {
            return None;
        }
        let Some(p) = self.playing else {
            // nothing playing yet: one label, then the whole queue.
            return Some(match at {
                0 => Row::Section("up next"),
                _ => Row::Entry(at - 1),
            });
        };
        match at {
            0 => Some(Row::Section("now playing")),
            1 => Some(Row::Entry(p)),
            _ => {
                let upcoming = self.len - p - 1;
                if upcoming > 0 {
                    if at == 2 {
                        return Some(Row::Section("up next"));
                    }
                    if at < 3 + upcoming {
                        return Some(Row::Entry(p + 1 + (at - 3)));
                    }
                }
                // `total` already refused everything past the played
                // run, so `p` is non-zero here and this label exists.
                let label = self.played_label(p);
                match at == label {
                    true => Some(Row::Section("played")),
                    false => Some(Row::Entry(p - 1 - (at - label - 1))),
                }
            }
        }
    }

    /// Which display row holds `selected`, so the scroll offset can
    /// follow the cursor through the section labels without the labels
    /// ever being what's "selected". A `selected` naming no entry
    /// scrolls to the top, the same answer the position scan this
    /// replaced gave by failing to find it.
    fn row_of(&self, selected: usize) -> usize {
        let Some(p) = self.playing else {
            return match selected < self.len {
                true => selected + 1,
                false => 0,
            };
        };
        if selected >= self.len {
            return 0;
        }
        use std::cmp::Ordering;
        match selected.cmp(&p) {
            Ordering::Equal => 1,
            Ordering::Greater => 3 + (selected - p - 1),
            // `selected < p` makes `p` non-zero, so the played run is
            // there and its label is the row above it.
            Ordering::Less => self.played_label(p) + 1 + (p - 1 - selected),
        }
    }
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
    let rows = Layout::new(entries.len(), pos);
    let offset = scroll::offset_for(rows.row_of(selected), body_height);
    (0..body_height)
        .map(|row| match rows.get(offset + row) {
            Some(Row::Entry(index)) => Some(index),
            Some(Row::Section(_)) | None => None,
        })
        .collect()
}

/// Entry indices in display order, which is what j/k step through;
/// the caller needs this to move the cursor past a section label in
/// one keypress rather than landing on it.
pub fn selectable(entries: &[SearchItem], pos: Option<usize>) -> Vec<usize> {
    let rows = Layout::new(entries.len(), pos);
    let Some(p) = rows.playing else {
        return (0..rows.len).collect();
    };
    let mut out = Vec::with_capacity(rows.len);
    out.push(p);
    out.extend(p + 1..rows.len);
    out.extend((0..p).rev());
    out
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
            let mut fitted = String::new();
            draw_fitted(
                buf,
                area,
                body_top,
                "nothing queued",
                shade(MID),
                &mut fitted,
            );
            let hint = format!(
                "enter on a song to start, {} to add",
                key_label(self.keymap.get(KeyAction::Enqueue))
            );
            let mut out = String::new();
            draw_fitted(
                buf,
                area,
                body_top.saturating_add(1),
                &hint,
                shade(DIM),
                &mut out,
            );
            return;
        }

        let rows = Layout::new(self.entries.len(), self.pos);
        let offset = scroll::offset_for(rows.row_of(self.selected), body_height);
        // the index gutter is sized to the widest number actually on
        // screen, so a 9-track queue doesn't pay for three digits.
        let gutter = digits(self.entries.len()).max(2);
        // one pair of buffers for every row of the frame; both reach
        // the widest row's length on the first screenful and stop
        // reallocating, where a `format!` plus a `truncate` was two
        // allocations per visible row every frame.
        let mut text = String::new();
        let mut fitted = String::new();

        for row_i in 0..body_height {
            let Some(row) = rows.get(offset + row_i) else {
                break;
            };
            let y = body_top + row_i as u16;
            match row {
                Row::Section(name) => {
                    // one blank column of indent so section labels sit
                    // outside the rows they head, not in the same
                    // column as the entry text.
                    text.clear();
                    text.push(' ');
                    text.push_str(name);
                    draw_fitted(buf, area, y, &text, shade(NEAR_BLACK), &mut fitted);
                }
                Row::Entry(index) => {
                    let item = &self.entries[index];
                    let is_current = self.pos == Some(index);
                    let is_selected = index == self.selected;
                    let is_played = self.pos.is_some_and(|p| index < p);

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
                    write_row_text(&mut text, item, index, marker, gutter);
                    draw_fitted(buf, area, y, &text, shade(level), &mut fitted);
                }
            }
        }
    }
}

/// `▌ 12. title · artist` for one row, written into `out` rather than
/// returned: the paint hands the same buffer to every row, where a
/// `format!` per row was an allocation per visible row every frame.
/// Clears `out` itself, so a caller cannot leak the previous row into
/// this one.
fn write_row_text(out: &mut String, item: &SearchItem, index: usize, marker: char, gutter: usize) {
    use std::fmt::Write;
    out.clear();
    out.push(marker);
    // right-aligned by hand: `{:>gutter$}` would do it, but only
    // through a second `format!`.
    let number = index + 1;
    for _ in digits(number)..gutter {
        out.push(' ');
    }
    // writing into a `String` cannot fail.
    let _ = write!(out, "{number}. ");
    out.push_str(&label(item));
}

/// Decimal digits in a positive number, which is the index gutter's
/// width; `to_string().len()` was the same answer and an allocation.
fn digits(n: usize) -> usize {
    n.max(1).ilog10() as usize + 1
}

/// One row of `area`, clipped to its width with an ellipsis where
/// anything was cut. `fitted` is the caller's reused buffer: the
/// ellipsis goes on first (it is what signals "there is more"), then
/// the width-correct writer does the clipping and cell advance.
fn draw_fitted(
    buf: &mut Buffer,
    area: Rect,
    y: u16,
    text: &str,
    color: ratatui::style::Color,
    fitted: &mut String,
) {
    truncate_into(fitted, text, area.width as usize);
    crate::ui::draw::line(buf, area, y, fitted, color);
}

/// What a frame of the queue pane costs, and whether it costs it per
/// *visible row* or per entry in the whole queue. main.rs derives this
/// pane's geometry twice a frame (the click map, then the paint), so
/// anything in here that scales with the queue length is paid for
/// twice at the display's rate.
///
/// A measurement, not an assertion: the numbers move with the box and
/// a ceiling here would be a flake. Run it with
/// `cargo test --release --lib ui::queue::frame_budget -- --ignored --nocapture`;
/// the dev profile is `opt-level = 0` and reads about 10x the shipping
/// cost, which is why the build is the first thing printed.
#[cfg(test)]
mod frame_budget {
    use super::tests::{eager_row_of, eager_row_text, eager_rows, queue};
    use super::*;
    use crate::ui::testbuf::buffer;
    use std::time::Instant;

    /// Alternating batches in one process, best of each. Sequential
    /// runs on this box differ 1.6-2.1x under load, so two separate
    /// measurements are not comparable and this is the only shape that
    /// is; same harness as `results_list::frame_budget`.
    fn best_each(
        rounds: usize,
        iters: usize,
        a: &mut dyn FnMut(),
        b: &mut dyn FnMut(),
    ) -> (f64, f64) {
        let mut best = (f64::MAX, f64::MAX);
        let time = |f: &mut dyn FnMut()| {
            let at = Instant::now();
            for _ in 0..iters {
                f();
            }
            at.elapsed().as_secs_f64() * 1e6 / iters as f64
        };
        for _ in 0..rounds {
            best.0 = best.0.min(time(a));
            best.1 = best.1.min(time(b));
        }
        best
    }

    /// The pane the workspace hands this view, at a tall terminal's
    /// row count.
    const PANE: Rect = Rect {
        x: 0,
        y: 0,
        width: 60,
        height: 40,
    };

    /// One frame's work on this pane: the click map main.rs builds
    /// outside the widget, plus the paint (which lays the geometry out
    /// a second time itself).
    fn frame(entries: &[SearchItem], pos: Option<usize>, selected: usize, buf: &mut Buffer) {
        let _ = row_items(entries, pos, selected, PANE);
        QueueView {
            entries,
            pos,
            selected,
            focused: true,
            elapsed: Duration::ZERO,
            keymap: &Keymap::default(),
        }
        .render(PANE, buf);
    }

    /// One screenful of geometry, which is what each of a frame's two
    /// geometry derivations does: place the cursor, then ask for every
    /// row in the window.
    ///
    /// A function, and `screenful_materialized` below is another one,
    /// because an "old" shape written as a branch inside the timing
    /// loop gets inlined and specialized while the real path stays an
    /// ordinary call, and the A/B then lies.
    fn screenful(entries: &[SearchItem], pos: Option<usize>, selected: usize) {
        let l = Layout::new(entries.len(), pos);
        let offset = scroll::offset_for(l.row_of(selected), PANE.height as usize);
        for row in 0..PANE.height as usize {
            std::hint::black_box(l.get(offset + row));
        }
    }

    /// The same screenful out of the `Vec<Row>` that `Layout`
    /// replaced, i.e. one entry per entry in the whole queue, plus the
    /// linear scan for the cursor that went with it.
    fn screenful_materialized(entries: &[SearchItem], pos: Option<usize>, selected: usize) {
        let seq = eager_rows(entries, pos);
        let offset = scroll::offset_for(eager_row_of(&seq, selected), PANE.height as usize);
        for row in 0..PANE.height as usize {
            std::hint::black_box(seq.get(offset + row));
        }
    }

    #[test]
    #[ignore = "a measurement; run it explicitly"]
    fn what_a_frame_of_the_queue_costs() {
        // 144hz is `anim::MAX_HZ`, the fastest the poll loop will ever
        // redraw at, so it is the tightest deadline this path has.
        let budget = 1e6 / crate::anim::MAX_HZ as f64;
        println!(
            "--- queue pane {}x{}, budget {budget:.0}us at {}hz, {} build",
            PANE.width,
            PANE.height,
            crate::anim::MAX_HZ,
            if cfg!(debug_assertions) {
                "DEBUG (not the shipping cost; re-run with --release)"
            } else {
                "release"
            }
        );
        // the short queue is the control: a pane's worth of entries and
        // nothing more. Any gap between the two is work the frame did
        // on entries that are not on screen.
        let small = queue(PANE.height as usize);
        let mut buf_a = buffer(PANE.width, PANE.height);
        let mut buf_b = buffer(PANE.width, PANE.height);
        for n in [2_000usize, 8_000, 15_000] {
            let long = queue(n);
            // mid-queue, so all three sections are on and the played
            // run is as long as the upcoming one.
            let pos = Some(n / 2);
            for (what, selected) in [("the current track", n / 2), ("the last played", 0usize)] {
                let (big, ctrl) = {
                    let mut on_long = || frame(&long, pos, selected, &mut buf_a);
                    let mut on_small =
                        || frame(&small, Some(PANE.height as usize / 2), 0, &mut buf_b);
                    best_each(5, 20, &mut on_long, &mut on_small)
                };
                let (closed, materialized) =
                    best_each(5, 20, &mut || screenful(&long, pos, selected), &mut || {
                        screenful_materialized(&long, pos, selected)
                    });
                // and the whole frame either way. `frame` already does
                // the two closed-form derivations, so stacking the two
                // materialized ones on top is the old frame plus one
                // closed screenful of noise (0.2us of a 100us frame).
                let (was, now) = best_each(
                    5,
                    20,
                    &mut || {
                        screenful_materialized(&long, pos, selected);
                        screenful_materialized(&long, pos, selected);
                        frame(&long, pos, selected, &mut buf_a);
                    },
                    &mut || frame(&long, pos, selected, &mut buf_b),
                );
                println!(
                    "  {n:>6} entries, cursor on {what:<18}: frame {big:>8.1}us ({:>5.1}% of budget)   {} on screen {ctrl:>7.1}us   {:>5.1}x\n{:30}one screenful of geometry: closed form {closed:>8.2}us   a Vec<Row> {materialized:>8.1}us   {:>6.1}x\n{:30}the whole frame {now:>8.1}us   with both derivations materialized {was:>8.1}us   {:>5.1}x",
                    big / budget * 100.0,
                    PANE.height,
                    big / ctrl,
                    "",
                    materialized / closed,
                    "",
                    was / now,
                );
            }
        }

        // the row text, which the paint builds once per visible row:
        // one reused buffer plus `truncate_into` against the
        // `format!` + `truncate_with_ellipsis` pair it replaced. The
        // label itself still allocates (`results_list::label` returns
        // a `String`), so this is two of the three allocations a row
        // used to cost, not all three.
        let rows = queue(PANE.height as usize);
        let mut text = String::new();
        let mut fitted = String::new();
        let mut reused = || {
            for (i, item) in rows.iter().enumerate() {
                write_row_text(&mut text, item, i, ' ', 2);
                truncate_into(&mut fitted, &text, PANE.width as usize);
                std::hint::black_box(&fitted);
            }
        };
        let mut formatted = || {
            for (i, item) in rows.iter().enumerate() {
                let t = eager_row_text(item, i, ' ', 2);
                std::hint::black_box(crate::ui::textfit::truncate_with_ellipsis(
                    &t,
                    PANE.width as usize,
                ));
            }
        };
        let (one_buffer, per_row) = best_each(5, 200, &mut reused, &mut formatted);
        println!(
            "  {} rows of row text: two buffers {one_buffer:>7.2}us   a String per row {per_row:>7.2}us   {:.1}x",
            PANE.height,
            per_row / one_buffer,
        );

        // and the whole paint, which is what that sits inside: the
        // geometry is a rounding error at a pane's worth of entries,
        // so this is where the rest of a frame goes.
        let mut pane = buffer(PANE.width, PANE.height);
        let mut paint = || {
            QueueView {
                entries: &rows,
                pos: Some(0),
                selected: 0,
                focused: true,
                elapsed: Duration::ZERO,
                keymap: &Keymap::default(),
            }
            .render(PANE, &mut pane);
        };
        let mut geometry = || {
            std::hint::black_box(row_items(&rows, Some(0), 0, PANE));
        };
        let (painted, geo) = best_each(5, 200, &mut paint, &mut geometry);
        println!(
            "  {} rows painted {painted:>7.2}us ({:>4.1}% of budget)   the click map alone {geo:>7.2}us",
            PANE.height,
            painted / budget * 100.0,
        );
        // the floor, so this is a test and not just a printer: a
        // render that drew nothing would measure beautifully.
        let grid = crate::ui::testbuf::grid(&pane);
        assert!(grid.contains("▌ 1. t0"), "nothing painted:\n{grid}");
        assert!(
            grid.lines().last().is_some_and(|r| r.contains(". t")),
            "the pane is not full to its last row:\n{grid}"
        );
    }
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

    /// `n` entries with an artist each, so every row's label is the
    /// two-part form the pane actually shows. Shared with
    /// `frame_budget`.
    pub(super) fn queue(n: usize) -> Vec<SearchItem> {
        (0..n).map(|i| song(&format!("t{i}"))).collect()
    }

    /// What the row text used to cost: a `String` for the label and a
    /// `format!` for the row, per visible row per frame. Kept as the
    /// definition `write_row_text` has to agree with, and as the
    /// benchmark's other half.
    pub(super) fn eager_row_text(
        item: &SearchItem,
        index: usize,
        marker: char,
        gutter: usize,
    ) -> String {
        format!(
            "{marker}{:>gutter$}. {}",
            index + 1,
            label(item),
            gutter = gutter
        )
    }

    /// The one-buffer row text has to spell the same row as the
    /// `format!` it replaced, for every item variant and at every
    /// gutter width the index can cross: `digits` right-aligns by hand
    /// where `{:>gutter$}` did it, so a number wider than the gutter
    /// (which a queue shortened under the cursor produces) has to
    /// overflow the column the same way rather than be clipped.
    #[test]
    fn the_row_text_says_what_a_formatted_one_said() {
        let client = crate::browse::fixtures::test_client();
        let items = [
            song("a title"),
            SearchItem::LocalMusic {
                path: "/m/b.flac".into(),
                title: "no artist".into(),
                artist: None,
                album: None,
            },
            SearchItem::LocalVideo {
                path: "/v/c.mkv".into(),
                title: "a clip".into(),
            },
            SearchItem::Silence { secs: 930 },
            SearchItem::Song {
                id: "3".into(),
                title: "a song".into(),
                artist: "an artist".into(),
                album: "an album".into(),
                client,
            },
        ];
        let mut out = String::new();
        for item in &items {
            for index in [0usize, 8, 9, 98, 99, 100, 998] {
                for gutter in [1usize, 2, 3, 4] {
                    for marker in [' ', '▌'] {
                        // the same buffer across every case, so a
                        // missing `clear` shows up as the previous
                        // row's text still being in front.
                        write_row_text(&mut out, item, index, marker, gutter);
                        assert_eq!(
                            out,
                            eager_row_text(item, index, marker, gutter),
                            "{item:?} index {index} gutter {gutter} marker {marker:?}"
                        );
                    }
                }
            }
        }
    }

    /// The gutter is sized off this, where a `to_string().len()` did
    /// it before; an empty queue never reaches the render, but the
    /// helper still has to answer rather than panic on 0.
    #[test]
    fn the_gutter_width_is_the_decimal_length_of_the_count() {
        for n in [0usize, 1, 9, 10, 99, 100, 101, 999, 1000, 15_000] {
            assert_eq!(digits(n), n.max(1).to_string().len(), "{n}");
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

    /// What `Layout` used to be: the display-row sequence as a `Vec`
    /// the length of the whole queue, plus a linear scan for the
    /// selected row. Kept as the definition the closed form has to
    /// agree with at every index; `frame_budget` next door times the
    /// two against each other, which is the only reason the old shape
    /// is worth keeping around.
    pub(super) fn eager_rows(entries: &[SearchItem], pos: Option<usize>) -> Vec<Row> {
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

    /// The same scan `eager_rows` paired with, so the two halves of
    /// the old geometry are both here to compare against.
    pub(super) fn eager_row_of(rows: &[Row], selected: usize) -> usize {
        rows.iter()
            .position(|r| matches!(r, Row::Entry(i) if *i == selected))
            .unwrap_or(0)
    }

    /// The live layout materialized, so the shape tests below read the
    /// production geometry rather than a second copy of it.
    fn sequence(entries: &[SearchItem], pos: Option<usize>) -> Vec<Row> {
        let l = Layout::new(entries.len(), pos);
        (0..l.total()).map(|at| l.get(at).unwrap()).collect()
    }

    /// `Layout` answers one display row at a time out of the queue's
    /// length and play position instead of out of a `Vec` as long as
    /// the queue. It has to give the same answer at every index, the
    /// same total, and the same cursor row, including at a `selected`
    /// that names no entry at all (which scrolls to the top) and a
    /// `pos` past the end of the queue.
    #[test]
    fn the_row_geometry_answers_what_a_materialized_one_would() {
        for n in [0usize, 1, 2, 3, 7, 40, 137] {
            let e: Vec<_> = (0..n).map(|i| song(&format!("t{i}"))).collect();
            let positions = [
                None,
                Some(0),
                Some(1),
                Some(n / 2),
                Some(n.saturating_sub(1)),
                Some(n),
                Some(n + 5),
            ];
            for pos in positions {
                let eager = eager_rows(&e, pos);
                let lazy = Layout::new(n, pos);
                let at = format!("n {n}, pos {pos:?}");
                assert_eq!(lazy.total(), eager.len(), "total: {at}");
                // three past the end, so "past the end" is pinned as
                // `None` on both sides rather than left unasked.
                for row in 0..eager.len() + 3 {
                    assert_eq!(lazy.get(row), eager.get(row).copied(), "row {row} of {at}");
                }
                for selected in [0usize, 1, n / 2, n.saturating_sub(1), n, n + 9] {
                    assert_eq!(
                        lazy.row_of(selected),
                        eager_row_of(&eager, selected),
                        "cursor row for selected {selected} of {at}"
                    );
                }
            }
        }
    }

    /// And the one thing the row-by-row agreement does not cover: the
    /// order j/k steps through, which is built straight from the
    /// layout rather than read out of it.
    #[test]
    fn selectable_is_the_entries_of_the_materialized_sequence() {
        for n in [0usize, 1, 2, 7, 40] {
            let e: Vec<_> = (0..n).map(|i| song(&format!("t{i}"))).collect();
            for pos in [None, Some(0), Some(1), Some(n / 2), Some(n), Some(n + 5)] {
                let want: Vec<usize> = eager_rows(&e, pos)
                    .into_iter()
                    .filter_map(|r| match r {
                        Row::Entry(i) => Some(i),
                        Row::Section(_) => None,
                    })
                    .collect();
                assert_eq!(selectable(&e, pos), want, "n {n}, pos {pos:?}");
            }
        }
    }

    #[test]
    fn sections_run_current_then_upcoming_then_played_backwards() {
        let e: Vec<_> = ["a", "b", "c", "d"].iter().map(|t| song(t)).collect();
        let r = sequence(&e, Some(1));
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
        let r = sequence(&e, None);
        assert_eq!(r.len(), 3, "one label plus two entries: {r:?}");
        assert_eq!(r[0], Row::Section("up next"));
    }

    #[test]
    fn the_last_track_playing_has_no_up_next_section() {
        let e: Vec<_> = ["a", "b"].iter().map(|t| song(t)).collect();
        let r = sequence(&e, Some(1));
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
