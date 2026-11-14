use std::borrow::Cow;
use std::collections::HashSet;
use std::time::Duration;

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::widgets::Widget;

use crate::anim;
use crate::favorites::Favorites;
use crate::search::{group_boundaries, Category, Corpus, Ranking, SearchItem};
use crate::theme::{shade, BRIGHT, DIM, NEAR_BLACK};
use crate::ui::progress::fmt_span;
use crate::ui::textfit::truncate_with_ellipsis;

/// A results list as this widget reads it: how many rows, where the
/// category runs begin, and one row at a time.
///
/// Row *geometry* needs only the first two, never a row's content,
/// which is what lets a 15k-match list be laid out without
/// materializing a single `SearchItem` (see the result-clone item in
/// ROADMAP.md). `Ranked` is how `App` holds its results; a plain
/// slice is still a source, for lists that are already owned.
pub trait Rows {
    fn count(&self) -> usize;

    /// `(index, category)` per category change, as
    /// `search::group_boundaries` defines them.
    fn boundaries(&self) -> Vec<(usize, Category)>;

    /// One row, or `None` past the end.
    fn row(&self, at: usize) -> Option<&SearchItem>;
}

impl Rows for [SearchItem] {
    fn count(&self) -> usize {
        self.len()
    }

    fn boundaries(&self) -> Vec<(usize, Category)> {
        group_boundaries(self)
    }

    fn row(&self, at: usize) -> Option<&SearchItem> {
        self.get(at)
    }
}

/// The owned form too, because `&Vec<SearchItem>` reaching a
/// `&dyn Rows` parameter would be a deref *and* an unsize coercion in
/// one step, which the compiler will not do.
impl Rows for Vec<SearchItem> {
    fn count(&self) -> usize {
        self.len()
    }

    fn boundaries(&self) -> Vec<(usize, Category)> {
        group_boundaries(self)
    }

    fn row(&self, at: usize) -> Option<&SearchItem> {
        self.get(at)
    }
}

/// A ranked order plus the corpus it addresses, which is how `App`
/// holds its results: rows resolve one at a time instead of arriving
/// as a cloned list.
///
/// A ranking that no longer addresses this corpus reports **no rows
/// at all**, matching `Ranking::row`: an order is indices into one
/// corpus state, so reading it against another resolves to
/// real-looking wrong rows, which plays the wrong song. An empty list
/// is the safe answer, and `App::replace_corpus` is what keeps it
/// from being the answer in practice.
pub struct Ranked<'a> {
    pub ranking: &'a Ranking,
    pub corpus: &'a Corpus,
}

impl Rows for Ranked<'_> {
    fn count(&self) -> usize {
        if self.ranking.addresses(self.corpus) {
            self.ranking.len()
        } else {
            0
        }
    }

    fn boundaries(&self) -> Vec<(usize, Category)> {
        self.ranking.group_boundaries(self.corpus)
    }

    fn row(&self, at: usize) -> Option<&SearchItem> {
        self.ranking.row(self.corpus, at)
    }
}

/// One rendered row: either a section header or an actual result,
/// carrying its index into the row source so the caller can map back
/// to it (`self.selected` stays a plain row index; headers never get
/// one).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DisplayRow {
    Header(Category),
    Item(usize),
}

/// Where every display row of a list lands, as an answer per row
/// rather than as a sequence.
///
/// The whole sequence is a function of the category boundaries alone:
/// header `k` sits at display row `boundary[k] + k`, strictly
/// increasing, so one binary search over the headers places any row.
/// `search::rank_order` groups the order by category, which caps the
/// header count at `Category`'s variants however long the list gets,
/// so this is a handful of entries where the sequence was one per
/// match. main.rs asks for this geometry three times a frame, and a
/// `Vec` the length of the match set cost 28% of a 144hz frame at 15k
/// matches (see `frame_budget`).
pub struct RowLayout {
    /// `(display row, category)` per section header, ascending.
    headers: Vec<(usize, Category)>,
    count: usize,
    /// First display row of the pane, keeping `selected` on screen.
    pub scroll_offset: usize,
}

impl RowLayout {
    /// Display rows in total: one per row plus one per header.
    fn len(&self) -> usize {
        self.count + self.headers.len()
    }

    /// The display row at `at`, or `None` past the end.
    pub fn get(&self, at: usize) -> Option<DisplayRow> {
        if at >= self.len() {
            return None;
        }
        // headers at or before `at`: the last of them *is* `at` when
        // `at` is a header row, and otherwise counts how far the
        // headers above have pushed this row past its row index.
        let before = self.headers.partition_point(|&(row, _)| row <= at);
        match before.checked_sub(1).and_then(|k| self.headers.get(k)) {
            Some(&(row, cat)) if row == at => Some(DisplayRow::Header(cat)),
            _ => Some(DisplayRow::Item(at - before)),
        }
    }
}

/// Lays `rows` (already grouped into category runs by the ranking)
/// out as display rows with a header before each run, plus the scroll
/// offset keeping `selected` visible within `visible_rows`. The one
/// place both this widget's own render and main.rs's sidebar-divider
/// header-junction marks compute this, so they can't disagree about
/// where a header actually lands.
///
/// Reads nothing but the count and the boundaries, which is why it
/// takes `&dyn Rows` rather than a list of rows.
pub fn layout(rows: &dyn Rows, selected: usize, visible_rows: usize) -> RowLayout {
    let count = rows.count();
    let boundaries = rows.boundaries();
    let mut headers = Vec::with_capacity(boundaries.len());
    // a row's display position is its index plus the headers above it,
    // which is also what `selected`'s own position needs. a boundary
    // at or past `count` names no row, so it gets no header.
    let mut selected_row = selected;
    for (k, (idx, cat)) in boundaries
        .into_iter()
        .take_while(|&(idx, _)| idx < count)
        .enumerate()
    {
        headers.push((idx + k, cat));
        if idx <= selected {
            selected_row += 1;
        }
    }
    // `selected` naming no row scrolls to the top, same answer the
    // position scan this replaced gave by failing to find it.
    if selected >= count {
        selected_row = 0;
    }
    RowLayout {
        headers,
        count,
        scroll_offset: crate::ui::scroll::offset_for(selected_row, visible_rows),
    }
}

/// Absolute rows of `area` on which `render` draws a section-header
/// rule, which is what the divider immediately right of this list
/// needs to join them (see `ui::junctions`); derived from the same
/// `layout` call the render itself uses, so the two can't disagree
/// about where a header actually lands.
pub fn header_rows(items: &dyn Rows, selected: usize, area: Rect) -> Vec<u16> {
    if items.count() == 0 || area.height == 0 {
        return Vec::new();
    }
    let visible_rows = area.height as usize;
    let rows = layout(items, selected, visible_rows);
    (0..visible_rows)
        .filter(|row| {
            matches!(
                rows.get(rows.scroll_offset + row),
                Some(DisplayRow::Header(_))
            )
        })
        .map(|row| area.y + row as u16)
        .collect()
}

