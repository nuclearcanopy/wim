//! Where two hand-drawn lines meet. Nothing here boxes a region with
//! a stock widget, so every corner and tee has to be derived from
//! which arms actually have a line running into them; a `┤` on a row
//! where the rule in fact continues to the right (or a `┬` on a row
//! where nothing is to the right at all) is exactly how the layout
//! reads as broken.

use ratatui::layout::Rect;
use ratatui::style::Color;

/// Which of the four directions around one cell have a line in them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Arms {
    pub up: bool,
    pub down: bool,
    pub left: bool,
    pub right: bool,
}

/// The one light box-drawing glyph that matches `arms`. Fewer than
/// two arms isn't a joint at all, so it falls back to the straight
/// line being continued (`│` for a vertical stub, `─` otherwise).
pub fn glyph(arms: Arms) -> char {
    match (arms.up, arms.down, arms.left, arms.right) {
        (true, true, true, true) => '┼',
        (true, true, true, false) => '┤',
        (true, true, false, true) => '├',
        (true, false, true, true) => '┴',
        (false, true, true, true) => '┬',
        (true, false, true, false) => '┘',
        (true, false, false, true) => '└',
        (false, true, true, false) => '┐',
        (false, true, false, true) => '┌',
        (true, _, false, false) | (false, true, false, false) => '│',
        _ => '─',
    }
}

/// Junction glyphs for a vertical divider whose stroke runs from row
/// `top` to row `bottom` inclusive, given the rows where a horizontal
/// rule reaches it from the left (the sidebar's search box and
/// section headers) and from the right (the browse view's header
/// rules, the mini player's strip). `bottom` is where the vertical
/// genuinely ends, which may be one row past the divider's own rect
/// when something else draws the terminating `┴` there; rows outside
/// the rect are simply never looked up.
///
/// Rows with no rule on either side are left out: the divider already
/// draws its plain `│` on those.
pub fn vertical_divider_junctions(
    top: u16,
    bottom: u16,
    left_rules: &[u16],
    right_rules: &[u16],
    color: Color,
) -> Vec<(u16, char, Color)> {
    let mut rows: Vec<u16> = left_rules
        .iter()
        .chain(right_rules.iter())
        .copied()
        .filter(|y| (top..=bottom).contains(y))
        .collect();
    rows.sort_unstable();
    rows.dedup();
    rows.into_iter()
        .map(|y| {
            let arms = Arms {
                up: y > top,
                down: y < bottom,
                left: left_rules.contains(&y),
                right: right_rules.contains(&y),
            };
            (y, glyph(arms), color)
        })
        .collect()
}

/// One frame's worth of what reaches the sidebar/main-content
/// divider. From the left: the search box's own two border rows (it
/// draws no right edge, this divider is its right edge) plus every
/// results-list section header, itself a full-width rule. From the
/// right: whatever main content draws at its own left edge, which
/// depends on what's open there (see `ui::browse::rule_rows`; the
/// player, lyrics and banner draw nothing). The stroke ends on the
/// bottom bar's own rule row, which draws the terminating `┴`
/// itself, so every row above it still continues downward.
pub struct SidebarDivider<'a> {
    pub search: Rect,
    pub results_header_rows: &'a [u16],
    pub main_content_rule_rows: &'a [u16],
    pub bottom_rule_row: u16,
}

