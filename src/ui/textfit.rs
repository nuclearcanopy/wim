//! Terminal width is unpredictable and resizable; text that silently
//! runs off the edge with no indication is a real bug, not a cosmetic
//! nitpick: it hides what you're actively typing. Two strategies,
//! chosen per use case:
//!
//! - `tail_fit`: for text being actively typed (search query, login
//!   fields); shows the *end* of the string so the cursor position
//!   stays visible as you type past the visible width, the way any
//!   real text input does.
//! - `truncate_with_ellipsis`: for static/display text (status
//!   messages, result rows); shows the start and marks the cut with
//!   `…` so it's visually obvious there's more, rather than an
//!   unexplained hard stop.
//!
//! All of it measures *display width*, not chars: CJK, Hangul and
//! emoji occupy two terminal cells each, so a char count under-
//! measures them by half and a music library full of them overflows
//! its column and shoves the box borders out of alignment.
//!
//! The unit is the **grapheme cluster**, not the char, and the two
//! disagree on ordinary library content: nfd `Beyoncé` is 8 chars and
//! 7 clusters, `❤️` is 2 chars and 1. Measuring one unit and drawing
//! the other is what the whole module used to do; see
//! docs/text-measurement.md.

use unicode_segmentation::UnicodeSegmentation;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

/// The grapheme clusters of `s`: one user-perceived character each,
/// which is the unit a terminal draws into a cell and advances by.
pub fn clusters(s: &str) -> impl DoubleEndedIterator<Item = &str> {
    s.graphemes(true)
}

/// Terminal cells one cluster occupies. Measured on the whole cluster
/// so the emoji-presentation and jamo rules apply: per *char* a zwj
/// family measures six cells and `❤️` one, neither of which is what
/// gets drawn.
pub fn cluster_cells(c: &str) -> usize {
    UnicodeWidthStr::width(c)
}

/// True when `s` is printable ascii: one byte, one char, one cluster
/// and one cell each, with nothing in it that can join or measure
/// zero. `\r\n` is the one ascii cluster and a control char the one
/// ascii zero, so both are excluded.
pub fn plain_ascii(s: &str) -> bool {
    // `is_ascii` is a word-at-a-time library check and most strings
    // stop there; the control scan only runs on ascii.
    s.is_ascii() && !s.as_bytes().iter().any(u8::is_ascii_control)
}

/// True when every char of `s` is exactly one cell. Then a char is a
/// cell, so `width` and `draw::at` need no grapheme walk; the *fits*
/// still do, because a char here is not necessarily a whole cluster
/// and cutting one in half is visible.
///
/// This is what keeps a frame cheap, because nearly all the text in
/// one is one cell per char (ascii, plus the chrome's middle dot,
/// ellipsis and rules) and a grapheme walk costs ~180ns a cluster in
/// this profile. Almost every codepoint that can join a cluster is
/// ruled out by the same check: a combining mark, zwj, variation
/// selector or jamo medial measures zero cells, and a jamo initial,
/// cjk glyph or skin-tone modifier measures two. The two that slip
/// through, a flag's pair of regional indicators and an indic matra,
/// join into a cluster of the *same* total width, so the measurement
/// still matches what `draw::at` advances by; cutting between them
/// would not, which is why the fits use `plain_ascii` instead.
pub fn one_cell_chars(s: &str) -> bool {
    s.chars().all(|c| char_cells(c) == 1)
}

/// Display width of `s` in terminal cells: the sum of its clusters,
/// which is exactly what `draw::at` advances by. The layout decisions
/// call this many times a frame, hence the two cheap gates first.
pub fn width(s: &str) -> usize {
    if plain_ascii(s) {
        return s.len();
    }
    if one_cell_chars(s) {
        return s.chars().count();
    }
    clusters(s).map(cluster_cells).sum()
}

/// Terminal cells one char occupies; a control or zero-width char
/// counts as 0. Only sound for text known to be one char per cluster:
/// a combining mark or a zwj is 0 here and the cluster it belongs to
/// is not measurable without its neighbours, so a drawing loop wants
/// `clusters` and `cluster_cells`.
pub fn char_cells(ch: char) -> usize {
    UnicodeWidthChar::width(ch).unwrap_or(0)
}

/// Whether `b` joins `a` into one cluster. Asked of two chars on a
/// stack buffer, so a cut can be checked without segmenting the whole
/// string.
fn joined(a: char, b: char) -> bool {
    let mut buf = [0u8; 8];
    let head = a.encode_utf8(&mut buf).len();
    let len = head + b.encode_utf8(&mut buf[head..]).len();
    std::str::from_utf8(&buf[..len]).is_ok_and(|pair| clusters(pair).count() == 1)
}

