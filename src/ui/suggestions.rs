use std::time::Duration;

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::widgets::Widget;

use crate::anim;
use crate::suggest::{Reason, Suggestions};
use crate::theme::{shade, BRIGHT, DIM, MID, NEAR_BLACK, NEAR_WHITE};
use crate::ui::draw;
use crate::ui::header::draw_header;
use crate::ui::results_list::{fill_row_bg, label};
use crate::ui::scroll;
use crate::ui::textfit::truncate_with_ellipsis;

/// The suggestions menu: grouped, labelled sections with the reason
/// each row fired printed under it.
///
/// Showing the reason is not decoration. A recommendation with no
/// stated cause is indistinguishable from a random row, and the engine
/// already knows exactly why it fired ("4 of 5 tracks played from
/// Eversince, never this one"), so withholding it would throw away the
/// one thing that makes the feature feel deliberate. Each suggestion
/// is therefore two rows, and the reason brightens with its row so
/// it's legible for the one you're actually on.
pub struct SuggestionsView<'a> {
    pub suggestions: &'a Suggestions,
    /// Index into the flattened suggestion list, not into display rows.
    pub selected: usize,
    pub focused: bool,
    pub elapsed: Duration,
    /// True while the background enrichment fetches are still in
    /// flight, so the footer can say the list is still filling out
    /// rather than letting a thin list look like the whole answer.
    pub enriching: bool,
}

/// One drawn row. A suggestion occupies two (`Item` then `Reason`), so
/// the selection plate can span both and read as a single unit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Row {
    /// Blank spacer above a section heading; sections need air between
    /// them or the whole menu reads as one undifferentiated list.
    Gap,
    Section(Reason),
    Item(usize),
    Reason(usize),
}

fn rows(s: &Suggestions) -> Vec<Row> {
    let mut out = Vec::new();
    let mut flat = 0usize;
    for (i, section) in s.sections.iter().enumerate() {
        if i > 0 {
            out.push(Row::Gap);
        }
        out.push(Row::Section(section.reason));
        for _ in &section.items {
            out.push(Row::Item(flat));
            out.push(Row::Reason(flat));
            flat += 1;
        }
    }
    out
}

/// Which drawn row holds suggestion `selected`, so the scroll offset
/// follows the cursor without a heading ever being selectable.
fn row_of(rows: &[Row], selected: usize) -> usize {
    rows.iter()
        .position(|r| matches!(r, Row::Item(i) if *i == selected))
        .unwrap_or(0)
}

/// How many suggestions there are to move between; the caller clamps
/// its cursor against this rather than against the drawn row count.
pub fn count(s: &Suggestions) -> usize {
    s.len()
}

