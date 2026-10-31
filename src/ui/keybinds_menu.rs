use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::widgets::Widget;

use crate::keymap::ALL_ACTIONS;
use crate::theme::{shade, DIM, NEAR_BLACK, NEAR_WHITE};
use crate::ui::header::draw_header;
use crate::ui::results_list::scroll_cues;

/// Every remappable action and its current key, full-workspace like
/// the settings menu (this effectively *is* one of its settings,
/// just big enough to deserve its own screen rather than a row
/// inside that one). `capturing` shows the "press a key..." prompt
/// in place of the selected row's key while main.rs is waiting for
/// the next keypress to bind.
pub struct KeybindsMenu<'a> {
    pub keys: &'a [String],
    pub selected: usize,
    pub capturing: bool,
    pub conflict: Option<&'a str>,
}

impl Widget for KeybindsMenu<'_> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        // takes over a rect that already has other content drawn
        // into it, so it has to own every cell: without the clear,
        // whatever this menu did not write still showed through.
        let area = crate::ui::draw::overlay(buf, area);

        let mut y = draw_header(
            buf,
            area,
            area.y,
            "keybinds",
            // shift+k or esc, not "k": lowercase k moves the
            // selection here, as the footer says two rows down, and
            // this header contradicted it.
            "shift+k or esc: close",
            shade(DIM),
            shade(NEAR_BLACK),
        );
        y += 1;

        let label_width = ALL_ACTIONS
            .iter()
            .map(|a| a.label().len())
            .max()
            .unwrap_or(0);
        // the hint row (and the conflict notice while there is one)
        // claim their rows before the list does, then the list scrolls
        // inside what is left: it had no offset at all, so past the
        // pane height `j` moved an invisible cursor and `r` rebound a
        // row nobody could see.
        let footer = 2 + u16::from(self.conflict.is_some());
        let list_top = y;
        let room = (area.y + area.height).saturating_sub(list_top);
        let rows = room.saturating_sub(footer).min(ALL_ACTIONS.len() as u16);
        let offset = crate::ui::scroll::offset_for(self.selected, rows as usize);
        // one column of gutter for the scroll cue, and only while
        // there is one: a list that fits keeps its full width.
        let scrolls = ALL_ACTIONS.len() > rows as usize;
        let text = Rect {
            width: area.width.saturating_sub(u16::from(scrolls)),
            ..area
        };
        for (row, (i, action)) in ALL_ACTIONS
            .iter()
            .enumerate()
            .skip(offset)
            .take(rows as usize)
            .enumerate()
        {
            let is_selected = i == self.selected;
            let color = if is_selected {
                shade(NEAR_WHITE)
            } else {
                shade(DIM)
            };
            let marker = if is_selected { '>' } else { ' ' };
            let key_text = if is_selected && self.capturing {
                "press a key...".to_string()
            } else {
                self.keys[i].clone()
            };
            draw_line(
                buf,
                text,
                list_top + row as u16,
                &format!("{marker} {:<label_width$}  {key_text}", action.label(),),
                color,
            );
        }
        scroll_cues(
            buf,
            Rect {
                y: list_top,
                height: rows,
                ..area
            },
            offset,
            ALL_ACTIONS.len(),
        );

        y = list_top + rows + 1;
        draw_line(
            buf,
            area,
            y,
            "j/k select, r rebind selected, 0 reset selected, shift+r reset all",
            shade(DIM),
        );
        if let Some(owner) = self.conflict {
            y += 1;
            draw_line(
                buf,
                area,
                y,
                &format!("already bound to \"{owner}\"; rebinding anyway"),
                shade(NEAR_WHITE),
            );
        }
    }
}