/// Byte index `cells` chars into `s` (or its end, if it is shorter),
/// for an `s` where a char is a cell. `None` when that index lands
/// *inside* a cluster, where a cell count is not a cut: a flag's two
/// regional indicators are two cells and one cluster, and so is an
/// indic consonant with its matra. Callers fall back to the walk.
fn char_cut(s: &str, cells: usize) -> Option<usize> {
    let Some((at, next)) = s.char_indices().nth(cells) else {
        return Some(s.len());
    };
    match s[..at].chars().next_back() {
        Some(prev) => (!joined(prev, next)).then_some(at),
        // nothing before the cut, so there is nothing to cut into
        None => Some(at),
    }
}

/// Longest prefix of `s` that fits in `max_width` cells. A
/// double-width cluster that would straddle the right edge is dropped
/// rather than half-drawn, so the result is never *wider* than asked
/// for (it can be one cell narrower).
pub fn truncate_to_width(s: &str, max_width: usize) -> String {
    if plain_ascii(s) {
        // one byte, one cluster, one cell: the cut is a cell count
        return s[..s.len().min(max_width)].to_string();
    }
    if one_cell_chars(s) {
        if let Some(at) = char_cut(s, max_width) {
            return s[..at].to_string();
        }
    }
    let mut out = String::new();
    let mut used = 0;
    for c in clusters(s) {
        let cw = cluster_cells(c);
        // a zero-width cluster owns no cell: a lone combining run, a
        // stray zwj, a control char. keeping it is what gave it a cell
        // of its own and shoved everything after it one column left.
        if cw == 0 {
            continue;
        }
        if used + cw > max_width {
            break;
        }
        out.push_str(c);
        used += cw;
    }
    out
}

/// Returns the last `max_width` cells' worth of `s`, or all of it if
/// it already fits.
pub fn tail_fit(s: &str, max_width: usize) -> String {
    if plain_ascii(s) {
        return s[s.len().saturating_sub(max_width)..].to_string();
    }
    if one_cell_chars(s) {
        let drop = s.chars().count().saturating_sub(max_width);
        if let Some(at) = char_cut(s, drop) {
            return s[at..].to_string();
        }
    }
    let mut kept: Vec<&str> = Vec::new();
    let mut used = 0;
    for c in clusters(s).rev() {
        let cw = cluster_cells(c);
        if cw == 0 {
            continue;
        }
        if used + cw > max_width {
            break;
        }
        kept.push(c);
        used += cw;
    }
    kept.reverse();
    kept.concat()
}