/// Item index per visible row of `area`, `None` where `render` drew a
/// section header or ran out of list. Same `layout` call the render
/// itself uses, so a click resolves to the row the user saw rather
/// than to a second guess at where it landed (see `hit::List`).
pub fn row_items(items: &dyn Rows, selected: usize, area: Rect) -> Vec<Option<usize>> {
    if items.count() == 0 || area.height == 0 {
        return Vec::new();
    }
    let visible_rows = area.height as usize;
    let rows = layout(items, selected, visible_rows);
    (0..visible_rows)
        .map(|row| match rows.get(rows.scroll_offset + row) {
            Some(DisplayRow::Item(idx)) => Some(idx),
            _ => None,
        })
        .collect()
}

/// Plain ASCII, not unicode pictographs: even nominally "geometric,
/// not emoji" symbols (e.g. the bullseye tried earlier for albums)
/// can still be flagged emoji-presentation in some fonts/terminals, or
/// render at ambiguous width and throw off alignment. A letter in
/// brackets can't do either.
pub(crate) fn kind_code(item: &SearchItem) -> char {
    match item {
        SearchItem::LocalMusic { .. } | SearchItem::Song { .. } => 's',
        SearchItem::TorBoxFile { video: false, .. } => 's',
        SearchItem::LocalVideo { .. } => 'v',
        SearchItem::TorBoxFile { video: true, .. } => 'v',
        SearchItem::WatchSource { .. } => 'v',
        SearchItem::Artist { .. } => 'a',
        SearchItem::Album { .. } => 'l',
        SearchItem::Playlist { .. } => 'p',
        // 'z' for a silence: alphabetic like the rest, and the one
        // letter nobody reads as a media kind.
        SearchItem::Silence { .. } => 'z',
    }
}

/// 'n' for navidrome (remote), 'f' for file (local), 'g' for generated
/// (a silence, which came from neither).
pub(crate) fn source_code(item: &SearchItem) -> char {
    match item {
        SearchItem::Silence { .. } => 'g',
        SearchItem::LocalMusic { .. } | SearchItem::LocalVideo { .. } => 'f',
        SearchItem::Artist { .. }
        | SearchItem::Album { .. }
        | SearchItem::Song { .. }
        | SearchItem::Playlist { .. } => 'n',
        SearchItem::TorBoxFile { .. } => 't',
        SearchItem::WatchSource { .. } => 'w',
    }
}

pub fn label(item: &SearchItem) -> String {
    // "·" (middle dot), not an em dash, for title/artist metadata
    // separators; same convention most music apps use.
    match item {
        SearchItem::LocalMusic { title, artist, .. } => match artist {
            Some(a) => format!("{title} · {a}"),
            None => title.clone(),
        },
        SearchItem::LocalVideo { title, .. } => title.clone(),
        SearchItem::Artist { name, .. } => name.clone(),
        SearchItem::Album { name, artist, .. } => format!("{name} · {artist}"),
        SearchItem::Song {
            title,
            artist,
            album,
            ..
        } => format!("{title} · {artist} ({album})"),
        SearchItem::Playlist { name, .. } => name.clone(),
        SearchItem::Silence { secs } => format!("silence · {}", fmt_span(*secs)),
        SearchItem::TorBoxFile { title, .. } => title.clone(),
        SearchItem::WatchSource { title, .. } => title.clone(),
    }
}

/// Same as `label`, but the artist/album metadata only shows on the
/// hovered (selected) row; everywhere else just the bare title, so a
/// long results list doesn't read as a wall of repeated artist names.
///
/// Borrowed for every row but the hovered one, which is the row the
/// paint wants: a clone per row was one allocation and one title copy
/// per visible row, every frame.
fn row_label(item: &SearchItem, hovered: bool) -> Cow<'_, str> {
    if hovered {
        return Cow::Owned(label(item));
    }
    match item {
        SearchItem::LocalMusic { title, .. } => Cow::Borrowed(title),
        SearchItem::LocalVideo { title, .. } => Cow::Borrowed(title),
        SearchItem::Artist { name, .. } => Cow::Borrowed(name),
        SearchItem::Album { name, .. } => Cow::Borrowed(name),
        SearchItem::Song { title, .. } => Cow::Borrowed(title),
        SearchItem::Playlist { name, .. } => Cow::Borrowed(name),
        // the one row that keeps its metadata unhovered. for every
        // other item the title is the identity and the artist is
        // decoration; for a silence the *duration* is the identity,
        // and a column of bare "silence" rows would be unreadable.
        SearchItem::Silence { .. } => Cow::Owned(label(item)),
        SearchItem::TorBoxFile { title, .. } => Cow::Borrowed(title),
        SearchItem::WatchSource { title, .. } => Cow::Borrowed(title),
    }
}

/// `[sk] title` for one row, written into `out` rather than returned:
/// the paint hands the same buffer to every row, where a `format!`
/// per row was an allocation per visible row every frame. Clears
/// `out` itself, so a caller cannot leak the previous row into this
/// one.
fn write_row_text(out: &mut String, item: &SearchItem, hovered: bool, starred: bool) {
    out.clear();
    out.push_str(if hovered { "> " } else { "  " });
    out.push('[');
    out.push(source_code(item));
    out.push(kind_code(item));
    out.push_str("] ");
    if starred {
        out.push_str("* ");
    }
    out.push_str(&row_label(item, hovered));
}

/// Only tracks are starrable (see `main.rs::is_starred`); this is a
/// display-only mirror of that same check, since the widget only gets
/// the raw favorites/remote-starred state, not the logic that owns it.
fn is_starred(item: &SearchItem, favorites: &Favorites, remote_starred: &HashSet<String>) -> bool {
    match item {
        SearchItem::LocalMusic { path, .. } | SearchItem::LocalVideo { path, .. } => {
            favorites.is_starred(&path.to_string_lossy())
        }
        SearchItem::Song { id, .. } => remote_starred.contains(id),
        SearchItem::Silence { .. }
        | SearchItem::Artist { .. }
        | SearchItem::Album { .. }
        | SearchItem::Playlist { .. } => false,
        SearchItem::TorBoxFile { .. } => false,
        SearchItem::WatchSource { .. } => false,
    }
}

/// Custom-painted results list: no stock `List` widget. Each row is
/// `[kind] label`, the selected row rendered near-white, others dim, and
/// the window scrolls to keep the selection visible without wrapping.
/// An empty `items` isn't a no-op render; a blank screen with no
/// explanation is a dead end, not a loved-app experience; `empty_message`
/// is shown instead so there's always *something* telling the user why
/// nothing's there and what to do about it.
///
/// `items` is a row *source*, not a list: it is asked for the rows on
/// screen and for nothing else, so the ranked order `App` holds never
/// has to be materialized to draw a screenful of it.
pub struct ResultsList<'a> {
    pub items: &'a dyn Rows,
    pub selected: usize,
    pub empty_message: &'a str,
    pub favorites: &'a Favorites,
    pub remote_starred: &'a HashSet<String>,
    /// Whether the sidebar itself currently holds focus: the selected
    /// row's background only breathes in the focused pane, and sits at
    /// a flat dimmer level otherwise (see `anim::selection_bg`).
    pub focused: bool,
    /// Time since app start; the one input to every animation, so the
    /// pulse runs at the same speed whatever the display's rate is.
    pub elapsed: Duration,
}

