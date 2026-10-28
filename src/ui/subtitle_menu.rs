use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::widgets::Widget;

use crate::keymap::{key_label, KeyAction, Keymap};
use crate::theme::{shade, DIM, NEAR_BLACK, NEAR_WHITE};
use crate::ui::textfit::width;

/// One adjustable row: label plus the already-formatted current
/// value, so this widget stays pure rendering and knows nothing about
/// `SubtitleConfig`'s actual field types (a mix of f64/u32/f32) or
/// their individual step sizes/bounds; main.rs owns all of that.
pub const FIELD_COUNT: usize = 7;
pub const FIELD_LABELS: [&str; FIELD_COUNT] = [
    "delay",
    "font size",
    "position",
    "outline",
    "background",
    "blur",
    "color",
];

/// Centered bordered box, not a full-workspace takeover like the
/// help/settings overlays: "centered... and cover it" was the
/// explicit ask for this one specifically, smaller and opaque over
/// just its own footprint rather than the whole screen.
pub struct SubtitleMenu<'a> {
    pub values: [&'a str; FIELD_COUNT],
    pub selected: usize,
    pub keymap: &'a Keymap,
}

const WIDTH: u16 = 32;

impl Widget for SubtitleMenu<'_> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        let height = FIELD_COUNT as u16 + 4;
        if area.width < WIDTH || area.height < height {
            return;
        }
        let x = area.x + (area.width - WIDTH) / 2;
        let y = area.y + area.height.saturating_sub(height) / 2;
        let rect = Rect {
            x,
            y,
            width: WIDTH,
            height,
        };
        // clear, not just the spaces draw_box used to lay down: a
        // selection plate underneath sets a background, which a space
        // with no style reset would leave showing through this box.
        crate::ui::draw::clear(buf, rect);
        crate::ui::draw::frame(buf, rect, shade(NEAR_BLACK));
        // the close key is remappable; j/k and h/l are the fixed
        // conventions reserved in keymap.rs, so they stay literal.
        let close = key_label(self.keymap.get(KeyAction::OpenSubtitleMenu));
        draw_text(
            buf,
            rect,
            x + 2,
            y,
            &format!("subtitles ({close} to close)"),
            shade(NEAR_WHITE),
        );
        draw_text(
            buf,
            rect,
            x + 2,
            y + 1,
            "j/k select, h/l adjust",
            shade(DIM),
        );

        for (i, (label, value)) in FIELD_LABELS.iter().zip(self.values.iter()).enumerate() {
            let row_y = y + 3 + i as u16;
            let color = if i == self.selected {
                shade(NEAR_WHITE)
            } else {
                shade(DIM)
            };
            let marker = if i == self.selected { '>' } else { ' ' };
            draw_text(buf, rect, x + 2, row_y, &format!("{marker} {label}"), color);
            // right-aligned by display width: a char count parks a
            // wide-char value one cell per wide glyph past the edge.
            // clamped to the label column so a value wider than the
            // box is clipped by the right border rather than starting
            // outside the left one.
            let value_x = (x + WIDTH - 2)
                .saturating_sub(width(value) as u16)
                .max(x + 2);
            draw_text(buf, rect, value_x, row_y, value, color);
        }
    }
}