/// Truncates `s` to `max_width` cells, replacing the last visible
/// cluster with `…` (itself one cell) if anything was actually cut.
/// Never wider than `max_width` once drawn; `eyeball` holds that
/// contract against a real `Buffer` rather than against `width`.
pub fn truncate_with_ellipsis(s: &str, max_width: usize) -> String {
    if max_width == 0 {
        return String::new();
    }
    if width(s) <= max_width {
        return s.to_string();
    }
    if max_width == 1 {
        return "…".to_string();
    }
    let mut out = truncate_to_width(s, max_width - 1);
    out.push('…');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tail_fit_leaves_short_strings_alone() {
        assert_eq!(tail_fit("hello", 10), "hello");
    }

    #[test]
    fn tail_fit_keeps_the_end_not_the_start() {
        assert_eq!(tail_fit("https://navidrome.example.com", 10), "xample.com");
        // 10 chars: last 10 of the string, always the typing cursor end
        assert_eq!(tail_fit("abcdefghij", 10), "abcdefghij");
        assert_eq!(tail_fit("abcdefghijk", 10), "bcdefghijk");
    }

    #[test]
    fn truncate_leaves_short_strings_alone() {
        assert_eq!(truncate_with_ellipsis("hello", 10), "hello");
    }

    #[test]
    fn truncate_marks_the_cut_with_an_ellipsis() {
        assert_eq!(truncate_with_ellipsis("hello world", 8), "hello w…");
        assert_eq!(truncate_with_ellipsis("hello world", 1), "…");
        assert_eq!(truncate_with_ellipsis("hello world", 0), "");
    }

    #[test]
    fn handles_multibyte_characters_without_mis_slicing() {
        // "─" is 3 bytes in utf-8 but one cell wide; must not panic
        // or slice mid-codepoint.
        let s = "──────────"; // 10 cells, 30 bytes
        assert_eq!(width(&tail_fit(s, 5)), 5);
        assert_eq!(width(&truncate_with_ellipsis(s, 5)), 5);
    }

    #[test]
    fn measures_wide_characters_as_two_cells() {
        assert_eq!(width("日本語"), 6);
        assert_eq!(width("한국어"), 6);
        assert_eq!(width("🎵"), 2);
        assert_eq!(width("ab日"), 4);
    }

    #[test]
    fn truncates_cjk_on_the_cell_that_actually_fits() {
        // 12 cells of text into 7: 6 cells of text plus the ellipsis.
        assert_eq!(truncate_with_ellipsis("日本語テスト曲", 7), "日本語…");
        // one less cell of room drops a whole wide char rather than
        // half-drawing it, so the result stays inside the column.
        assert_eq!(truncate_with_ellipsis("日本語テスト曲", 6), "日本…");
        // a wide string that exactly fills the column isn't cut
        assert_eq!(truncate_with_ellipsis("日本語", 6), "日本語");
    }

    #[test]
    fn truncates_emoji_without_overflowing_the_column() {
        let s = "🎵🎵🎵 track";
        let out = truncate_with_ellipsis(s, 5);
        assert_eq!(out, "🎵🎵…");
        assert!(width(&out) <= 5);
    }

    #[test]
    fn tail_fit_counts_cells_not_chars() {
        // "abc日本" is 7 cells; the last 4 are the two wide chars.
        assert_eq!(tail_fit("abc日本", 4), "日本");
        assert_eq!(tail_fit("abc日本", 5), "c日本");
        // an odd budget can't split a wide char, so a cell goes
        // unused rather than drawing half of one.
        assert_eq!(width(&tail_fit("abc日本", 3)), 2);
    }

    #[test]
    fn truncate_to_width_never_exceeds_the_budget() {
        for max in 0..10 {
            assert!(width(&truncate_to_width("日本語テスト", max)) <= max);
        }
    }

    /// The unit of measurement. A cluster is what the terminal puts in
    /// a cell, and these are the ones a char count gets wrong: nfd
    /// latin and hangul, a variation selector, a keycap, a zwj family.
    #[test]
    fn a_cluster_is_one_grapheme_not_one_char() {
        for (s, want) in [
            ("Beyonce\u{301}", 7),
            // three jamo join into one cluster, then " song" is five
            ("\u{1100}\u{1161}\u{11A8} song", 6),
            ("\u{2764}\u{FE0F} love", 6),
            ("1\u{FE0F}\u{20E3}x", 2),
            ("\u{1F469}\u{200D}\u{1F469}\u{200D}\u{1F467}", 1),
            ("日本語", 3),
        ] {
            assert_eq!(clusters(s).count(), want, "{s:?}");
        }
    }

    /// `char_cells` and `cluster_cells` are two different models and
    /// mixing them is the whole bug: per char a zwj family measures
    /// six cells, per cluster two.
    #[test]
    fn a_cluster_is_measured_whole_not_char_by_char() {
        let family = "\u{1F469}\u{200D}\u{1F469}\u{200D}\u{1F467}";
        assert_eq!(cluster_cells(family), 2);
        assert_eq!(family.chars().map(char_cells).sum::<usize>(), 6);
        // and the other direction: a variation selector promotes a
        // one-cell glyph to two, which no per-char sum can see.
        assert_eq!(cluster_cells("\u{2764}\u{FE0F}"), 2);
        assert_eq!("\u{2764}\u{FE0F}".chars().map(char_cells).sum::<usize>(), 1);
    }

    /// A cluster with no cell of its own cannot be given one. A
    /// pasted run of combining marks with nothing to combine with is
    /// the case that drove a `u16` column past 65535.
    #[test]
    fn a_zero_width_cluster_is_dropped_rather_than_given_a_cell() {
        let run = "\u{301}".repeat(400);
        assert_eq!(width(&run), 0);
        assert_eq!(truncate_to_width(&run, 8), "");
        assert_eq!(tail_fit(&run, 8), "");
        // but a mark that *does* have a base stays with it
        assert_eq!(truncate_to_width("e\u{301}f", 8), "e\u{301}f");
        assert_eq!(tail_fit("e\u{301}f", 2), "e\u{301}f");
    }

    /// nfd text fits by cells, so a 7-cell name fits a 7-cell column
    /// whatever its byte or char count.
    #[test]
    fn nfd_text_fits_the_column_its_cells_fit() {
        let nfd = "Beyonce\u{301}";
        assert_eq!(width(nfd), 7);
        assert_eq!(truncate_to_width(nfd, 7), nfd);
        assert_eq!(width(&truncate_with_ellipsis(nfd, 6)), 6);
        // the accent is never cut off its base, leaving a loose mark
        assert_eq!(truncate_to_width(nfd, 6), "Beyonc");
    }

    /// The grapheme walk, as the reference the ascii fast path has to
    /// agree with.
    fn slow_width(s: &str) -> usize {
        clusters(s).map(cluster_cells).sum()
    }

    fn slow_truncate(s: &str, max: usize) -> String {
        let mut out = String::new();
        let mut used = 0;
        for c in clusters(s) {
            let cw = cluster_cells(c);
            if cw == 0 {
                continue;
            }
            if used + cw > max {
                break;
            }
            out.push_str(c);
            used += cw;
        }
        out
    }

    fn slow_tail(s: &str, max: usize) -> String {
        let mut kept: Vec<&str> = Vec::new();
        let mut used = 0;
        for c in clusters(s).rev() {
            let cw = cluster_cells(c);
            if cw == 0 {
                continue;
            }
            if used + cw > max {
                break;
            }
            kept.push(c);
            used += cw;
        }
        kept.reverse();
        kept.concat()
    }

    /// `width` is called many times per frame by the layout decisions,
    /// so printable ascii skips the grapheme walk. That is only sound
    /// while the two agree exactly, including on the strings the fast
    /// path declines: a tab is one ascii byte and zero cells.
    #[test]
    fn the_ascii_fast_path_agrees_with_the_grapheme_walk() {
        let family = "mix \u{1F469}\u{200D}\u{1F469}\u{200D}\u{1F467}";
        for s in [
            "",
            " ",
            "~",
            "abc",
            "0123456789",
            "j/k select, h/l adjust",
            "a\tb",
            "a\u{7f}b",
            "a\u{0}b",
            "a\r\nb",
            "\r\n\r\n",
            "Into Dust (Remastered) \u{b7} Mazzy Star",
            "a\u{2026}\u{2500}z",
            "\u{65E5}\u{672C}",
            "Beyonce\u{301}",
            "\u{301}",
            family,
            // the two that join while measuring one cell each: a flag
            // and an indic matra. both come to the same total either
            // way, which is what the fast path relies on.
            "\u{1F1EC}\u{1F1E7} uk",
            "\u{0915}\u{093E}",
            // and the width-2 joiner that must *not* take it
            "\u{1F44D}\u{1F3FB}",
        ] {
            assert_eq!(width(s), slow_width(s), "width {s:?}");
            for max in 0..12 {
                assert_eq!(
                    truncate_to_width(s, max),
                    slow_truncate(s, max),
                    "truncate_to_width {s:?}/{max}"
                );
                assert_eq!(tail_fit(s, max), slow_tail(s, max), "tail_fit {s:?}/{max}");
            }
        }
    }

    /// The contract the module docs claim, over the strings that broke
    /// it: an emoji with a variation selector, a keycap and a zwj
    /// family all used to come back wider than asked for.
    #[test]
    fn no_fit_returns_more_cells_than_it_was_given() {
        for (_, s) in eyeball::SAMPLES {
            for max in 0..14 {
                assert!(width(&truncate_to_width(s, max)) <= max, "{s:?}/{max}");
                assert!(width(&truncate_with_ellipsis(s, max)) <= max, "{s:?}/{max}");
                assert!(width(&tail_fit(s, max)) <= max, "{s:?}/{max}");
            }
        }
    }
}

