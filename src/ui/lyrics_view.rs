use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::widgets::Widget;

use crate::theme::{shade, DIM, NEAR_BLACK, NEAR_WHITE};
use crate::ui::header::draw_header;

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
            for (col, ch) in line.chars().enumerate() {
                if col >= area.width as usize {
                    break;
                }
                if let Some(cell) = buf.cell_mut((area.x + col as u16, y)) {
                    cell.set_char(ch);
                    cell.set_fg(shade(NEAR_WHITE));
                }
            }
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
            let word_len = word.chars().count();
            let current_len = current.chars().count();
            if current.is_empty() {
                if word_len > width {
                    // longer than the whole pane; split it across rows,
                    // nothing else can be done with it.
                    let mut chunk = String::new();
                    for ch in word.chars() {
                        if chunk.chars().count() == width {
                            out.push(std::mem::take(&mut chunk));
                        }
                        chunk.push(ch);
                    }
                    current = chunk;
                } else {
                    current.push_str(word);
                }
            } else if current_len + 1 + word_len <= width {
                current.push(' ');
                current.push_str(word);
            } else {
                out.push(std::mem::take(&mut current));
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