fn draw_line(buf: &mut Buffer, area: Rect, y: u16, text: &str, color: ratatui::style::Color) {
    crate::ui::draw::line(buf, area, y, text, color);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::testbuf::{
        assert_no_bleed_through, assert_outside_untouched, buffer, grid, plated_buffer,
    };

    /// Rows from the rect's top: the frame's top edge, the header
    /// title, the header rule, a blank, then the action rows.
    const FIRST_ROW: usize = 4;
    /// Frame edge plus its one column of padding.
    const FIRST_COL: usize = 2;

    fn keys() -> Vec<String> {
        ALL_ACTIONS.iter().map(|_| "x".to_string()).collect()
    }

    fn render(area: Rect, buf: &mut Buffer, keys: &[String], conflict: Option<&str>) {
        KeybindsMenu {
            keys,
            selected: 0,
            capturing: false,
            conflict,
        }
        .render(area, buf);
    }

    fn action_rows(g: &str) -> Vec<Vec<char>> {
        g.lines()
            .skip(FIRST_ROW)
            .take(ALL_ACTIONS.len())
            .map(|r| r.chars().collect())
            .collect()
    }

    #[test]
    fn every_action_gets_a_row_with_its_key_in_an_aligned_column() {
        let area = Rect::new(0, 0, 48, 8 + ALL_ACTIONS.len() as u16);
        let mut buf = buffer(area.width, area.height);
        render(area, &mut buf, &keys(), None);
        let g = grid(&buf);
        let rows = action_rows(&g);
        assert_eq!(rows.len(), ALL_ACTIONS.len());
        let key_col = rows[0]
            .iter()
            .rposition(|c| *c == 'x')
            .expect("a key column");
        for (row, action) in rows.iter().zip(ALL_ACTIONS.iter()) {
            let text: String = row.iter().collect();
            assert!(text.contains(action.label()), "missing label:\n{g}");
            assert_eq!(row[key_col], 'x', "key column is jagged:\n{g}");
        }
        assert!(g.contains("j/k select, r rebind selected"), "{g}");
    }

    #[test]
    fn the_conflict_notice_quotes_the_action_already_holding_the_key() {
        let area = Rect::new(0, 0, 64, 9 + ALL_ACTIONS.len() as u16);
        let mut buf = buffer(area.width, area.height);
        render(area, &mut buf, &keys(), Some("quit"));
        assert!(
            grid(&buf).contains("already bound to \"quit\"; rebinding anyway"),
            "{}",
            grid(&buf)
        );
    }

    /// A key bound to a wide char ("日" is two cells) must not push the
    /// rest of its own row, or the rows after it, off their columns.
    #[test]
    fn a_wide_key_label_does_not_shift_the_rows_after_it() {
        let area = Rect::new(0, 0, 48, 8 + ALL_ACTIONS.len() as u16);
        let mut wide = keys();
        wide[0] = "日".to_string();
        let mut buf = buffer(area.width, area.height);
        render(area, &mut buf, &wide, None);
        let g = grid(&buf);
        let rows = action_rows(&g);
        // the wide glyph owns two cells, so the cell after it is the
        // blank the writer laid down, not the next character of text.
        let col = rows[0]
            .iter()
            .position(|c| *c == '日')
            .expect("the wide key");
        assert_eq!(rows[0][col + 1], ' ', "{g}");
        // every later row still carries its own key at the same column
        // the first row's key starts at.
        for row in &rows[1..] {
            assert_eq!(row[col], 'x', "key column drifted:\n{g}");
        }
    }

    #[test]
    fn a_wide_key_label_stays_inside_the_rect() {
        let mut buf = buffer(40, 12);
        let area = Rect::new(2, 1, 20, 8);
        let mut wide = keys();
        for k in wide.iter_mut() {
            *k = "日本語日本語".to_string();
        }
        render(area, &mut buf, &wide, Some("日本語"));
        assert_outside_untouched(&buf, area);
    }

    #[test]
    fn the_overlay_covers_every_cell_of_its_rect() {
        let mut buf = plated_buffer(40, 16);
        let area = Rect::new(2, 1, 30, 13);
        render(area, &mut buf, &keys(), Some("quit"));
        assert_no_bleed_through(&buf, area);
    }

    #[test]
    fn the_frame_closes_all_four_corners() {
        let mut buf = buffer(40, 16);
        let area = Rect::new(2, 1, 30, 13);
        render(area, &mut buf, &keys(), None);
        let g = grid(&buf);
        let rows: Vec<Vec<char>> = g.lines().map(|r| r.chars().collect()).collect();
        assert_eq!(
            (rows[1][2], rows[1][31], rows[13][2], rows[13][31]),
            ('┌', '┐', '└', '┘')
        );
        // the content starts one row and two columns inside it
        assert_eq!(
            rows[2][area.x as usize + FIRST_COL],
            'k',
            "the title sits inside the frame"
        );
    }

    #[test]
    fn degenerate_sizes_do_not_panic() {
        for (w, h) in [(1, 1), (1, 40), (2, 1), (40, 0), (0, 0), (3, 2)] {
            let mut buf = buffer(w.max(1), h.max(1));
            render(Rect::new(0, 0, w, h), &mut buf, &keys(), Some("quit"));
        }
    }

    /// The host cycles the selection over every action, so every one
    /// of them has to be reachable on a pane that holds a fraction of
    /// them: 80x24 with the default 4-row bottom bar leaves 20 rows,
    /// and `r` rebinding a row nobody can see is the actual hazard.
    #[test]
    fn the_selected_action_and_the_prompt_are_on_screen_at_every_selection() {
        for (selected, action) in ALL_ACTIONS.iter().enumerate() {
            let area = Rect::new(0, 0, 80, 20);
            let mut buf = buffer(80, 20);
            KeybindsMenu {
                keys: &keys(),
                selected,
                capturing: true,
                conflict: None,
            }
            .render(area, &mut buf);
            let g = grid(&buf);
            let label = action.label();
            assert!(
                g.contains(&format!("> {label}")),
                "selection {selected} ({label}) is off screen:\n{g}"
            );
            assert!(
                g.contains("press a key..."),
                "the capture prompt went with it:\n{g}"
            );
            assert!(
                g.contains("j/k select, r rebind selected"),
                "the footer hint went with it:\n{g}"
            );
        }
    }

    /// The conflict notice is the one row that explains what a rebind
    /// just did, so it claims its row from the list rather than being
    /// pushed off the bottom by it.
    #[test]
    fn the_conflict_notice_survives_a_pane_the_list_overflows() {
        let area = Rect::new(0, 0, 80, 20);
        let mut buf = buffer(80, 20);
        render(area, &mut buf, &keys(), Some("quit"));
        assert!(
            grid(&buf).contains("already bound to \"quit\""),
            "{}",
            grid(&buf)
        );
    }

    /// A window onto a longer list has to say so, or 29 actions read
    /// as however many fit; same `▴`/`▾` gutter cue `lyrics_view` uses.
    #[test]
    fn a_scrolled_list_cues_what_is_off_each_edge() {
        let cue = |selected: usize, h: u16| {
            let mut buf = buffer(80, h);
            render_at(Rect::new(0, 0, 80, h), &mut buf, selected);
            let g = grid(&buf);
            (g.contains('▴'), g.contains('▾'))
        };
        assert_eq!(cue(0, 20), (false, true), "more below only");
        assert_eq!(cue(ALL_ACTIONS.len() - 1, 20), (true, false), "above only");
        assert_eq!(cue(13, 20), (true, true), "both edges");
        // tall enough for every action: no cue at all.
        assert_eq!(cue(0, 40), (false, false), "nothing is off screen");
    }

    fn render_at(area: Rect, buf: &mut Buffer, selected: usize) {
        KeybindsMenu {
            keys: &keys(),
            selected,
            capturing: false,
            conflict: None,
        }
        .render(area, buf);
    }
}