/// Cells between columns in a `columns` layout.
const COLUMN_GUTTER: usize = 2;

/// Narrowest a column may get before dropping to fewer of them. A
/// 12-cell column shows almost no title, so more columns stop being
/// worth the rows they save.
const MIN_COLUMN: usize = 18;

/// Packs `items` into up to `max_cols` columns, as the lines to print.
///
/// Column-major, like `ls`: reading *down* each column keeps a sorted
/// or ranked list in order, where row-major would scatter it across
/// the screen.
///
/// **Each column is sized to its own contents**, which is the whole
/// algorithm. Dividing the width evenly is the obvious thing and it is
/// wrong both ways: a list of eight-cell names wastes two thirds of
/// every column, and a list of long titles gets chopped at the same
/// arbitrary place whether or not there was room. So this tries three
/// columns, then two, then one, and takes the first whose *actual*
/// column widths fit; short names end up packed tight, long ones get
/// the room they need.
///
/// Only when nothing fits untruncated does it fall back to dividing
/// the width, and then only while each column stays at least
/// `MIN_COLUMN` cells, so a narrow terminal drops to fewer columns
/// rather than shredding every item into nothing. Items past that are
/// ellipsised, never wrapped, so one item is always one row and the
/// grid cannot come apart.
pub fn columns(items: &[String], avail: usize, max_cols: usize) -> Vec<String> {
    if items.is_empty() || avail == 0 {
        return Vec::new();
    }
    let max_cols = max_cols.max(1).min(items.len());

    // the widths `cols` columns would actually need, column-major.
    let natural = |cols: usize| -> (usize, Vec<usize>) {
        let rows = items.len().div_ceil(cols);
        let widths: Vec<usize> = (0..cols)
            .map(|c| {
                items
                    .iter()
                    .skip(c * rows)
                    .take(rows)
                    .map(|i| width(i))
                    .max()
                    .unwrap_or(0)
            })
            .collect();
        (
            widths.iter().sum::<usize>() + COLUMN_GUTTER * (cols - 1),
            widths,
        )
    };

    let (cols, widths) = (1..=max_cols)
        .rev()
        .find_map(|c| {
            let (total, widths) = natural(c);
            (total <= avail).then_some((c, widths))
        })
        .unwrap_or_else(|| {
            // nothing fits as-is, so share the width out and truncate,
            // keeping columns readable rather than merely numerous.
            let share = |c: usize| avail.saturating_sub(COLUMN_GUTTER * (c - 1)) / c;
            let cols = (1..=max_cols)
                .rev()
                .find(|&c| share(c) >= MIN_COLUMN)
                .unwrap_or(1);
            (cols, vec![share(cols).max(1); cols])
        });

    let rows = items.len().div_ceil(cols);
    (0..rows)
        .map(|row| {
            let mut line = String::new();
            for (col, col_w) in widths.iter().enumerate() {
                // column-major indexing is the point; row-major would
                // read across and lose the ranking.
                let Some(item) = items.get(col * rows + row) else {
                    break;
                };
                let cell = truncate_with_ellipsis(item, *col_w);
                let more = items.get((col + 1) * rows + row).is_some();
                line.push_str(&cell);
                if more {
                    // padded to the column, not the item, so the next
                    // one starts in the same place on every row.
                    // measured in cells: a wide glyph costs two.
                    line.push_str(&" ".repeat(col_w.saturating_sub(width(&cell)) + COLUMN_GUTTER));
                }
            }
            line
        })
        .collect()
}

