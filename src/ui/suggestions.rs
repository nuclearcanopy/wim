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

/// Rows `draw_header` eats above the body: its text line plus the rule
/// under it. Hardcoded here because the body's y is needed without
/// drawing; it must match what `ui::header::draw_header` returns.
/// The rect the suggestion rows are drawn into, so a hit map knows which
/// absolute row is row 0 of `row_items` (see `hit::List`). The header's
/// height comes from `ui::header`, which is also where it is checked
/// against what `draw_header` actually returns.
pub fn body_rect(area: Rect) -> Rect {
    crate::ui::header::body(area)
}

/// Suggestion index per visible body row, `None` on a gap, a heading or
/// past the end; always exactly `body_rect`'s height long, so the
/// caller can index it by row. Same `rows` and offset the render itself
/// uses, so a click resolves to the row the user saw rather than to a
/// second guess at where it landed.
pub fn row_items(suggestions: &Suggestions, selected: usize, area: Rect) -> Vec<Option<usize>> {
    // nothing to suggest draws two message rows and no suggestions, so
    // there is nothing on it to click.
    if area.width == 0 || suggestions.is_empty() {
        return Vec::new();
    }
    let body_height = body_rect(area).height as usize;
    if body_height == 0 {
        return Vec::new();
    }
    let rows = rows(suggestions);
    let offset = scroll::offset_for(row_of(&rows, selected), body_height);
    (0..body_height)
        .map(|row| match rows.get(offset + row) {
            // the reason line is part of the suggestion above it (the
            // plate already spans both), so either row picks it.
            Some(Row::Item(i) | Row::Reason(i)) => Some(*i),
            Some(Row::Gap | Row::Section(_)) | None => None,
        })
        .collect()
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
        // one derivation of the body geometry, shared with `row_items`.
        let body = body_rect(area);
        debug_assert_eq!(body_top, body.y, "ui::header::ROWS is out of step");
        let body_height = body.height as usize;
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
                    // `rows` numbers its items by walking the same
                    // sections in the same order `flat` does, so every
                    // index it emits is in range; the test
                    // `rows_numbers_every_suggestion_once_in_flat_order`
                    // is the proof. the `.get(..) else continue` that
                    // used to stand here could not fire, and if it ever
                    // had it would have skipped a row `row_items` still
                    // named, which is a click landing on nothing drawn.
                    let (_, suggestion) = flat[*idx];
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
    use crate::ui::testbuf::{buffer, char_at, grid};

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

    /// Sections of uneven length, including an empty one, so the
    /// numbering below is tested across section boundaries rather than
    /// inside one list.
    fn uneven_sections() -> Suggestions {
        let items = |n: usize, tag: &str| {
            (0..n)
                .map(|i| suggestion(&format!("{tag}{i}"), &format!("because {tag}{i}")))
                .collect::<Vec<_>>()
        };
        Suggestions {
            sections: vec![
                Section {
                    title: Reason::DailyRotation.title().to_string(),
                    reason: Reason::DailyRotation,
                    items: items(2, "a"),
                },
                Section {
                    title: Reason::AlbumGap.title().to_string(),
                    reason: Reason::AlbumGap,
                    items: Vec::new(),
                },
                Section {
                    title: Reason::Rediscovery.title().to_string(),
                    reason: Reason::Rediscovery,
                    items: items(3, "c"),
                },
            ],
        }
    }

    /// Why `render` may index `flat` directly. `rows` numbers its items
    /// by walking the same sections in the same order `flat` does, so
    /// the indices it hands out are exactly `0..flat().count()`, each
    /// once, ascending. Without this the render's old
    /// `flat.get(idx) else continue` would have been a row the map
    /// still named: a click resolving to a suggestion nothing drew.
    #[test]
    fn rows_numbers_every_suggestion_once_in_flat_order() {
        for s in [
            Suggestions::default(),
            two_sections(),
            one_long_section(),
            uneven_sections(),
        ] {
            let flat = s.flat().count();
            assert_eq!(flat, s.len(), "count() clamps against flat()'s length");
            let picked = |want_item: bool| -> Vec<usize> {
                rows(&s)
                    .into_iter()
                    .filter_map(|r| match r {
                        Row::Item(i) if want_item => Some(i),
                        Row::Reason(i) if !want_item => Some(i),
                        _ => None,
                    })
                    .collect()
            };
            let expect: Vec<usize> = (0..flat).collect();
            assert_eq!(picked(true), expect, "title rows");
            // the reason rows repeat the same numbering, which is what
            // lets either row of a suggestion pick it.
            assert_eq!(picked(false), expect, "reason rows");
        }
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

    /// Thirty suggestions in one section, named so that no title or
    /// reason is a substring of another's (`t2` inside `t27` would let a
    /// wrong answer pass the painted-text check below).
    fn one_long_section() -> Suggestions {
        Suggestions {
            sections: vec![Section {
                title: Reason::DailyRotation.title().to_string(),
                reason: Reason::DailyRotation,
                items: (0..30)
                    .map(|i| suggestion(&format!("t{i}"), &format!("because {i} fired")))
                    .collect(),
            }],
        }
    }

    /// What `row_items` claims for each body row, paired with the text
    /// `render` actually painted on that same row.
    fn painted(s: &Suggestions, selected: usize, w: u16, h: u16) -> Vec<(Option<usize>, String)> {
        let area = Rect::new(0, 0, w, h);
        let mut buf = buffer(w.max(1), h.max(1));
        SuggestionsView {
            suggestions: s,
            selected,
            focused: true,
            elapsed: Duration::ZERO,
            enriching: false,
        }
        .render(area, &mut buf);
        let lines: Vec<String> = grid(&buf).lines().map(str::to_string).collect();
        row_items(s, selected, area)
            .into_iter()
            .enumerate()
            .map(|(i, item)| (item, lines[crate::ui::header::ROWS as usize + i].clone()))
            .collect()
    }

    /// Every body row's answer is the suggestion whose title or reason
    /// is on it; a `None` row has to be a heading or untouched fill (a
    /// gap paints nothing), never a suggestion the map failed to name.
    fn assert_agrees(s: &Suggestions, selected: usize, w: u16, h: u16) {
        let flat: Vec<_> = s.flat().collect();
        for (i, (item, text)) in painted(s, selected, w, h).iter().enumerate() {
            match item {
                Some(idx) => {
                    let (_, sug) = flat[*idx];
                    assert!(
                        text.contains(&label(&sug.item)) || text.contains(&sug.reason_text),
                        "body row {i} claims suggestion {idx} but painted {text:?}"
                    );
                }
                None => {
                    let blank = text.chars().all(|c| c == '.');
                    let heading = s
                        .sections
                        .iter()
                        .any(|sec| text.contains(sec.reason.title()));
                    assert!(
                        blank || heading,
                        "body row {i} claims no suggestion but painted {text:?}"
                    );
                }
            }
        }
    }

    #[test]
    fn row_items_names_the_suggestion_render_painted_on_each_row() {
        let s = two_sections();
        assert_eq!(
            row_items(&s, 0, Rect::new(0, 0, 60, 12)),
            vec![
                None, // heading
                Some(0),
                Some(0), // the reason line belongs to the title above it
                None,    // gap
                None,    // heading
                Some(1),
                Some(1),
                // past the end of the list
                None,
                None,
                None,
            ],
            "ten body rows under a two-row header"
        );
        assert_agrees(&s, 0, 60, 12);
    }

    #[test]
    fn row_items_follows_the_scroll_offset() {
        let s = one_long_section();
        // a window this far down holds no heading and no gap, and it
        // opens on a reason line rather than a title, which an offset of
        // 0 would get wrong in both directions.
        assert_eq!(
            row_items(&s, 27, Rect::new(0, 0, 60, 10)),
            vec![
                Some(23),
                Some(24),
                Some(24),
                Some(25),
                Some(25),
                Some(26),
                Some(26),
                Some(27),
            ]
        );
        assert_agrees(&s, 27, 60, 10);

        // and with the selection on the very last suggestion.
        assert_eq!(
            row_items(&s, 29, Rect::new(0, 0, 60, 10)),
            vec![
                Some(25),
                Some(26),
                Some(26),
                Some(27),
                Some(27),
                Some(28),
                Some(28),
                Some(29),
            ]
        );
        assert_agrees(&s, 29, 60, 10);
    }

    /// Every selection, at sizes that scroll and sizes that do not, on
    /// a fixture with gaps, headings and an empty section: the map's
    /// answer for a row is the suggestion whose own text landed there.
    #[test]
    fn the_map_and_the_render_agree_row_for_row_across_sections() {
        let s = uneven_sections();
        for selected in 0..s.len() {
            for (w, h) in [(60u16, 20u16), (60, 12), (60, 8), (40, 6), (60, 5)] {
                assert_agrees(&s, selected, w, h);
            }
        }
    }

    #[test]
    fn the_body_rect_is_where_render_starts_drawing_rows() {
        let s = two_sections();
        let area = Rect::new(2, 1, 50, 10);
        let mut buf = buffer(60, 12);
        SuggestionsView {
            suggestions: &s,
            selected: 0,
            focused: true,
            elapsed: Duration::ZERO,
            enriching: false,
        }
        .render(area, &mut buf);
        let body = body_rect(area);
        assert_eq!((body.x, body.y, body.width, body.height), (2, 3, 50, 8));
        let row: String = (body.x..body.x + body.width)
            .map(|x| char_at(&buf, x, body.y))
            .collect();
        assert!(
            row.contains("your daily rotation"),
            "row 0 of the body rect is the first row render drew: {row}"
        );
    }

    #[test]
    fn row_items_at_degenerate_sizes_is_empty_or_exactly_the_body() {
        let s = two_sections();
        for (w, h) in [(1, 1), (0, 5), (5, 0), (3, 2), (80, 1), (80, 2), (80, 3)] {
            let area = Rect::new(0, 0, w, h);
            let map = row_items(&s, 0, area);
            let body = body_rect(area).height as usize;
            assert!(
                map.is_empty() || map.len() == body,
                "{w}x{h}: {} answers for a {body}-row body",
                map.len()
            );
            assert_agrees(&s, 0, w, h);
        }
        // an empty result draws messages, not suggestions, so no row on
        // it is clickable.
        assert!(row_items(&Suggestions::default(), 0, Rect::new(0, 0, 50, 8)).is_empty());
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