impl Widget for ResultsList<'_> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        if area.height == 0 {
            return;
        }

        if self.items.count() == 0 {
            let text = truncate_with_ellipsis(self.empty_message, area.width as usize);
            crate::ui::draw::line(buf, area, area.y, &text, shade(DIM));
            return;
        }

        let visible_rows = area.height as usize;
        let rows = layout(self.items, self.selected, visible_rows);
        // one buffer for every row of the frame; it reaches the widest
        // row's length on the first screenful and stops reallocating.
        let mut text = String::new();

        for row in 0..visible_rows {
            let Some(display_row) = rows.get(rows.scroll_offset + row) else {
                break;
            };
            let y = area.y + row as u16;

            match display_row {
                DisplayRow::Header(cat) => draw_header_row(buf, area, y, cat.label()),
                DisplayRow::Item(idx) => {
                    // `count()` covered this index a moment ago, so a
                    // miss is a corpus that moved mid-render; draw
                    // nothing rather than guess at a row.
                    let Some(item) = self.items.row(idx) else {
                        continue;
                    };
                    let is_selected = idx == self.selected;

                    // the selection reads off the row's background,
                    // not its borders; the marker and brightness bump
                    // below stay on top of it.
                    if is_selected {
                        fill_row_bg(buf, area, y, anim::selection_bg(self.elapsed, self.focused));
                    }

                    let fg = if is_selected {
                        shade(BRIGHT)
                    } else {
                        shade(DIM)
                    };
                    write_row_text(
                        &mut text,
                        item,
                        is_selected,
                        is_starred(item, self.favorites, self.remote_starred),
                    );
                    let fitted = truncate_with_ellipsis(&text, area.width as usize);

                    crate::ui::draw::line(buf, area, y, &fitted, fg);
                }
            }
        }
    }
}

/// Whether `area` actually owns row `y`. Both painters below write raw
/// cells, which `Buffer` bounds-checks and the rect does not: an
/// unguarded row lands in whatever pane sits past this one, usually the
/// bottom bar's rule. `draw::line`/`centered` carry the same check.
fn owns_row(area: Rect, y: u16) -> bool {
    y >= area.y && y < area.y + area.height
}

/// `▴`/`▾` in the last column of a list showing a window onto a longer
/// one; without it a scrolled list reads as the whole list. Same
/// glyphs `lyrics_view` puts in its gutter. `list` is the rows that
/// were drawn, which callers narrow by this one column first so the
/// cue never lands on text.
pub(crate) fn scroll_cues(buf: &mut Buffer, list: Rect, offset: usize, total: usize) {
    let shown = (total.saturating_sub(offset) as u16).min(list.height);
    if list.width == 0 || shown == 0 {
        return;
    }
    let x = list.x + list.width - 1;
    if offset > 0 {
        crate::ui::draw::put(buf, x, list.y, '▴', shade(DIM));
    }
    if offset + (shown as usize) < total {
        crate::ui::draw::put(buf, x, list.y + shown - 1, '▾', shade(DIM));
    }
}

/// Paints one row of `area` a flat background level across its whole
/// width, so a selection highlight is a full bar rather than ending
/// wherever that row's label happens to stop. Shared with
/// `ui::browse`'s lists, same as `draw_header_row` below.
pub(crate) fn fill_row_bg(buf: &mut Buffer, area: Rect, y: u16, level: f32) {
    if !owns_row(area, y) {
        return;
    }
    let bg = shade(level);
    for col in 0..area.width {
        if let Some(cell) = buf.cell_mut((area.x + col, y)) {
            cell.set_bg(bg);
        }
    }
}

/// A full-width dim rule with the section name centered, rather than
/// a plain text line: it's what actually reads as "a divider between
/// groups" instead of just another row, and fills the whole width so
/// it visually meets the sidebar's own vertical divider immediately
/// to its right (see main.rs's matching junction mark at this same
/// row).
pub(crate) fn draw_header_row(buf: &mut Buffer, area: Rect, y: u16, label: &str) {
    // `ui::browse` hands this a height-0 label rect on a squeezed
    // pane, and the height-1 row below would have smuggled it past
    // `draw::centered`'s own guard onto the bottom bar's rule.
    if !owns_row(area, y) {
        return;
    }
    let rule_color = shade(NEAR_BLACK);
    for col in 0..area.width {
        if let Some(cell) = buf.cell_mut((area.x + col, y)) {
            cell.set_char('─');
            cell.set_fg(rule_color);
        }
    }
    let text = format!(" {label} ");
    let row = Rect {
        y,
        height: 1,
        ..area
    };
    crate::ui::draw::centered(buf, row, y, &text, shade(DIM));
}

/// What a frame of this list costs, and whether it costs it per
/// *visible row* or per row in the whole result set. main.rs asks for
/// the row geometry three times a frame (the sidebar divider's
/// junction marks, then the click map, then the paint), so anything in
/// here that scales with the match count is paid for three times at
/// the display's rate.
///
/// A measurement, not an assertion: the numbers move with the box and
/// a ceiling here would be a flake. Run it with
/// `cargo test --release --lib ui::results_list::frame_budget -- --ignored --nocapture`;
/// the dev profile is `opt-level = 0` and reads about 10x the shipping
/// cost, which is why the build is the first thing printed.
#[cfg(test)]
mod frame_budget {
    use super::tests::{ranked, Walked};
    use super::*;
    use crate::ui::testbuf::buffer;
    use std::time::Instant;

    /// Alternating batches in one process, best of each. Sequential
    /// runs on this box differ 1.6-1.8x under load, so two separate
    /// measurements are not comparable and this is the only shape that
    /// is; same alternating-batch harness used by the raster benchmarks.
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

    /// A sidebar's worth: the real pane is about this wide and this is
    /// a tall terminal's row count.
    const PANE: Rect = Rect {
        x: 0,
        y: 0,
        width: 28,
        height: 40,
    };

    /// One frame's work on this list: the two geometry calls main.rs
    /// makes outside the widget, plus the paint (which lays the
    /// geometry out a third time itself).
    fn frame(rows: &dyn Rows, selected: usize, buf: &mut Buffer) {
        let _ = header_rows(rows, selected, PANE);
        let _ = row_items(rows, selected, PANE);
        ResultsList {
            items: rows,
            selected,
            empty_message: "",
            favorites: &Favorites::default(),
            remote_starred: &HashSet::new(),
            focused: true,
            elapsed: Duration::ZERO,
        }
        .render(PANE, buf);
    }

    /// One screenful of geometry, which is what each of a frame's
    /// three geometry calls does: lay the window out, then ask for
    /// every row in it.
    ///
    /// A function, and `screenful_materialized` below is another one,
    /// because an "old" shape written as a branch inside the timing
    /// loop gets inlined and specialized while the real path stays an
    /// ordinary call, and the A/B then lies (see the bottom bar's
    /// bench, where that cost a day).
    fn screenful(rows: &dyn Rows, selected: usize) {
        let l = layout(rows, selected, PANE.height as usize);
        for row in 0..PANE.height as usize {
            std::hint::black_box(l.get(l.scroll_offset + row));
        }
    }

    /// The same screenful out of the display-row `Vec` that
    /// `RowLayout` replaced, i.e. one entry per row in the whole match
    /// set.
    fn screenful_materialized(rows: &dyn Rows, selected: usize) {
        let (seq, offset) = super::tests::eager_layout(rows, selected, PANE.height as usize);
        for row in 0..PANE.height as usize {
            std::hint::black_box(seq.get(offset + row));
        }
    }