#[cfg(test)]
mod column_tests {
    use super::*;

    fn items(n: usize) -> Vec<String> {
        (0..n).map(|i| format!("item{i:02}")).collect()
    }

    #[test]
    fn a_wide_terminal_uses_every_column_it_is_allowed() {
        let lines = columns(&items(9), 80, 3);
        assert_eq!(lines.len(), 3, "9 items over 3 columns is 3 rows");
    }

    #[test]
    fn it_never_exceeds_the_column_cap() {
        let lines = columns(&items(60), 400, 3);
        assert_eq!(lines.len(), 20, "60 items, 3 columns, not more");
    }

    /// Reading down a column has to keep the order, or a ranked list
    /// is scattered across the screen.
    #[test]
    fn columns_are_filled_downwards() {
        let lines = columns(&items(6), 80, 3);
        assert_eq!(lines.len(), 2);
        assert!(lines[0].starts_with("item00"), "{:?}", lines[0]);
        // 6 items, 3 columns, 2 rows: the second column starts at the
        // third item, not the second.
        assert!(lines[0].contains("item02"), "{:?}", lines[0]);
        assert!(lines[0].contains("item04"), "{:?}", lines[0]);
        assert!(lines[1].starts_with("item01"), "{:?}", lines[1]);
    }

    /// Short names pack tight even in a narrow terminal, because the
    /// columns are sized to the content: six 6-cell names need
    /// 6+6+6+4 = 22 cells for three columns, so 24 is enough. An even
    /// division would have given 8-cell columns and refused.
    #[test]
    fn short_names_pack_into_a_narrow_terminal() {
        let lines = columns(&items(6), 24, 3);
        assert_eq!(lines.len(), 2, "{lines:?}");
    }

    /// And long ones drop to a single column rather than three
    /// unreadable slivers.
    #[test]
    fn long_names_fall_back_to_one_column() {
        let long: Vec<String> = (0..6).map(|i| format!("a fairly long title {i}")).collect();
        let lines = columns(&long, 30, 3);
        assert_eq!(lines.len(), 6, "{lines:?}");
    }

    /// The payoff of content-sized columns: a column only takes the
    /// room its own items need, so a short first column does not
    /// reserve space a long second one could have used.
    #[test]
    fn a_column_is_only_as_wide_as_its_own_contents() {
        let mixed = vec![
            "ab".to_string(),
            "cd".to_string(),
            "a much longer title here".to_string(),
            "another long one there".to_string(),
        ];
        // 2 rows, 2 columns: col 0 is 2 cells, col 1 is 24.
        let lines = columns(&mixed, 40, 3);
        assert_eq!(lines.len(), 2, "{lines:?}");
        assert!(
            lines[0].starts_with("ab  a much longer"),
            "the long column starts right after the short one: {:?}",
            lines[0]
        );
    }

    #[test]
    fn no_line_is_wider_than_the_terminal() {
        let long: Vec<String> = (0..7).map(|i| format!("{i} {}", "x".repeat(90))).collect();
        for w in [20usize, 40, 72, 100] {
            for line in columns(&long, w, 3) {
                assert!(width(&line) <= w, "{w}: {} cells in {line:?}", width(&line));
            }
        }
    }