/// Clipped to `rect`, the box this menu owns; the old copy had no
/// bounds guard at all and ran to the buffer's own edge, over whatever
/// the overlay is sitting on top of.
fn draw_text(
    buf: &mut Buffer,
    rect: Rect,
    x: u16,
    y: u16,
    text: &str,
    color: ratatui::style::Color,
) {
    if y < rect.y || y >= rect.y + rect.height {
        return;
    }
    let right = rect.x + rect.width;
    crate::ui::draw::at(buf, x, y, text, color, right.saturating_sub(x) as usize);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::testbuf::{
        assert_no_bleed_through, assert_outside_untouched, buffer, grid, plated_buffer,
    };
    use crossterm::event::KeyCode;

    /// Where the box lands inside an `area` of this size: it centres
    /// itself, so the tests have to derive the rect the same way.
    fn box_rect(w: u16, h: u16) -> Rect {
        let height = FIELD_COUNT as u16 + 4;
        Rect::new((w - WIDTH) / 2, h.saturating_sub(height) / 2, WIDTH, height)
    }

    fn render(area: Rect, buf: &mut Buffer, values: [&str; FIELD_COUNT]) {
        SubtitleMenu {
            values,
            selected: 0,
            keymap: &Keymap::default(),
        }
        .render(area, buf);
    }

    #[test]
    fn the_box_holds_a_labelled_row_per_field_with_its_value_right_aligned() {
        let area = Rect::new(0, 0, 32, 11);
        let mut buf = buffer(32, 11);
        render(
            area,
            &mut buf,
            ["0.0s", "48", "100", "2", "0.5", "0.2", "white"],
        );
        assert_eq!(
            grid(&buf),
            [
                "┌─subtitles (t to close)───────┐",
                "│ j/k select, h/l adjust       │",
                "│                              │",
                "│ > delay                 0.0s │",
                "│   font size               48 │",
                "│   position               100 │",
                "│   outline                  2 │",
                "│   background             0.5 │",
                "│   blur                   0.2 │",
                "│   color                white │",
                "└──────────────────────────────┘",
            ]
            .join("\n")
        );
    }

    /// The value column is right-aligned; measured in chars it parks a
    /// wide value one cell per wide glyph past the box's right border.
    #[test]
    fn a_wide_value_is_right_aligned_by_cells_and_keeps_the_border() {
        let area = Rect::new(0, 0, 32, 11);
        let mut buf = buffer(32, 11);
        render(area, &mut buf, ["日本", "", "", "", "", "", ""]);
        let row: Vec<char> = grid(&buf).lines().nth(3).unwrap().chars().collect();
        assert_eq!(row[31], '│', "value overran the border");
        // four cells for two wide chars, ending one cell in from the
        // border exactly like the ascii values above.
        assert_eq!((row[26], row[27], row[28], row[29]), ('日', ' ', '本', ' '));
    }

    /// This one always drew a border, but its interior was spaces with
    /// no style reset, so a selection plate underneath still showed
    /// through the box as a background.
    #[test]
    fn the_box_covers_every_cell_of_its_own_footprint() {
        let mut buf = plated_buffer(40, 16);
        render(Rect::new(0, 0, 40, 16), &mut buf, ["0.0s"; FIELD_COUNT]);
        assert_no_bleed_through(&buf, box_rect(40, 16));
    }

    #[test]
    fn nothing_is_drawn_outside_the_box() {
        let mut buf = buffer(40, 16);
        let long = "日本語テスト曲日本語テスト曲日本語テスト曲";
        render(Rect::new(0, 0, 40, 16), &mut buf, [long; FIELD_COUNT]);
        // the box is centred in the area; everything outside it stays
        // as the overlay found it.
        let box_rect = box_rect(40, 16);
        assert_outside_untouched(&buf, box_rect);
    }

    #[test]
    fn degenerate_sizes_draw_nothing_instead_of_panicking() {
        for (w, h) in [(1, 1), (1, 40), (2, 1), (40, 0), (0, 0), (31, 11)] {
            let mut buf = buffer(w.max(1), h.max(1));
            render(Rect::new(0, 0, w, h), &mut buf, ["日本語"; FIELD_COUNT]);
            if w > 0 && h > 0 {
                assert!(
                    !grid(&buf).contains('┌'),
                    "too small for the box, so it must not draw one"
                );
            }
        }
    }

    #[test]
    fn the_close_hint_follows_the_live_keymap() {
        let mut keymap = Keymap::default();
        keymap.set(KeyAction::OpenSubtitleMenu, KeyCode::Char('z'));
        let area = Rect::new(0, 0, 60, 20);
        let mut buf = Buffer::empty(area);
        SubtitleMenu {
            values: ["", "", "", "", "", "", ""],
            selected: 0,
            keymap: &keymap,
        }
        .render(area, &mut buf);
        let out: String = (0..area.height)
            .map(|y| {
                (0..area.width)
                    .map(|x| buf.cell((x, y)).unwrap().symbol().to_string())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(out.contains("subtitles (z to close)"), "got:\n{out}");
        assert!(!out.contains("(t to close)"), "stale default key:\n{out}");
        // h/l and j/k are reserved, not keymap entries.
        assert!(out.contains("j/k select, h/l adjust"), "got:\n{out}");
    }
}