#[cfg(test)]
mod eyeball {
    use super::*;
    use crate::ui::testbuf::{buffer, grid};

    /// The menu at the rows `main_content` actually gets (80x24 with
    /// the default 4-row bottom bar leaves 20), with the selection on
    /// the last action, which is where it ends up after holding `j`:
    /// `cargo test --lib ui::keybinds_menu::eyeball -- --nocapture`.
    /// Asserts the floor too, so it is a test and not just a printer.
    #[test]
    fn print_the_menu_with_the_selection_at_the_end() {
        let keys: Vec<String> = ALL_ACTIONS.iter().map(|_| "x".to_string()).collect();
        for (w, h, selected, capturing, conflict) in [
            (80u16, 20u16, ALL_ACTIONS.len() - 1, true, None),
            (80, 20, 0, false, None),
            (80, 20, 13, false, None),
            (80, 20, ALL_ACTIONS.len() - 1, false, Some("quit")),
            (80, 8, ALL_ACTIONS.len() - 1, true, None),
        ] {
            let mut buf = buffer(w, h);
            KeybindsMenu {
                keys: &keys,
                selected,
                capturing,
                conflict,
            }
            .render(Rect::new(0, 0, w, h), &mut buf);
            let text = grid(&buf);
            println!("--- {w}x{h} selected={selected} capturing={capturing} conflict={conflict:?}\n{text}");
            assert!(
                text.chars().any(|c| c != '.' && c != '\n'),
                "{w}x{h} drew nothing at all"
            );
        }
    }
}