    #[test]
    fn columns_line_up_across_rows() {
        let mut mixed = items(6);
        mixed[0] = "short".into();
        mixed[1] = "a much longer item here".into();
        let lines = columns(&mixed, 80, 3);
        // the second column begins at the same cell on both rows.
        let at = |l: &str, needle: &str| {
            let cells: Vec<char> = l.chars().collect();
            let want: Vec<char> = needle.chars().collect();
            cells
                .windows(want.len())
                .position(|w| w == want.as_slice())
                .unwrap_or_else(|| panic!("{needle} missing from {l:?}"))
        };
        assert_eq!(at(&lines[0], "item02"), at(&lines[1], "item03"));
    }

    #[test]
    fn nothing_in_nothing_out() {
        assert!(columns(&[], 80, 3).is_empty());
        assert!(columns(&items(3), 0, 3).is_empty());
    }

    /// One item cannot be laid out in three columns, and trying would
    /// leave two empty ones padded out to nothing.
    #[test]
    fn fewer_items_than_columns_uses_fewer_columns() {
        let lines = columns(&items(2), 80, 3);
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].trim_end().matches("item").count(), 2);
    }
}

/// Prints, rather than asserts, what the measuring functions and a
/// real `Buffer` do for the strings that used to come apart; see
/// docs/text-measurement.md. Run with
/// `cargo test eyeball -- --nocapture`.
#[cfg(test)]
pub mod eyeball {
    use super::*;
    use crate::shellsong::{Console, NoCandidates};
    use crate::theme::{shade, NEAR_WHITE};
    use crate::ui::draw;
    use crate::ui::testbuf::{buffer, grid};
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use ratatui::layout::Rect;
    use ratatui::widgets::Widget;

    /// Every string the audit measured, all of it ordinary library
    /// content: emoji with a variation selector, a keycap, a zwj
    /// family, nfd latin (how apfs stores filenames), decomposed
    /// hangul, cjk, and a pasted combining-mark run with no base.
    pub const SAMPLES: &[(&str, &str)] = &[
        ("vs16 emoji", "\u{2764}\u{FE0F} love song"),
        ("keycap", "1\u{FE0F}\u{20E3} favourites"),
        (
            "zwj family",
            "\u{1F469}\u{200D}\u{1F469}\u{200D}\u{1F467} mix",
        ),
        ("nfd latin", "Beyonce\u{301}"),
        ("nfd hangul", "\u{1100}\u{1161}\u{11A8} song"),
        ("cjk", "\u{65E5}\u{672C}\u{8A9E}\u{30C6}\u{30B9}\u{30C8}"),
        ("combining run", "\u{301}\u{301}\u{301}\u{301}\u{301}"),
    ];

    /// Sum of per-char widths, i.e. the model `char_cells` implements,
    /// kept here only to show how far it diverges from `width`.
    fn char_sum(s: &str) -> usize {
        s.chars().map(char_cells).sum()
    }

    /// `draw::at` into a row with a border at `BORDER`, as the x it
    /// returned, the painted row, and the rightmost cell it touched.
    const BORDER: u16 = 8;

    fn painted(s: &str, budget: usize) -> (u16, String, Option<u16>) {
        let mut buf = buffer(24, 1);
        draw::put(&mut buf, BORDER, 0, '\u{2502}', shade(NEAR_WHITE));
        let end = draw::at(&mut buf, 0, 0, s, shade(NEAR_WHITE), budget);
        let row = grid(&buf);
        let last = (0..24).rev().find(|x| {
            let sym = buf.cell((*x, 0u16)).unwrap().symbol();
            sym != "." && !(*x == BORDER && sym == "\u{2502}")
        });
        (end, row, last)
    }

    #[test]
    fn measurement() {
        println!("\n== how wide is it ==");
        println!(
            "{:<15} {:>5} {:>9} {:>9}",
            "sample", "width", "str-model", "char-model"
        );
        for (name, s) in SAMPLES {
            println!(
                "{name:<15} {:>5} {:>9} {:>9}   {s:?}",
                width(s),
                UnicodeWidthStr::width(*s),
                char_sum(s),
            );
        }
    }

    #[test]
    fn truncation() {
        println!("\n== truncation into 8 cells ==");
        for (name, s) in SAMPLES {
            let cut = truncate_to_width(s, 8);
            let ell = truncate_with_ellipsis(s, 8);
            let tail = tail_fit(s, 8);
            println!(
                "{name:<15} to_width={:>2} ellipsis={:>2} tail={:>2}   {cut:?} {ell:?} {tail:?}",
                width(&cut),
                width(&ell),
                width(&tail),
            );
        }
    }

    #[test]
    fn drawing() {
        println!("\n== draw::at, budget 8, border at column 8 ==");
        for (name, s) in SAMPLES {
            let (end, row, last) = painted(s, 8);
            println!("{name:<15} returned x={end:<3} last painted={last:?}");
            println!("{:<15} |{row}|", "");
        }
    }

