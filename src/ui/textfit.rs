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