impl SidebarDivider<'_> {
    pub fn junctions(&self, color: Color) -> Vec<(u16, char, Color)> {
        let mut left = vec![
            self.search.y,
            self.search.y + self.search.height.saturating_sub(1),
        ];
        left.extend_from_slice(self.results_header_rows);
        vertical_divider_junctions(
            self.search.y,
            self.bottom_rule_row,
            &left,
            self.main_content_rule_rows,
            color,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::theme::{shade, NEAR_BLACK};

    fn chars(junctions: &[(u16, char, Color)]) -> Vec<(u16, char)> {
        junctions.iter().map(|(y, ch, _)| (*y, *ch)).collect()
    }

    #[test]
    fn glyph_covers_every_arm_combination() {
        let cases = [
            ((true, true, true, true), '┼'),
            ((true, true, true, false), '┤'),
            ((true, true, false, true), '├'),
            ((true, false, true, true), '┴'),
            ((false, true, true, true), '┬'),
            ((true, false, true, false), '┘'),
            ((true, false, false, true), '└'),
            ((false, true, true, false), '┐'),
            ((false, true, false, true), '┌'),
            ((true, true, false, false), '│'),
            ((true, false, false, false), '│'),
            ((false, true, false, false), '│'),
            ((false, false, true, true), '─'),
            ((false, false, true, false), '─'),
            ((false, false, false, true), '─'),
        ];
        for ((up, down, left, right), expected) in cases {
            let arms = Arms {
                up,
                down,
                left,
                right,
            };
            assert_eq!(glyph(arms), expected, "wrong glyph for {arms:?}");
        }
    }

    /// Blank home screen: the search box's two border rows reach the
    /// divider from the left, nothing from the right. The top row must
    /// close the box (┐), not point into empty space (┬).
    #[test]
    fn blank_home_screen_closes_the_search_box_instead_of_trailing_off() {
        let j = vertical_divider_junctions(0, 20, &[0, 2], &[], shade(NEAR_BLACK));
        assert_eq!(chars(&j), vec![(0, '┐'), (2, '┤')]);
    }

    /// Browse view open: its leading rule shares the search box's top
    /// row and its title rule shares the box's bottom row, so both
    /// cross straight through.
    #[test]
    fn browse_view_open_crosses_both_shared_rows() {
        let j = vertical_divider_junctions(0, 20, &[0, 2], &[0, 2], shade(NEAR_BLACK));
        assert_eq!(chars(&j), vec![(0, '┬'), (2, '┼')]);
    }

    /// A rule only on the right (the mini player strip, which spans
    /// main content's width and never the sidebar's).
    #[test]
    fn right_only_rule_tees_into_the_divider() {
        let j = vertical_divider_junctions(0, 30, &[], &[25], shade(NEAR_BLACK));
        assert_eq!(chars(&j), vec![(25, '├')]);
    }

    #[test]
    fn rule_on_the_last_row_of_the_stroke_terminates_the_divider() {
        let j = vertical_divider_junctions(0, 10, &[10], &[], shade(NEAR_BLACK));
        assert_eq!(chars(&j), vec![(10, '┘')]);
    }

    #[test]
    fn rules_outside_the_stroke_are_ignored() {
        let j = vertical_divider_junctions(5, 10, &[0, 7, 40], &[], shade(NEAR_BLACK));
        assert_eq!(chars(&j), vec![(7, '┤')]);
    }

    #[test]
    fn duplicate_rows_yield_one_junction() {
        let j = vertical_divider_junctions(0, 10, &[4, 4], &[4], shade(NEAR_BLACK));
        assert_eq!(chars(&j), vec![(4, '┼')]);
    }

    /// The real frame geometry: search box at rows 0..=2, blank gap
    /// row 3, results list from row 4, bottom bar rule at row 20.
    fn frame(results_header_rows: &[u16], main_content_rule_rows: &[u16]) -> Vec<(u16, char)> {
        chars(
            &SidebarDivider {
                search: Rect::new(0, 0, 20, 3),
                results_header_rows,
                main_content_rule_rows,
                bottom_rule_row: 20,
            }
            .junctions(shade(NEAR_BLACK)),
        )
    }

    /// Blank home screen: nothing is open in main content, so the
    /// search box's top border must be closed off (┐) rather than
    /// teeing into a rule that doesn't exist.
    #[test]
    fn home_screen_without_results_closes_the_box() {
        assert_eq!(frame(&[], &[]), vec![(0, '┐'), (2, '┤')]);
    }

    #[test]
    fn home_screen_with_results_tees_each_section_header() {
        assert_eq!(
            frame(&[4, 9], &[]),
            vec![(0, '┐'), (2, '┤'), (4, '┤'), (9, '┤')]
        );
    }

    /// Album/playlist page: its leading rule shares row 0 with the
    /// search box's top border and its title rule shares row 2 with
    /// the box's bottom border, so both cross straight through.
    #[test]
    fn album_page_crosses_the_search_box_borders() {
        assert_eq!(frame(&[], &[0, 2]), vec![(0, '┬'), (2, '┼')]);
    }

    /// Artist page: same two header rules, plus the ALBUMS/SONGS
    /// column labels' own rule on row 4, which here also happens to be
    /// where the results list's first section header lands.
    #[test]
    fn artist_page_columns_rule_crosses_a_section_header() {
        assert_eq!(frame(&[4], &[0, 2, 4]), vec![(0, '┬'), (2, '┼'), (4, '┼')]);
    }

    #[test]
    fn artist_page_columns_rule_alone_tees_from_the_right() {
        assert_eq!(
            frame(&[9], &[0, 2, 4]),
            vec![(0, '┬'), (2, '┼'), (4, '├'), (9, '┤')]
        );
    }

    /// Lyrics and the maximized player draw nothing at main content's
    /// left edge, so the sidebar is on its own again.
    #[test]
    fn lyrics_or_player_open_is_the_same_as_the_home_screen() {
        assert_eq!(frame(&[4], &[]), vec![(0, '┐'), (2, '┤'), (4, '┤')]);
    }

    /// Mini player strip showing: its own rule spans main content's
    /// width only, so the divider tees right (├), never ┼.
    #[test]
    fn mini_player_strip_tees_right_only() {
        assert_eq!(
            frame(&[4], &[17]),
            vec![(0, '┐'), (2, '┤'), (4, '┤'), (17, '├')]
        );
    }

    /// The whole blank-home-screen sidebar drawn by the real widgets:
    /// search box, one blank gap row (not a second rule), the results
    /// list's own section-header rule, and the bottom bar's rule, with
    /// the divider closing every one of them. '.' is a cell nothing
    /// drew into.
    #[test]
    fn blank_home_screen_sidebar_renders_connected() {
        use crate::favorites::Favorites;
        use crate::search::SearchItem;
        use crate::ui::divider::{HorizontalRule, VerticalRule};
        use crate::ui::results_list::{header_rows, ResultsList};
        use crate::ui::searchbox::SearchBox;
        use crate::ui::testbuf::{buffer, grid};
        use ratatui::widgets::Widget;
        use std::collections::HashSet;

        let search = Rect::new(0, 0, 16, 3);
        let results = Rect::new(0, 4, 16, 3);
        let bottom_rule_row = 7;
        let items = vec![SearchItem::LocalMusic {
            path: "/m/t.flac".into(),
            title: "t".to_string(),
            artist: None,
            album: None,
        }];
        let favorites = Favorites::default();
        let starred = HashSet::new();

        let mut buf = buffer(24, 8);
        SearchBox {
            query: "",
            editing: false,
            label: "search",
            cursor_visible: false,
            right_border: false,
        }
        .render(search, &mut buf);
        ResultsList {
            items: &items,
            selected: 0,
            empty_message: "",
            favorites: &favorites,
            remote_starred: &starred,
        }
        .render(results, &mut buf);
        VerticalRule {
            highlighted: false,
            junction_rows: SidebarDivider {
                search,
                results_header_rows: &header_rows(&items, 0, results),
                main_content_rule_rows: &[],
                bottom_rule_row,
            }
            .junctions(shade(NEAR_BLACK)),
        }
        .render(Rect::new(16, 0, 1, bottom_rule_row), &mut buf);
        HorizontalRule {
            highlighted: false,
            junctions: vec![(16, '┴')],
        }
        .render(Rect::new(0, bottom_rule_row, 24, 1), &mut buf);

        assert_eq!(
            grid(&buf),
            [
                "┌┤ search ├─────┐.......",
                "│search.........│.......",
                "└───────────────┤.......",
                "................│.......",
                "──── SONGS ─────┤.......",
                "> [fs] t........│.......",
                "................│.......",
                "────────────────┴───────",
            ]
            .join("\n")
        );
    }
}
