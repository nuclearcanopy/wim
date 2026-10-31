use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::widgets::Widget;

use crate::theme::{shade, DIM, NEAR_BLACK, NEAR_WHITE};
use crate::ui::header::draw_header;
use crate::ui::textfit;

/// Scrollable plain-text lyrics, no stock `Paragraph`/wrapping widget.
/// Headed by the track it belongs to: without that, a lyrics pane left
/// open across a track change gave no way to tell whose words were on
/// screen. Long lines wrap rather than being cut at the pane edge,
/// since a clipped lyric loses the half that rhymes.
pub struct LyricsView<'a> {
    pub lines: &'a [&'a str],
    pub scroll: usize,
    /// "title · artist" of the track these lyrics are for.
    pub track: &'a str,
}

impl Widget for LyricsView<'_> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        if area.height == 0 || area.width == 0 {
            return;
        }
        let body_top = draw_header(
            buf,
            area,
            area.y,
            self.track,
            "j/k: scroll, esc: close",
            shade(DIM),
            shade(NEAR_BLACK),
        );
        let body_height = (area.y + area.height).saturating_sub(body_top) as usize;
        if body_height == 0 {
            return;
        }

        let wrapped = wrap(self.lines, area.width as usize);
        for (row, line) in wrapped
            .iter()
            .skip(self.scroll)
            .take(body_height)
            .enumerate()
        {
            let y = body_top + row as u16;
            crate::ui::draw::line(buf, area, y, line, shade(NEAR_WHITE));
        }
    }
}

/// How many rows the lyrics occupy once wrapped to `width`, so the
/// caller can clamp its scroll offset instead of scrolling forever
/// into blank space.
pub fn wrapped_len(lines: &[&str], width: usize) -> usize {
    wrap(lines, width).len()
}

/// Word-wraps at `width`, keeping blank lines (verse breaks) and
/// hard-splitting a single over-long word rather than dropping it.
/// Measured in display cells (`textfit`), not chars: lyrics come from
/// lrclib as arbitrary utf-8, and a cjk line packed `width` *chars*
/// onto a row is twice that many cells, so `draw::line` clipped half
/// of every row away and `wrapped_len` under-counted the rows it took,
/// which also put the end of the song out of scroll range.
fn wrap(lines: &[&str], width: usize) -> Vec<String> {
    if width == 0 {
        return Vec::new();
    }
    let mut out = Vec::new();
    for line in lines {
        if line.trim().is_empty() {
            out.push(String::new());
            continue;
        }
        let mut current = String::new();
        for word in line.split_whitespace() {
            let word_cells = textfit::width(word);
            if !current.is_empty() && textfit::width(&current) + 1 + word_cells <= width {
                current.push(' ');
                current.push_str(word);
                continue;
            }
            if !current.is_empty() {
                out.push(std::mem::take(&mut current));
            }
            if word_cells > width {
                // longer than the whole pane; split it across rows,
                // nothing else can be done with it. reached from a
                // flushed row too, not only from the start of a line:
                // an over-long word mid-line used to skip the split and
                // overflow the pane on its own.
                let mut used = 0;
                for ch in word.chars() {
                    let cells = textfit::char_cells(ch);
                    // a wide glyph that would straddle the right edge
                    // starts the next row instead of being half-drawn;
                    // a pane too narrow for even one glyph still gets
                    // it rather than looping forever.
                    if used + cells > width && !current.is_empty() {
                        out.push(std::mem::take(&mut current));
                        used = 0;
                    }
                    current.push(ch);
                    used += cells;
                }
            } else {
                current.push_str(word);
            }
        }
        if !current.is_empty() {
            out.push(current);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_lines_are_left_alone() {
        let lines = ["i know", "you know"];
        assert_eq!(wrap(&lines, 20), vec!["i know", "you know"]);
    }

    #[test]
    fn long_lines_wrap_on_word_boundaries() {
        let lines = ["the quick brown fox jumps"];
        assert_eq!(wrap(&lines, 10), vec!["the quick", "brown fox", "jumps"]);
    }

    #[test]
    fn verse_breaks_survive_wrapping() {
        let lines = ["first verse", "", "second verse"];
        assert_eq!(wrap(&lines, 40), vec!["first verse", "", "second verse"]);
    }

    #[test]
    fn a_word_longer_than_the_pane_is_split_not_dropped() {
        let lines = ["aaaaaaaaaa"];
        assert_eq!(wrap(&lines, 4), vec!["aaaa", "aaaa", "aa"]);
    }

    /// Lyrics are whatever lrclib returns, and a japanese or korean
    /// song is two cells per char. Wrapped by char count these rows
    /// were up to twice the pane wide and `draw::line` clipped the
    /// half that rhymes.
    #[test]
    fn a_cjk_line_wraps_on_cells_not_chars() {
        // 6 cells of words either side of the space: by chars both fit
        // on one 8-cell row, by cells neither does.
        assert_eq!(
            wrap(&["日本語 テスト曲"], 8),
            vec!["日本語", "テスト曲"],
            "a char-counted wrap puts 15 cells on an 8-cell row"
        );
        // and a single word longer than the pane splits on the cell
        // that fits, never half a glyph.
        assert_eq!(wrap(&["日本語テスト"], 6), vec!["日本語", "テスト"]);

        // the invariant behind both: no row is wider than the pane,
        // whatever mixture of scripts and word lengths is on it. from
        // two cells up, since one cell cannot hold a wide glyph at all
        // and splitting further is not a thing wrapping can do.
        let song = [
            "息を止めて anata の 曲 forever",
            "日本語テスト曲日本語テスト曲",
        ];
        for w in 2..=20 {
            for row in wrap(&song, w) {
                assert!(
                    textfit::width(&row) <= w,
                    "{row:?} is {} cells on a {w}-cell pane",
                    textfit::width(&row)
                );
            }
        }
    }

    #[test]
    fn wrapped_len_counts_rows_after_wrapping_not_before() {
        let lines = ["the quick brown fox jumps"];
        assert_eq!(wrapped_len(&lines, 10), 3);
        assert_eq!(wrapped_len(&lines, 100), 1);
    }

    #[test]
    fn zero_width_is_not_a_panic() {
        assert!(wrap(&["anything"], 0).is_empty());
    }

    #[test]
    fn renders_at_degenerate_sizes_without_panicking() {
        for (w, h) in [(1, 1), (0, 5), (5, 0), (3, 2), (80, 1)] {
            let area = Rect::new(0, 0, w, h);
            let mut buf = Buffer::empty(area);
            LyricsView {
                lines: &["some words here", "", "more words"],
                scroll: 0,
                track: "Song · Artist",
            }
            .render(area, &mut buf);
        }
    }

    #[test]
    fn scrolling_past_the_end_draws_nothing_rather_than_panicking() {
        let area = Rect::new(0, 0, 20, 6);
        let mut buf = Buffer::empty(area);
        LyricsView {
            lines: &["a", "b"],
            scroll: 999,
            track: "Song · Artist",
        }
        .render(area, &mut buf);
    }

    #[test]
    fn the_track_name_is_in_the_header() {
        let area = Rect::new(0, 0, 60, 8);
        let mut buf = Buffer::empty(area);
        LyricsView {
            lines: &["words"],
            scroll: 0,
            track: "Crest · Bladee",
        }
        .render(area, &mut buf);
        let top: String = (0..60)
            .map(|x| buf.cell((x, 0)).unwrap().symbol().to_string())
            .collect();
        assert!(top.contains("Crest · Bladee"), "got: {top}");
    }
}