    fn typed(input: &str) -> Console {
        let mut c = Console::empty();
        for ch in input.chars() {
            c.handle_key(
                KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE),
                &NoCandidates,
            );
        }
        c
    }

    /// The overlay's input row and the shell's prompt row, for each
    /// sample typed at the prompt: the caret has to land on the cell
    /// after the text, and neither may paint past its own edge.
    #[test]
    fn console_rows() {
        println!("\n== shellsong overlay input row (box is 24 wide) ==");
        for (name, s) in SAMPLES {
            let c = typed(s);
            let mut buf = buffer(24, 5);
            crate::ui::shellsong::ShellsongConsole {
                console: &c,
                cursor_visible: true,
            }
            .render(Rect::new(0, 0, 24, 5), &mut buf);
            let row = grid(&buf)
                .lines()
                .find(|r| r.contains(':'))
                .unwrap_or("<no input row>")
                .to_string();
            println!("{name:<15} |{row}|");
        }

        println!("\n== shellswim prompt row (screen is 24 wide) ==");
        for (name, s) in SAMPLES {
            let c = typed(s);
            let mut buf = buffer(24, 4);
            crate::swimconsole::SwimConsole {
                console: &c,
                cursor_visible: true,
            }
            .render(Rect::new(0, 0, 24, 4), &mut buf);
            let row = grid(&buf).lines().last().unwrap_or("").to_string();
            println!("{name:<15} |{row}|");
        }
    }

    /// A real ghost on both consoles, caret at the end of the line and
    /// then seven cells back into it. The mid-line row is the one that
    /// used to paint `atline` over the `, hexed` that was typed.
    #[test]
    fn ghost_rows() {
        struct Fake;
        impl crate::shellsong::Candidates for Fake {
            fn corpus(&self, _token: &str) -> Vec<String> {
                vec!["flatline".to_string()]
            }
        }
        let build = |text: &str, back: usize| {
            let mut c = Console::empty();
            for ch in text.chars() {
                c.handle_key(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE), &Fake);
            }
            for _ in 0..back {
                c.handle_key(KeyEvent::new(KeyCode::Left, KeyModifiers::NONE), &Fake);
            }
            c
        };
        println!("\n== ghost placement (30 cells wide) ==");
        for (name, text, back) in [
            ("caret at end", "queue(fl", 0usize),
            ("caret mid-line", "queue(fl, hexed", 7),
        ] {
            let c = build(text, back);
            println!(
                "{name:<15} input={:?} cursor={} ghost={:?}",
                c.input, c.cursor, c.suggestion.ghost
            );
            let mut buf = buffer(30, 5);
            crate::ui::shellsong::ShellsongConsole {
                console: &c,
                cursor_visible: true,
            }
            .render(Rect::new(0, 0, 30, 5), &mut buf);
            let row = grid(&buf)
                .lines()
                .find(|r| r.contains(':'))
                .unwrap_or("")
                .to_string();
            println!("{:<15} overlay   |{row}|", "");
            let mut buf = buffer(30, 3);
            crate::swimconsole::SwimConsole {
                console: &c,
                cursor_visible: true,
            }
            .render(Rect::new(0, 0, 30, 3), &mut buf);
            let row = grid(&buf).lines().last().unwrap_or("").to_string();
            println!("{:<15} shellswim |{row}|", "");
        }
    }

    /// The contract the module docs state: what comes back out of a
    /// fit never paints wider than the budget it was given. Measured
    /// in a real buffer, because `width()` agreeing with itself is
    /// not the claim.
    #[test]
    fn the_painted_width_never_exceeds_the_budget() {
        for (name, s) in SAMPLES {
            for budget in 0..=14usize {
                for (how, out) in [
                    ("truncate_to_width", truncate_to_width(s, budget)),
                    ("truncate_with_ellipsis", truncate_with_ellipsis(s, budget)),
                    ("tail_fit", tail_fit(s, budget)),
                ] {
                    assert!(
                        width(&out) <= budget,
                        "{name}/{how}/{budget}: measured {} cells in {out:?}",
                        width(&out)
                    );
                    let (end, row, last) = painted(&out, budget);
                    assert!(
                        usize::from(end) <= budget,
                        "{name}/{how}/{budget}: draw::at returned {end}\n|{row}|"
                    );
                    if let Some(last) = last {
                        assert!(
                            usize::from(last) < budget.max(1),
                            "{name}/{how}/{budget}: painted column {last}\n|{row}|"
                        );
                    }
                }
            }
        }
    }
}

/// Timings, not assertions: `width` is on the per-frame path and a
/// grapheme walk costs several times a byte scan, so the ascii fast
/// path is load-bearing rather than tidy. Run with
/// `cargo test textfit::bench -- --ignored --nocapture`.
#[cfg(test)]
mod bench {
    use super::*;
    use crate::theme::{shade, NEAR_WHITE};
    use crate::ui::draw;
    use crate::ui::testbuf::buffer;
    use std::time::{Duration, Instant};