impl Widget for SuggestionsView<'_> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        if area.width == 0 || area.height == 0 {
            return;
        }
        let total = self.suggestions.len();
        let title = if self.enriching {
            format!("suggestions · {total} so far")
        } else {
            format!("suggestions · {total}")
        };
        let body_top = draw_header(
            buf,
            area,
            area.y,
            &title,
            "enter: play, esc: close",
            shade(DIM),
            shade(NEAR_BLACK),
        );
        let body_height = (area.y + area.height).saturating_sub(body_top) as usize;
        if body_height == 0 {
            return;
        }

        if self.suggestions.is_empty() {
            // a brand new library genuinely has nothing to go on, and
            // saying so beats an empty pane that looks broken.
            draw::line(buf, area, body_top, "nothing to suggest yet", shade(MID));
            let why = if self.enriching {
                "still reading your library"
            } else {
                "play a few things and this fills in"
            };
            draw::line(buf, area, body_top.saturating_add(1), why, shade(DIM));
            return;
        }

        let rows = rows(self.suggestions);
        let offset = scroll::offset_for(row_of(&rows, self.selected), body_height);
        let flat: Vec<_> = self.suggestions.flat().collect();

        for (i, row) in rows.iter().skip(offset).take(body_height).enumerate() {
            let y = body_top + i as u16;
            match row {
                Row::Gap => {}
                Row::Section(reason) => {
                    draw::line(buf, area, y, &format!(" {}", reason.title()), shade(MID));
                }
                Row::Item(idx) | Row::Reason(idx) => {
                    let Some((_, suggestion)) = flat.get(*idx) else {
                        continue;
                    };
                    let is_selected = *idx == self.selected;
                    if is_selected {
                        // the plate covers both of a suggestion's rows,
                        // so the title and its reason read as one block
                        // rather than two neighbouring entries.
                        fill_row_bg(buf, area, y, anim::selection_bg(self.elapsed, self.focused));
                    }
                    let text = match row {
                        Row::Item(_) => {
                            format!(
                                "{}{}",
                                if is_selected { "▌ " } else { "  " },
                                label(&suggestion.item)
                            )
                        }
                        // the reason is indented past the title it
                        // explains, so the eye reads title-then-cause.
                        _ => format!("      {}", suggestion.reason_text),
                    };
                    let level = match (row, is_selected) {
                        (Row::Item(_), true) => BRIGHT,
                        (Row::Item(_), false) => DIM,
                        // only legible on the row you're on: every
                        // reason at full brightness would compete with
                        // the titles for attention.
                        (_, true) => NEAR_WHITE * 0.6,
                        (_, false) => NEAR_BLACK,
                    };
                    let fitted = truncate_with_ellipsis(&text, area.width as usize);
                    draw::line(buf, area, y, &fitted, shade(level));
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::search::SearchItem;
    use crate::suggest::{Section, Suggestion};
    use crate::ui::testbuf::{buffer, grid};

    fn song(title: &str) -> SearchItem {
        SearchItem::LocalMusic {
            path: format!("/m/{title}.flac").into(),
            title: title.to_string(),
            artist: Some("Bladee".to_string()),
            album: None,
        }
    }

    fn suggestion(title: &str, reason: &str) -> Suggestion {
        Suggestion {
            item: song(title),
            reason_text: reason.to_string(),
            score: 0.5,
        }
    }

    fn two_sections() -> Suggestions {
        Suggestions {
            sections: vec![
                Section {
                    title: Reason::DailyRotation.title().to_string(),
                    reason: Reason::DailyRotation,
                    items: vec![suggestion("Obedient", "played on 10 of the last 10 days")],
                },
                Section {
                    title: Reason::AlbumGap.title().to_string(),
                    reason: Reason::AlbumGap,
                    items: vec![suggestion("Crest", "28 plays, nothing from this album")],
                },
            ],
        }
    }

    fn render(s: &Suggestions, selected: usize, w: u16, h: u16) -> String {
        let area = Rect::new(0, 0, w, h);
        let mut buf = buffer(w, h);
        SuggestionsView {
            suggestions: s,
            selected,
            focused: true,
            elapsed: Duration::ZERO,
            enriching: false,
        }
        .render(area, &mut buf);
        grid(&buf)
    }

    #[test]
    fn every_row_is_followed_by_its_reason() {
        let s = two_sections();
        let r = rows(&s);
        assert_eq!(
            r,
            vec![
                Row::Section(Reason::DailyRotation),
                Row::Item(0),
                Row::Reason(0),
                Row::Gap,
                Row::Section(Reason::AlbumGap),
                Row::Item(1),
                Row::Reason(1),
            ]
        );
    }

    #[test]
    fn the_reason_is_actually_rendered_not_just_computed() {
        let out = render(&two_sections(), 0, 60, 12);
        assert!(
            out.contains("played on 10 of the last 10 days"),
            "the explanation is the feature:\n{out}"
        );
        assert!(out.contains("28 plays, nothing from this album"), "{out}");
    }

    #[test]
    fn section_headings_are_present_and_separated() {
        let out = render(&two_sections(), 0, 60, 12);
        assert!(out.contains("your daily rotation"), "{out}");
        assert!(out.contains("albums you have never opened"), "{out}");
    }

    #[test]
    fn the_selected_suggestion_plates_both_of_its_rows() {
        let s = two_sections();
        let area = Rect::new(0, 0, 60, 12);
        let mut buf = buffer(60, 12);
        SuggestionsView {
            suggestions: &s,
            selected: 0,
            focused: true,
            elapsed: Duration::ZERO,
            enriching: false,
        }
        .render(area, &mut buf);
        let plated: Vec<u16> = (0..12)
            .filter(|y| {
                buf.cell((0, *y))
                    .is_some_and(|c| c.bg != ratatui::style::Color::Reset)
            })
            .collect();
        assert_eq!(
            plated.len(),
            2,
            "title and reason are one unit, so both get the plate"
        );
        assert_eq!(plated[1], plated[0] + 1, "and they are adjacent");
    }

    #[test]
    fn only_the_selected_suggestion_is_plated() {
        let s = two_sections();
        let area = Rect::new(0, 0, 60, 12);
        let mut buf = buffer(60, 12);
        SuggestionsView {
            suggestions: &s,
            selected: 1,
            focused: true,
            elapsed: Duration::ZERO,
            enriching: false,
        }
        .render(area, &mut buf);
        let plated: Vec<u16> = (0..12)
            .filter(|y| {
                buf.cell((0, *y))
                    .is_some_and(|c| c.bg != ratatui::style::Color::Reset)
            })
            .collect();
        assert_eq!(plated.len(), 2);
        // the second suggestion sits below a gap and a heading
        let row: String = (0..60)
            .map(|x| buf.cell((x, plated[0])).unwrap().symbol().to_string())
            .collect();
        assert!(row.contains("Crest"), "plated the wrong row: {row}");
    }

    #[test]
    fn an_empty_result_explains_itself() {
        let out = render(&Suggestions::default(), 0, 50, 8);
        assert!(out.contains("nothing to suggest yet"), "{out}");
        assert!(out.contains("play a few things"), "{out}");
    }

    #[test]
    fn a_still_filling_list_says_so_in_the_title() {
        let s = two_sections();
        let area = Rect::new(0, 0, 60, 12);
        let mut buf = buffer(60, 12);
        SuggestionsView {
            suggestions: &s,
            selected: 0,
            focused: true,
            elapsed: Duration::ZERO,
            enriching: true,
        }
        .render(area, &mut buf);
        let g = grid(&buf);
        assert!(
            g.contains("so far"),
            "a thin list must not look final:\n{g}"
        );
    }

    #[test]
    fn scrolls_to_keep_a_far_selection_on_screen() {
        let many: Vec<_> = (0..30)
            .map(|i| suggestion(&format!("t{i}"), &format!("reason {i}")))
            .collect();
        let s = Suggestions {
            sections: vec![Section {
                title: Reason::DailyRotation.title().to_string(),
                reason: Reason::DailyRotation,
                items: many,
            }],
        };
        let out = render(&s, 27, 60, 10);
        assert!(out.contains("t27"), "selection must be visible:\n{out}");
    }

    #[test]
    fn renders_at_degenerate_sizes_without_panicking() {
        let s = two_sections();
        for (w, h) in [(1, 1), (0, 5), (5, 0), (3, 2), (80, 1), (80, 2)] {
            let area = Rect::new(0, 0, w, h);
            let mut buf = buffer(w.max(1), h.max(1));
            SuggestionsView {
                suggestions: &s,
                selected: 0,
                focused: false,
                elapsed: Duration::ZERO,
                enriching: false,
            }
            .render(area, &mut buf);
        }
    }
}
