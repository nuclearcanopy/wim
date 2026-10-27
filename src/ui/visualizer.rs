use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::widgets::Widget;

use crate::theme::shade;

/// Height-varying unicode blocks, one column per history sample
/// (newest on the right), reading as a scrolling level meter even
/// though it's driven by a single overall value per frame rather than
/// a true per-frequency-band spectrum (see `main.rs::audio_level`).
pub struct LevelMeter<'a> {
    pub history: &'a [f32],
}

const GLYPHS: [char; 8] = ['▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];

impl Widget for LevelMeter<'_> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        if area.height == 0 || area.width == 0 {
            return;
        }
        let y = area.y + area.height / 2;
        let cols = area.width as usize;
        let start = self.history.len().saturating_sub(cols);
        for (col, &level) in self.history[start..].iter().enumerate() {
            let x = area.x + col as u16;
            let glyph_idx = ((level.clamp(0.0, 1.0) * (GLYPHS.len() - 1) as f32).round()) as usize;
            if let Some(cell) = buf.cell_mut((x, y)) {
                cell.set_char(GLYPHS[glyph_idx]);
                cell.set_fg(shade(0.3 + level * 0.5));
            }
        }
    }
}
