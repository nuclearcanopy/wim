use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::widgets::Widget;

use crate::theme::{lerp_shade, DIM, WHITE};

/// Chunky 7-row block glyphs. Hand-drawn placeholder for the real banner —
/// good enough to prove custom-painted (non-widget) rendering end to end.
const GLYPH_HEIGHT: usize = 7;

fn glyph(c: char) -> &'static [&'static str] {
    match c {
        'W' => &[
            "█     █", //
            "█     █", //
            "█     █", //
            "█  █  █", //
            "█ █ █ █", //
            "██   ██", //
            "█     █", //
        ],
        'I' => &[
            "█████", //
            "  █  ", //
            "  █  ", //
            "  █  ", //
            "  █  ", //
            "  █  ", //
            "█████", //
        ],
        'M' => &[
            "█     █", //
            "██   ██", //
            "█ █ █ █", //
            "█  █  █", //
            "█     █", //
            "█     █", //
            "█     █", //
        ],
        _ => &["", "", "", "", "", "", ""],
    }
}

/// Renders "WIM" as large block glyphs with an animated left-to-right
/// brightness sweep. `phase` drives the sweep position, advanced by the
/// caller each tick.
pub struct Banner {
    pub phase: f32,
}

impl Widget for Banner {
    fn render(self, area: Rect, buf: &mut Buffer) {
        let rows: Vec<&[&str]> = ['W', 'I', 'M'].iter().map(|&c| glyph(c)).collect();
        let gap = 2usize;
        let total_width: usize = rows.iter().map(|g| g[0].chars().count() + gap).sum::<usize>() - gap;
        let total_width = total_width as u16;
        let start_x = area.x + area.width.saturating_sub(total_width) / 2;
        let start_y = area.y + area.height.saturating_sub(GLYPH_HEIGHT as u16) / 2;

        let mut col_cursor = start_x;
        for glyph_rows in rows {
            let glyph_width = glyph_rows[0].chars().count() as u16;
            for (row_idx, row) in glyph_rows.iter().enumerate() {
                let y = start_y + row_idx as u16;
                if y >= area.y + area.height {
                    continue;
                }
                for (col_idx, ch) in row.chars().enumerate() {
                    if ch == ' ' {
                        continue;
                    }
                    let x = col_cursor + col_idx as u16;
                    if x >= area.x + area.width {
                        continue;
                    }
                    // sweep: brightness is a function of column position and
                    // the animation phase, wrapping smoothly.
                    let rel = (x - start_x) as f32 / total_width.max(1) as f32;
                    let t = ((rel - self.phase).rem_euclid(1.0) - 0.5).abs() * 2.0;
                    let color = lerp_shade(WHITE, DIM, t);
                    if let Some(cell) = buf.cell_mut((x, y)) {
                        cell.set_char(ch);
                        cell.set_fg(color);
                    }
                }
            }
            col_cursor += glyph_width + gap as u16;
        }
    }
}