    #[test]
    #[ignore = "a measurement; run it explicitly"]
    fn what_a_frame_of_the_results_list_costs() {
        // 144hz is `anim::MAX_HZ`, the fastest the poll loop will ever
        // redraw at, so it is the tightest deadline this path has.
        let budget = 1e6 / crate::anim::MAX_HZ as f64;
        println!(
            "--- results list, {} rows visible, budget {budget:.0}us at {}hz, {} build",
            PANE.height,
            crate::anim::MAX_HZ,
            if cfg!(debug_assertions) {
                "DEBUG (not the shipping cost; re-run with --release)"
            } else {
                "release"
            }
        );
        // the short list is the control: a pane's worth of rows and
        // nothing more. Any gap between the two is work the frame did
        // on rows that are not on screen.
        let (small_corpus, small_ranking) = ranked(PANE.height as usize);
        let small = Ranked {
            ranking: &small_ranking,
            corpus: &small_corpus,
        };
        // printed, because the weak-match cutoff can prune a ranking
        // and a control that turned out to hold three rows would make
        // every ratio below it look better than it is.
        println!(
            "    control: {} tracks -> {} matches, {} category runs",
            PANE.height,
            small.count(),
            small.boundaries().len()
        );
        for n in [2_000usize, 8_000, 15_000] {
            let (corpus, ranking) = ranked(n);
            let long = Ranked {
                ranking: &ranking,
                corpus: &corpus,
            };
            let count = long.count();
            let runs = long.boundaries().len();
            let mut buf_a = buffer(PANE.width, PANE.height);
            let mut buf_b = buffer(PANE.width, PANE.height);
            {
                let mut on_long = || frame(&long, 0, &mut buf_a);
                let mut on_small = || frame(&small, 0, &mut buf_b);
                let (big, ctrl) = best_each(5, 20, &mut on_long, &mut on_small);
                println!(
                    "  {n:>6} tracks -> {count:>6} matches in {runs} category runs: frame {big:>8.1}us ({:>5.1}% of budget)   {} on screen {ctrl:>7.1}us   {:.1}x",
                    big / budget * 100.0,
                    PANE.height,
                    big / ctrl,
                );
            }
            // the same frame off the runs `rank_order` recorded and
            // off a walk of the order, which is the only thing that
            // differs between the two (`Walked` is the real path with
            // the old boundary source). Both selections, since the walk
            // the recorded runs replace is the whole match set either
            // way; three geometry calls a frame pay the gap three
            // times.
            let walked = Walked(Ranked {
                ranking: &ranking,
                corpus: &corpus,
            });
            for (label, selected) in [("row 0", 0usize), ("last row", count - 1)] {
                let (stored, walk) = {
                    let mut a = || frame(&long, selected, &mut buf_a);
                    let mut b = || frame(&walked, selected, &mut buf_b);
                    best_each(5, 20, &mut a, &mut b)
                };
                let (geo_stored, geo_walk) =
                    best_each(5, 20, &mut || screenful(&long, selected), &mut || {
                        screenful(&walked, selected)
                    });
                let (_, materialized) =
                    best_each(5, 20, &mut || screenful(&walked, selected), &mut || {
                        screenful_materialized(&walked, selected)
                    });
                println!(
                    "            selection on the {label:<8}: frame {stored:>8.1}us vs {walk:>8.1}us walked ({:.1}x)   one screenful of geometry: recorded runs {geo_stored:>8.1}us   walked {geo_walk:>8.1}us   materialized {materialized:>8.1}us",
                    walk / stored,
                );
            }
        }

        // Measured and NOT taken, kept so it is not retried: a
        // `Rows::boundaries_upto(limit)` so `layout` walks only as far
        // as its own window. Against the walked source that was 17.8x
        // on a 15k frame with the selection on row 0 and 1.0x with it
        // on the last row, whose headers have to be counted either
        // way. The recorded runs win at both ends, and they cost no
        // narrowing of `RowLayout`, which answers only inside its
        // window once the walk stops there.

        // the row text, which the paint builds once per visible row:
        // one reused buffer and a borrowed label against the `String`
        // + `format!` pair it replaced. Same rows either way, and the
        // hovered one is in there, since that is the row that still
        // has to own its label.
        let rows = super::tests::with_artists(PANE.height as usize);
        let mut buf = String::new();
        let mut borrowed = || {
            for (i, item) in rows.iter().enumerate() {
                write_row_text(&mut buf, item, i == 0, false);
                std::hint::black_box(&buf);
            }
        };
        let mut formatted = || {
            for (i, item) in rows.iter().enumerate() {
                std::hint::black_box(super::tests::eager_row_text(item, i == 0, false));
            }
        };
        let (reused, fresh) = best_each(5, 200, &mut borrowed, &mut formatted);
        println!(
            "  {} rows of row text: one buffer {reused:>7.2}us   a String per row {fresh:>7.2}us   {:.1}x",
            PANE.height,
            fresh / reused,
        );

        // Measured and NOT taken, kept so it is not retried: drawing
        // a row that already fits straight out of its buffer, instead
        // of through `truncate_with_ellipsis`' unconditional copy.
        // 1.8x where rows fit, but 0.8x where they do not (one extra
        // width walk), and ~1us either way against a 6944us budget.
        // A `truncate_into(&mut String, ..)` in `ui::textfit` would
        // win both cases; a branch here wins one and loses the other.
        let texts: Vec<String> = rows
            .iter()
            .enumerate()
            .map(|(i, item)| {
                let mut s = String::new();
                write_row_text(&mut s, item, i == 0, false);
                s
            })
            .collect();
        for (case, w) in [("fits", PANE.width as usize), ("too long", 10)] {
            let mut borrowed = || {
                for t in &texts {
                    if crate::ui::textfit::width(t) <= w {
                        std::hint::black_box(t.as_str());
                    } else {
                        std::hint::black_box(truncate_with_ellipsis(t, w));
                    }
                }
            };
            let mut copied = || {
                for t in &texts {
                    std::hint::black_box(truncate_with_ellipsis(t, w));
                }
            };
            let (keep, copy) = best_each(5, 200, &mut borrowed, &mut copied);
            println!(
                "  {} rows, {case} in {w} cols: borrow when it fits {keep:>7.2}us   always a String {copy:>7.2}us   {:.2}x",
                PANE.height,
                copy / keep,
            );
        }

        // and the whole paint, which is what that sits inside: the
        // geometry is a rounding error at a pane's worth of rows, so
        // this is where the rest of a frame goes.
        let mut pane = buffer(PANE.width, PANE.height);
        let mut paint = || {
            ResultsList {
                items: &rows,
                selected: 0,
                empty_message: "",
                favorites: &Favorites::default(),
                remote_starred: &HashSet::new(),
                focused: true,
                elapsed: Duration::ZERO,
            }
            .render(PANE, &mut pane);
        };
        let mut geometry = || {
            std::hint::black_box(row_items(&rows, 0, PANE));
        };
        let (painted, geo) = best_each(5, 200, &mut paint, &mut geometry);
        println!(
            "  {} rows painted {painted:>7.2}us ({:>4.1}% of budget)   geometry alone {geo:>7.2}us",
            PANE.height,
            painted / budget * 100.0,
        );
        // the floor, so this is a test and not just a printer: a
        // render that drew nothing would measure beautifully. The
        // last row rather than a named index, because the header
        // `layout` inserts pushes the 40th item off a 40-row pane.
        let grid = crate::ui::testbuf::grid(&pane);
        assert!(grid.contains("> [fs] track 0"), "nothing painted:\n{grid}");
        assert!(
            grid.lines()
                .last()
                .is_some_and(|r| r.contains("[fs] track")),
            "the pane is not full to its last row:\n{grid}"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::testbuf::{assert_outside_untouched, buffer};
    use ratatui::style::Color;

    /// `pub(super)` for the `eyeball` module next door, which prints
    /// the same rows this one asserts about.
    pub(super) fn items(n: usize) -> Vec<SearchItem> {
        (0..n)
            .map(|i| SearchItem::LocalMusic {
                path: format!("/m/{i}.flac").into(),
                title: format!("track {i}"),
                artist: None,
                album: None,
            })
            .collect()
    }

    /// Rows whose category flips every 7th, so the geometry has a
    /// string of header runs to place rather than the single one a
    /// flat list has. More runs than a real ranking can produce
    /// (`rank_order` groups by category), which is the point: the
    /// header arithmetic is what is under test.
    pub(super) fn mixed(n: usize) -> Vec<SearchItem> {
        (0..n)
            .map(|i| {
                if (i / 7) % 2 == 0 {
                    SearchItem::LocalMusic {
                        path: format!("/m/{i}.flac").into(),
                        title: format!("track {i}"),
                        artist: None,
                        album: None,
                    }
                } else {
                    SearchItem::LocalVideo {
                        path: format!("/v/{i}.mkv").into(),
                        title: format!("clip {i}"),
                    }
                }
            })
            .collect()
    }

    /// A ranked source that walks the order for its boundaries instead
    /// of reading the runs `rank_order` recorded, i.e. what `Ranked`
    /// was. Everything else is the real path, so a frame through this
    /// and a frame through `Ranked` differ in exactly one thing: where
    /// the category boundaries came from.
    pub(super) struct Walked<'a>(pub(super) Ranked<'a>);

    impl Rows for Walked<'_> {
        fn count(&self) -> usize {
            self.0.count()
        }

        fn boundaries(&self) -> Vec<(usize, Category)> {
            self.0.ranking.walked_group_boundaries(self.0.corpus)
        }

        fn row(&self, at: usize) -> Option<&SearchItem> {
            self.0.row(at)
        }
    }