    /// Variants timed with their batches alternating, minimum each,
    /// the discipline `ui::player::bench` states: back-to-back runs on
    /// this machine came out 1.6x apart with other builds running,
    /// which is larger than the split being measured.
    fn best_each<const N: usize>(
        batches: u32,
        n: u32,
        mut f: impl FnMut(usize),
    ) -> [Duration; N] {
        let mut best = [Duration::MAX; N];
        for _ in 0..batches {
            for (i, slot) in best.iter_mut().enumerate() {
                let t = Instant::now();
                for _ in 0..n {
                    f(i);
                }
                *slot = (*slot).min(t.elapsed() / n);
            }
        }
        best
    }

    /// What a frame actually measures: the control labels and hints,
    /// a couple of real titles, and one cjk and one nfd title so the
    /// grapheme path is in the mix at about the rate a library puts it
    /// there.
    fn corpus() -> Vec<String> {
        [
            "shuffle",
            "loop all",
            "1.25x",
            "eq bass",
            "crossfade",
            "3:07 / 6:22",
            "j/k select, h/l adjust, enter to open",
            "Into Dust (Remastered) \u{b7} Mazzy Star",
            "\u{65E5}\u{672C}\u{8A9E}\u{306E}\u{30BF}\u{30A4}\u{30C8}\u{30EB}",
            "Beyonce\u{301} \u{b7} Renaissance",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect()
    }

    fn grapheme_width(s: &str) -> usize {
        clusters(s).map(cluster_cells).sum()
    }

    #[test]
    #[ignore = "timing, run explicitly"]
    fn width_cost() {
        let corpus = corpus();
        let [fast, graphemes, head] = best_each::<3>(8, 2000, |which| {
            for s in &corpus {
                std::hint::black_box(match which {
                    0 => width(s),
                    1 => grapheme_width(s),
                    _ => UnicodeWidthStr::width(s.as_str()),
                });
            }
        });
        println!(
            "width over {} strings: fast {fast:>10.3?}  graphemes {graphemes:>10.3?}  \
             head's UnicodeWidthStr {head:>10.3?}",
            corpus.len()
        );
    }

    #[test]
    #[ignore = "timing, run explicitly"]
    fn truncate_cost() {
        let corpus = corpus();
        let [fast, graphemes] = best_each::<2>(8, 2000, |which| {
            for s in &corpus {
                std::hint::black_box(match which {
                    0 => truncate_with_ellipsis(s, 20),
                    _ => {
                        if grapheme_width(s) <= 20 {
                            s.clone()
                        } else {
                            let mut out = String::new();
                            let mut used = 0;
                            for c in clusters(s) {
                                let cw = cluster_cells(c);
                                if cw == 0 {
                                    continue;
                                }
                                if used + cw > 19 {
                                    break;
                                }
                                out.push_str(c);
                                used += cw;
                            }
                            out.push('\u{2026}');
                            out
                        }
                    }
                });
            }
        });
        println!(
            "truncate_with_ellipsis over {} strings: fast {fast:>10.3?}  \
             graphemes {graphemes:>10.3?}",
            corpus.len()
        );
    }

    /// `draw::at` with and without the ascii branch, over the same
    /// rows: the one in the middle is what every widget calls.
    #[test]
    #[ignore = "timing, run explicitly"]
    fn draw_cost() {
        let corpus = corpus();
        let mut buf = buffer(200, 1);
        let color = shade(NEAR_WHITE);
        let [fast, graphemes] = best_each::<2>(8, 2000, |which| {
            for s in &corpus {
                match which {
                    0 => {
                        std::hint::black_box(draw::at(&mut buf, 0, 0, s, color, 60));
                    }
                    _ => {
                        let mut cursor = 0u16;
                        let mut left = 60usize;
                        for glyph in clusters(s) {
                            let cells = cluster_cells(glyph) as u16;
                            if cells == 0 {
                                continue;
                            }
                            if usize::from(cells) > left {
                                break;
                            }
                            left -= usize::from(cells);
                            if let Some(cell) = buf.cell_mut((cursor, 0u16)) {
                                cell.set_symbol(glyph);
                                cell.set_fg(color);
                            }
                            for off in 1..cells {
                                if let Some(cell) = buf.cell_mut((cursor + off, 0u16)) {
                                    cell.set_char(' ');
                                }
                            }
                            cursor += cells;
                        }
                        std::hint::black_box(cursor);
                    }
                }
            }
        });
        println!(
            "draw::at over {} rows: fast {fast:>10.3?}  graphemes {graphemes:>10.3?}",
            corpus.len()
        );
    }
}
