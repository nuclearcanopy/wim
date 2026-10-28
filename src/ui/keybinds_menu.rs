use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::widgets::Widget;

use crate::keymap::ALL_ACTIONS;
use crate::theme::{shade, DIM, NEAR_BLACK, NEAR_WHITE};
use crate::ui::header::draw_header;

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
        let mut y = draw_header(
            buf,
            area,
            area.y,
            "keybinds",
            "k to close",
            shade(DIM),
            shade(NEAR_BLACK),
        );
        y += 1;

        let label_width = ALL_ACTIONS
            .iter()
            .map(|a| a.label().len())
            .max()
            .unwrap_or(0);
        for (i, action) in ALL_ACTIONS.iter().enumerate() {
            if y >= area.y + area.height {
                break;
            }
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
                area,
                y,
                &format!("{marker} {:<label_width$}  {key_text}", action.label(),),
                color,
            );
            y += 1;
        }
        y += 1;
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
    use crate::ui::testbuf::{assert_outside_untouched, buffer, grid};

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

    #[test]
    fn every_action_gets_a_row_with_its_key_in_an_aligned_column() {
        let area = Rect::new(0, 0, 44, 5 + ALL_ACTIONS.len() as u16);
        let mut buf = buffer(area.width, area.height);
        render(area, &mut buf, &keys(), None);
        let g = grid(&buf);
        let rows: Vec<&str> = g.lines().skip(3).take(ALL_ACTIONS.len()).collect();
        assert_eq!(rows.len(), ALL_ACTIONS.len());
        let key_col = rows[0].find("  x").expect("a key column") + 2;
        for (row, action) in rows.iter().zip(ALL_ACTIONS.iter()) {
            assert!(row.contains(action.label()), "missing label:\n{g}");
            assert_eq!(
                row.chars().nth(key_col),
                Some('x'),
                "key column is jagged:\n{g}"
            );
        }
        assert!(g.contains("j/k select, r rebind selected"), "{g}");
    }

    #[test]
    fn the_conflict_notice_quotes_the_action_already_holding_the_key() {
        let area = Rect::new(0, 0, 60, 6 + ALL_ACTIONS.len() as u16);
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
        let area = Rect::new(0, 0, 44, 4 + ALL_ACTIONS.len() as u16);
        let mut wide = keys();
        wide[0] = "日".to_string();
        let mut buf = buffer(area.width, area.height);
        render(area, &mut buf, &wide, None);
        let g = grid(&buf);
        let rows: Vec<&str> = g.lines().skip(3).take(ALL_ACTIONS.len()).collect();
        // the wide glyph owns two cells, so the cell after it is the
        // blank the writer laid down, not the next character of text.
        let col = rows[0].find('日').expect("the wide key");
        assert_eq!(rows[0].chars().nth(col + 1), Some(' '), "{g}");
        // every later row still carries its own key at the same column
        // the first row's key starts at.
        for row in &rows[1..] {
            assert_eq!(row.chars().nth(col), Some('x'), "key column drifted:\n{g}");
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
    fn degenerate_sizes_do_not_panic() {
        for (w, h) in [(1, 1), (1, 40), (2, 1), (40, 0), (0, 0), (3, 2)] {
            let mut buf = buffer(w.max(1), h.max(1));
            render(Rect::new(0, 0, w, h), &mut buf, &keys(), Some("quit"));
        }
    }
}