    /// `n` tracks ranked the way `App` holds them, alternating music
    /// and video so the order has two category runs and the layout has
    /// headers to place (`rank_order` groups by category, so the run
    /// count is bounded by `Category`'s variants however long the list
    /// gets). Shared with `frame_budget`.
    pub(super) fn ranked(n: usize) -> (Corpus, Ranking) {
        use crate::library::{LocalTrack, MediaKind};
        let mut corpus = Corpus::new();
        corpus.add_local_tracks((0..n).map(|i| LocalTrack {
            path: format!("/m/{i}.flac").into(),
            title: format!("track {i}"),
            artist: Some(format!("artist {}", i % 97)),
            album: None,
            duration_secs: None,
            kind: if i % 2 == 0 {
                MediaKind::Music
            } else {
                MediaKind::Video
            },
        }));
        let ranking = corpus.rank("track");
        (corpus, ranking)
    }

    /// What `layout` used to be: the display-row sequence as a `Vec`
    /// the length of the whole list, plus a linear scan for the
    /// selected row. Kept as the definition `RowLayout` has to agree
    /// with at every index; `frame_budget` next door times the two
    /// against each other, which is the only reason the old shape is
    /// worth keeping around.
    pub(super) fn eager_layout(
        rows: &dyn Rows,
        selected: usize,
        visible_rows: usize,
    ) -> (Vec<DisplayRow>, usize) {
        let count = rows.count();
        let boundaries = rows.boundaries();
        let mut display = Vec::with_capacity(count + boundaries.len());
        let mut next_boundary = boundaries.into_iter().peekable();
        for i in 0..count {
            if let Some((idx, cat)) = next_boundary.peek() {
                if *idx == i {
                    display.push(DisplayRow::Header(*cat));
                    next_boundary.next();
                }
            }
            display.push(DisplayRow::Item(i));
        }
        let selected_row = display
            .iter()
            .position(|r| matches!(r, DisplayRow::Item(i) if *i == selected))
            .unwrap_or(0);
        (
            display,
            crate::ui::scroll::offset_for(selected_row, visible_rows),
        )
    }

    /// `RowLayout` answers one display row at a time out of the
    /// category boundaries instead of out of a `Vec` as long as the
    /// list. It has to give the same answer at every index, the same
    /// total, and the same scroll offset, including at a `selected`
    /// that names no row at all (which scrolls to the top).
    #[test]
    fn the_row_geometry_answers_what_a_materialized_one_would() {
        for n in [0usize, 1, 3, 7, 8, 14, 40, 137] {
            let rows = mixed(n);
            for selected in [0usize, 1, 6, 7, n / 2, n.saturating_sub(1), n, n + 9] {
                for visible in [0usize, 1, 7, 40, 200] {
                    let (eager, offset) = eager_layout(&rows, selected, visible);
                    let lazy = layout(&rows, selected, visible);
                    let at = format!("n {n}, selected {selected}, visible {visible}");
                    assert_eq!(lazy.scroll_offset, offset, "scroll offset: {at}");
                    // three past the end, so "past the end" is pinned
                    // as `None` on both sides rather than unasked.
                    for row in 0..eager.len() + 3 {
                        assert_eq!(
                            lazy.get(row),
                            eager.get(row).copied(),
                            "display row {row} of {at}"
                        );
                    }
                }
            }
        }
    }

    /// The production row source is `Ranked` over a corpus, and its
    /// boundaries now come off the runs `rank_order` recorded rather
    /// than off a walk of the order. A frame through it has to be
    /// byte-identical to the same frame through that walk and to one
    /// through the owned list `search` returns, whose `Rows` impl is
    /// the untouched `group_boundaries`: every painted cell, the
    /// junction marks main.rs draws off `header_rows`, and the click
    /// map.
    #[test]
    fn a_ranked_frame_is_identical_through_either_boundary_source() {
        let area = Rect::new(1, 1, 26, 8);
        for n in [8usize, 40, 137, 1000] {
            let (corpus, ranking) = ranked(n);
            let rows = Ranked {
                ranking: &ranking,
                corpus: &corpus,
            };
            let count = rows.count();
            assert_eq!(count, n, "precondition: every track has to match");
            assert!(
                rows.boundaries().len() > 1,
                "precondition: a header inside the list, not just at the top"
            );
            let walked = Walked(Ranked {
                ranking: &ranking,
                corpus: &corpus,
            });
            let owned: Vec<SearchItem> = ranking.rows(&corpus).cloned().collect();
            for selected in [0usize, 1, 7, count / 2, count - 1, count + 3] {
                let at = format!("{n} rows, selected {selected}");
                for (how, other) in [
                    ("walked", &walked as &dyn Rows),
                    ("materialized", &owned as &dyn Rows),
                ] {
                    assert_eq!(
                        header_rows(&rows, selected, area),
                        header_rows(other, selected, area),
                        "header rows vs {how}: {at}"
                    );
                    assert_eq!(
                        row_items(&rows, selected, area),
                        row_items(other, selected, area),
                        "row map vs {how}: {at}"
                    );
                    let (stored, want) =
                        (painted_buf(&rows, selected), painted_buf(other, selected));
                    assert_eq!(
                        crate::ui::testbuf::grid(&stored),
                        crate::ui::testbuf::grid(&want),
                        "painted vs {how}: {at}"
                    );
                    // and the styles, which the chars above cannot
                    // show: the selection plate is a background and
                    // nothing else.
                    assert!(
                        stored == want,
                        "same chars, different styles vs {how}: {at}"
                    );
                }
            }
        }
    }

