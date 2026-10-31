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

use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

/// Display width of `s` in terminal cells.
pub fn width(s: &str) -> usize {
    UnicodeWidthStr::width(s)
}

/// Terminal cells one char occupies; a control or zero-width char
/// counts as 0. For drawing loops that advance a cursor per char: a
/// `x += 1` there is only correct for ascii, and runs a cell behind
/// the terminal for every wide glyph in the string.
pub fn char_cells(ch: char) -> usize {
    UnicodeWidthChar::width(ch).unwrap_or(0)
}

/// Longest prefix of `s` that fits in `max_width` cells. A
/// double-width char that would straddle the right edge is dropped
/// rather than half-drawn, so the result is never *wider* than asked
/// for (it can be one cell narrower).
pub fn truncate_to_width(s: &str, max_width: usize) -> String {
    let mut out = String::new();
    let mut used = 0;
    for ch in s.chars() {
        let cw = char_cells(ch);
        if used + cw > max_width {
            break;
        }
        out.push(ch);
        used += cw;
    }
    out
}

/// Returns the last `max_width` cells' worth of `s`, or all of it if
/// it already fits.
pub fn tail_fit(s: &str, max_width: usize) -> String {
    if width(s) <= max_width {
        return s.to_string();
    }
    let mut kept: Vec<char> = Vec::new();
    let mut used = 0;
    for ch in s.chars().rev() {
        let cw = char_cells(ch);
        if used + cw > max_width {
            break;
        }
        kept.push(ch);
        used += cw;
    }
    kept.into_iter().rev().collect()
}

/// Truncates `s` to `max_width` cells, replacing the last visible
/// character with `…` (itself one cell) if anything was actually cut.
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
