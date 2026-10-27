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

/// Returns the last `max_width` characters of `s`, or all of it if it
/// already fits.
pub fn tail_fit(s: &str, max_width: usize) -> String {
    let chars: Vec<char> = s.chars().collect();
    if chars.len() <= max_width {
        s.to_string()
    } else {
        chars[chars.len() - max_width..].iter().collect()
    }
}

/// Truncates `s` to `max_width` characters, replacing the last visible
/// character with `…` if anything was actually cut.
pub fn truncate_with_ellipsis(s: &str, max_width: usize) -> String {
    if max_width == 0 {
        return String::new();
    }
    let chars: Vec<char> = s.chars().collect();
    if chars.len() <= max_width {
        return s.to_string();
    }
    if max_width == 1 {
        return "…".to_string();
    }
    let mut out: String = chars[..max_width - 1].iter().collect();
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
    fn handles_multibyte_characters_by_count_not_bytes() {
        // "─" is 3 bytes in utf-8 but 1 char; must not panic/mis-slice.
        let s = "──────────"; // 10 chars, 30 bytes
        assert_eq!(tail_fit(s, 5).chars().count(), 5);
        assert_eq!(truncate_with_ellipsis(s, 5).chars().count(), 5);
    }
}