    /// And the production source's painted rows as literals, so the
    /// headers are pinned where they land rather than only against
    /// another computation of the same thing. The 1000-row ranking has
    /// its videos run starting at 500, which is the boundary the
    /// mid-list selection sits across.
    #[test]
    fn a_ranked_frame_paints_the_rows_recorded_here() {
        let (corpus, ranking) = ranked(1000);
        let rows = Ranked {
            ranking: &ranking,
            corpus: &corpus,
        };
        assert_eq!(
            ranking.group_boundaries(&corpus),
            vec![(0, Category::Songs), (500, Category::Videos)],
            "the runs this grid was recorded against"
        );
        assert_eq!(
            painted(&rows, 499),
            [
                "............................",
                ".  [fs] track 984...........",
                ".  [fs] track 986...........",
                ".  [fs] track 988...........",
                ".  [fs] track 990...........",
                ".  [fs] track 992...........",
                ".  [fs] track 994...........",
                ".  [fs] track 996...........",
                ".> [fs] track 998 · artist….",
                "............................",
            ]
            .join("\n")
        );
        assert_eq!(
            painted(&rows, 500),
            [
                "............................",
                ".  [fs] track 988...........",
                ".  [fs] track 990...........",
                ".  [fs] track 992...........",
                ".  [fs] track 994...........",
                ".  [fs] track 996...........",
                ".  [fs] track 998...........",
                ".───────── VIDEOS ─────────.",
                ".> [fv] track 1.............",
                "............................",
            ]
            .join("\n")
        );
    }

    /// What the row text used to cost: a `String` for the label, a
    /// `format!` for the row, both per visible row per frame. Kept as
    /// the definition `write_row_text` has to agree with, and as the
    /// benchmark's other half.
    pub(super) fn eager_row_text(item: &SearchItem, hovered: bool, starred: bool) -> String {
        let marker = if hovered { "> " } else { "  " };
        let star = if starred { "* " } else { "" };
        let owned: String = if hovered {
            label(item)
        } else {
            match item {
                SearchItem::LocalMusic { title, .. } => title.clone(),
                SearchItem::LocalVideo { title, .. } => title.clone(),
                SearchItem::Artist { name, .. } => name.clone(),
                SearchItem::Album { name, .. } => name.clone(),
                SearchItem::Song { title, .. } => title.clone(),
                SearchItem::Playlist { name, .. } => name.clone(),
                SearchItem::Silence { .. } => label(item),
                SearchItem::TorBoxFile { title, .. } => title.clone(),
                SearchItem::WatchSource { title, .. } => title.clone(),
            }
        };
        format!(
            "{marker}[{}{}] {star}{}",
            source_code(item),
            kind_code(item),
            owned
        )
    }

    /// The borrowed, one-buffer row text has to spell the same row as
    /// the `format!` it replaced, for every variant and both hover
    /// and star states; a silence is in here because it is the one
    /// item that keeps its metadata unhovered.
    #[test]
    fn the_row_text_says_what_a_formatted_one_said() {
        let client = crate::browse::fixtures::test_client();
        let items = [
            SearchItem::LocalMusic {
                path: "/m/a.flac".into(),
                title: "a title".into(),
                artist: Some("an artist".into()),
                album: Some("an album".into()),
            },
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
            SearchItem::Artist {
                id: "1".into(),
                name: "an artist".into(),
                client: client.clone(),
            },
            SearchItem::Album {
                id: "2".into(),
                name: "an album".into(),
                artist: "an artist".into(),
                client: client.clone(),
            },
            SearchItem::Song {
                id: "3".into(),
                title: "a song".into(),
                artist: "an artist".into(),
                album: "an album".into(),
                client: client.clone(),
            },
            SearchItem::Playlist {
                id: "4".into(),
                name: "a playlist".into(),
                client,
            },
        ];
        let mut out = String::new();
        for item in &items {
            for hovered in [false, true] {
                for starred in [false, true] {
                    // the same buffer across every case, so a missing
                    // `clear` shows up as the previous row's text
                    // still being in front.
                    write_row_text(&mut out, item, hovered, starred);
                    assert_eq!(
                        out,
                        eager_row_text(item, hovered, starred),
                        "{item:?} hovered {hovered} starred {starred}"
                    );
                }
            }
        }
    }

    /// Rows with an artist each, so the hovered row's metadata is
    /// visible and the others' absence is too.
    pub(super) fn with_artists(n: usize) -> Vec<SearchItem> {
        (0..n)
            .map(|i| SearchItem::LocalMusic {
                path: format!("/m/{i}.flac").into(),
                title: format!("track {i}"),
                artist: Some(format!("artist {i}")),
                album: None,
            })
            .collect()
    }

    /// The exact text of every row, hovered and not: the artist shows
    /// on the selected row only, and every other row is the bare
    /// title, which is what keeps a long list from reading as a wall
    /// of repeated artist names.
    ///
    /// Pinned as the whole painted grid rather than as a `contains`,
    /// because the row text is assembled from five pieces (marker,
    /// source code, kind code, star, label) and a `contains` on the
    /// title stays green while any of the other four moves.
    #[test]
    fn only_the_hovered_row_shows_its_artist() {
        let rows = with_artists(3);
        let area = Rect::new(0, 0, 26, 4);
        let mut buf = buffer(26, 4);
        ResultsList {
            items: &rows,
            selected: 1,
            empty_message: "",
            favorites: &Favorites::default(),
            remote_starred: &HashSet::new(),
            focused: true,
            elapsed: Duration::ZERO,
        }
        .render(area, &mut buf);
        assert_eq!(
            crate::ui::testbuf::grid(&buf),
            // the '.'s are cells nothing drew into (`testbuf::buffer`
            // fills with them), so this pins where each row's text
            // stops as well as what it says.
            [
                "───────── SONGS ──────────",
                "  [fs] track 0............",
                "> [fs] track 1 · artist 1.",
                "  [fs] track 2............",
            ]
            .join("\n")
        );
    }

    /// The list painted into a pane with a row of untouched buffer
    /// around it, so a recorded grid pins where the rows stop as well
    /// as what they say. `pub(super)` for the `eyeball` module, which
    /// prints the same grids.
    pub(super) fn painted_buf(rows: &dyn Rows, selected: usize) -> Buffer {
        let area = Rect::new(1, 1, 26, 8);
        let mut buf = buffer(28, 10);
        ResultsList {
            items: rows,
            selected,
            empty_message: "nothing here",
            favorites: &Favorites::default(),
            remote_starred: &HashSet::new(),
            focused: true,
            elapsed: Duration::ZERO,
        }
        .render(area, &mut buf);
        assert_outside_untouched(&buf, area);
        buf
    }

    /// The same pane as chars, for a recorded literal and for a
    /// failure message that can be read.
    pub(super) fn painted(rows: &dyn Rows, selected: usize) -> String {
        crate::ui::testbuf::grid(&painted_buf(rows, selected))
    }

    /// The four selections worth pinning: a list shorter than the
    /// pane, then the selection on row 0, mid-list and on the last row
    /// (the scrolled case) of a list far longer than it.
    pub(super) const RECORDED: [(usize, usize); 4] = [(5, 0), (137, 0), (137, 68), (137, 136)];

    /// Every cell of the painted pane at the four `RECORDED`
    /// selections, as the full-boundary walk drew them: literals taken
    /// off the old code, so a change to how the geometry is computed
    /// has to reproduce the rows rather than merely stay
    /// self-consistent. `eyeball::print_the_recorded_grids` prints the
    /// same four.
    #[test]
    fn the_painted_grid_is_what_the_full_boundary_walk_drew() {
        let expected = [
            [
                "............................",
                ".───────── SONGS ──────────.",
                ".> [fs] track 0.............",
                ".  [fs] track 1.............",
                ".  [fs] track 2.............",
                ".  [fs] track 3.............",
                ".  [fs] track 4.............",
                "............................",
                "............................",
                "............................",
            ],
            [
                "............................",
                ".───────── SONGS ──────────.",
                ".> [fs] track 0.............",
                ".  [fs] track 1.............",
                ".  [fs] track 2.............",
                ".  [fs] track 3.............",
                ".  [fs] track 4.............",
                ".  [fs] track 5.............",
                ".  [fs] track 6.............",
                "............................",
            ],
            [
                "............................",
                ".  [fs] track 62............",
                ".───────── VIDEOS ─────────.",
                ".  [fv] clip 63.............",
                ".  [fv] clip 64.............",
                ".  [fv] clip 65.............",
                ".  [fv] clip 66.............",
                ".  [fv] clip 67.............",
                ".> [fv] clip 68.............",
                "............................",
            ],
            [
                "............................",
                ".  [fs] track 130...........",
                ".  [fs] track 131...........",
                ".  [fs] track 132...........",
                ".───────── VIDEOS ─────────.",
                ".  [fv] clip 133............",
                ".  [fv] clip 134............",
                ".  [fv] clip 135............",
                ".> [fv] clip 136............",
                "............................",
            ],
        ];
        for (&(n, selected), want) in RECORDED.iter().zip(expected) {
            let rows = mixed(n);
            assert_eq!(
                painted(&rows, selected),
                want.join("\n"),
                "{n} rows, selected {selected}"
            );
        }
    }

    fn render(selected: usize, focused: bool, elapsed: Duration) -> Buffer {
        let area = Rect::new(0, 0, 20, 5);
        let mut buf = buffer(20, 5);
        ResultsList {
            items: &items(3),
            selected,
            empty_message: "",
            favorites: &Favorites::default(),
            remote_starred: &HashSet::new(),
            focused,
            elapsed,
        }
        .render(area, &mut buf);
        buf
    }

    fn row_bgs(buf: &Buffer, y: u16) -> Vec<Color> {
        (0..buf.area.width)
            .map(|x| buf.cell((x, y)).unwrap().bg)
            .collect()
    }

    /// Row 0 is the category header `layout` inserts, so the items
    /// start at row 1.
    const FIRST_ITEM_ROW: u16 = 1;

    /// The row map a click resolves against has to name the rows the
    /// render painted, including at a scroll offset and across the
    /// headers `layout` interleaves.
    #[test]
    fn the_recorded_row_map_matches_the_rows_that_were_drawn() {
        let all = items(40);
        for (height, selected) in [(5u16, 0usize), (5, 39), (12, 20), (40, 3)] {
            let area = Rect::new(2, 3, 24, height);
            let mut buf = buffer(area.x + area.width, area.y + area.height);
            ResultsList {
                items: &all,
                selected,
                empty_message: "",
                favorites: &Favorites::default(),
                remote_starred: &HashSet::new(),
                focused: true,
                elapsed: Duration::ZERO,
            }
            .render(area, &mut buf);

            let map = row_items(&all, selected, area);
            assert_eq!(map.len(), height as usize);
            assert!(
                map.contains(&Some(selected)),
                "the selection has to be on screen: {map:?}"
            );
            for (row, idx) in map.iter().enumerate() {
                let y = area.y + row as u16;
                let text: String = (area.x..area.x + area.width)
                    .map(|x| {
                        buf.cell((x, y))
                            .unwrap()
                            .symbol()
                            .chars()
                            .next()
                            .unwrap_or(' ')
                    })
                    .collect();
                match idx {
                    Some(i) => assert!(
                        text.contains(&format!("track {i}")),
                        "row {row} maps to {i} but reads {text:?}"
                    ),
                    // a header, or past the end: either way nothing a
                    // selection may move to.
                    None => assert!(
                        !text.contains("track "),
                        "row {row} maps to nothing but reads {text:?}"
                    ),
                }
            }
        }
    }

    /// A ranked order is indices into one corpus state, so reading it
    /// against another resolves to real-looking wrong rows. `Ranked`
    /// reports none at all instead, which is what makes the empty
    /// message show rather than a screenful of the wrong songs.
    #[test]
    fn a_ranking_read_against_a_changed_corpus_draws_nothing() {
        use crate::library::{LocalTrack, MediaKind};
        use crate::search::Corpus;

        let mut corpus = Corpus::new();
        corpus.add_local_tracks((0..6).map(|i| LocalTrack {
            path: format!("/m/{i}.flac").into(),
            title: format!("track {i}"),
            artist: None,
            album: None,
            duration_secs: None,
            kind: MediaKind::Music,
        }));
        let ranking = corpus.rank("track");
        assert_eq!(
            Ranked {
                ranking: &ranking,
                corpus: &corpus,
            }
            .count(),
            6
        );

        // the corpus moves on under the order.
        corpus.drop_local();
        let stale = Ranked {
            ranking: &ranking,
            corpus: &corpus,
        };
        assert_eq!(stale.count(), 0);
        assert!(stale.row(0).is_none());
        assert!(stale.boundaries().is_empty());

        let area = Rect::new(0, 0, 24, 4);
        let mut buf = buffer(24, 4);
        ResultsList {
            items: &stale,
            selected: 0,
            empty_message: "nothing found",
            favorites: &Favorites::default(),
            remote_starred: &HashSet::new(),
            focused: true,
            elapsed: Duration::ZERO,
        }
        .render(area, &mut buf);
        let painted = crate::ui::testbuf::grid(&buf);
        assert!(
            painted.contains("nothing found"),
            "a stale order must read as no results, not as rows\n{painted}"
        );
        assert!(
            !painted.contains("track "),
            "no row may be drawn from an order the corpus no longer matches\n{painted}"
        );
        assert!(row_items(&stale, 0, area).is_empty());
    }

    #[test]
    fn an_empty_or_flat_list_has_no_rows_to_click() {
        assert!(row_items(&items(0), 0, Rect::new(0, 0, 20, 5)).is_empty());
        assert!(row_items(&items(3), 0, Rect::new(0, 0, 20, 0)).is_empty());
    }

    #[test]
    fn the_selected_row_carries_a_background_across_its_full_width() {
        let buf = render(0, true, Duration::ZERO);
        let expected = shade(anim::selection_bg(Duration::ZERO, true));
        assert_eq!(row_bgs(&buf, FIRST_ITEM_ROW), vec![expected; 20]);
    }

    #[test]
    fn unselected_rows_keep_the_default_background() {
        let buf = render(0, true, Duration::ZERO);
        for y in [FIRST_ITEM_ROW + 1, FIRST_ITEM_ROW + 2] {
            assert_eq!(
                row_bgs(&buf, y),
                vec![Color::Reset; 20],
                "row {y} must not be highlighted"
            );
        }
    }

    #[test]
    fn the_focused_panes_selection_pulses_over_time() {
        let trough = render(0, true, Duration::ZERO);
        let peak = render(0, true, anim::SELECTION_PULSE_PERIOD / 2);
        assert_ne!(
            row_bgs(&trough, FIRST_ITEM_ROW),
            row_bgs(&peak, FIRST_ITEM_ROW),
            "the background has to move, not sit on one level"
        );
    }

    /// A row stays selected in a pane that lost focus; it just holds
    /// a flat dimmer background instead of competing for attention.
    #[test]
    fn an_unfocused_panes_selection_does_not_pulse() {
        let flat = shade(anim::SELECTION_BG_UNFOCUSED);
        for t in [Duration::ZERO, anim::SELECTION_PULSE_PERIOD / 2] {
            let buf = render(0, false, t);
            assert_eq!(row_bgs(&buf, FIRST_ITEM_ROW), vec![flat; 20]);
        }
    }

    /// Both shared painters write raw cells, which are bounds-checked
    /// against the buffer and not against the rect; a row outside the
    /// rect is a row of some other pane.
    #[test]
    fn a_section_rule_outside_its_rect_paints_nothing() {
        for (h, y) in [(0u16, 2u16), (0, 3), (1, 3), (2, 9), (3, 1)] {
            let area = Rect::new(1, 2, 18, h);
            let mut buf = buffer(22, 10);
            draw_header_row(&mut buf, area, y, "ALBUMS");
            assert_outside_untouched(&buf, area);
        }
    }

    /// Same for the selection plate, which sets only a background;
    /// `assert_outside_untouched` reads chars, so the plate needs its
    /// own sweep. The bottom edge was guarded, the top edge was not.
    #[test]
    fn a_selection_plate_outside_its_rect_paints_nothing() {
        for (h, y) in [(0u16, 2u16), (1, 3), (3, 9), (3, 1)] {
            let area = Rect::new(1, 2, 18, h);
            let mut buf = buffer(22, 10);
            fill_row_bg(&mut buf, area, y, 0.5);
            for by in 0..10u16 {
                for bx in 0..22u16 {
                    let inside = (area.x..area.x + area.width).contains(&bx)
                        && (area.y..area.y + area.height).contains(&by);
                    if !inside {
                        assert_eq!(
                            buf.cell((bx, by)).unwrap().bg,
                            Color::Reset,
                            "plate at ({bx}, {by}) is outside rect {area:?} (row {y}, h {h})"
                        );
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod eyeball {
    use super::*;
    use crate::ui::testbuf::{buffer, grid};

    /// The section rule, printed into a buffer taller than the rect it
    /// was handed, so a rule that escaped reads as a line of `─` below
    /// the rect rather than as a coordinate in a failure message:
    /// `cargo test --lib ui::results_list::eyeball -- --nocapture`.
    /// Asserts the floor too, so it is a test and not just a printer.
    #[test]
    fn print_the_section_rule_against_its_rect() {
        for (h, y) in [(1u16, 2u16), (0, 2), (2, 4)] {
            let area = Rect::new(0, 2, 20, h);
            let mut buf = buffer(20, 6);
            draw_header_row(&mut buf, area, y, "ALBUMS");
            println!("--- rect y=2 h={h}, rule asked for row {y}\n{}", grid(&buf));
        }
        // the one case that must draw: a row the rect owns.
        let mut buf = buffer(20, 6);
        draw_header_row(&mut buf, Rect::new(0, 2, 20, 1), 2, "ALBUMS");
        assert!(grid(&buf).contains("ALBUMS"), "{}", grid(&buf));
    }

    /// The grids `the_painted_grid_is_what_the_full_boundary_walk_drew`
    /// and `a_ranked_frame_paints_the_rows_recorded_here` pin, printed
    /// so they can be read rather than diffed out of an assert
    /// failure:
    /// `cargo test --lib ui::results_list::eyeball -- --nocapture`.
    #[test]
    fn print_the_recorded_grids() {
        for (n, selected) in super::tests::RECORDED {
            let rows = super::tests::mixed(n);
            println!(
                "--- {n} rows in a 26x8 pane at (1,1), selected {selected}\n{}",
                super::tests::painted(&rows, selected)
            );
        }
        // and the production source, either side of its one boundary.
        let (corpus, ranking) = super::tests::ranked(1000);
        let rows = Ranked {
            ranking: &ranking,
            corpus: &corpus,
        };
        println!("--- a 1000-row ranking, runs {:?}", rows.boundaries());
        for selected in [499usize, 500] {
            println!(
                "--- ranked, selected {selected}\n{}",
                super::tests::painted(&rows, selected)
            );
        }
    }

    /// The whole list, printed at three heights against a 6-row pane:
    /// shorter than the pane, exactly filling it, and longer than it
    /// (the scrolled case, pinned to the last row). Read it:
    /// `cargo test --lib ui::results_list::eyeball -- --nocapture`.
    /// Asserts the floor too, so it is a test and not just a printer.
    #[test]
    fn print_the_results_list_at_three_list_lengths() {
        for (n, selected) in [(3usize, 1usize), (5, 0), (40, 39)] {
            let area = Rect::new(1, 1, 24, 6);
            let mut buf = buffer(26, 8);
            let rows = super::tests::items(n);
            ResultsList {
                items: &rows,
                selected,
                empty_message: "nothing here",
                favorites: &Favorites::default(),
                remote_starred: &HashSet::new(),
                focused: true,
                elapsed: Duration::ZERO,
            }
            .render(area, &mut buf);
            println!(
                "--- {n} rows in a 6-row pane at (1,1), selected {selected}\n{}",
                grid(&buf)
            );
            // the selection is always on screen, and the pane is full
            // whenever there are enough rows to fill it.
            let painted = grid(&buf);
            assert!(
                painted.contains(&format!("track {selected}")),
                "the selected row has to be visible\n{painted}"
            );
        }

        // and the empty list, which is a message rather than a blank.
        let mut buf = buffer(26, 8);
        ResultsList {
            items: &super::tests::items(0),
            selected: 0,
            empty_message: "nothing here",
            favorites: &Favorites::default(),
            remote_starred: &HashSet::new(),
            focused: true,
            elapsed: Duration::ZERO,
        }
        .render(Rect::new(1, 1, 24, 6), &mut buf);
        println!("--- no rows at all\n{}", grid(&buf));
        assert!(grid(&buf).contains("nothing here"), "{}", grid(&buf));
    }
}
