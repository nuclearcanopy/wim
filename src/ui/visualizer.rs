use std::time::Duration;

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::widgets::Widget;

use crate::theme::shade;

const ATTACK: f32 = 0.055;
const RELEASE: f32 = 0.24;
const BAR_HEIGHT: u16 = 8;
const BAR_GLYPH: char = '▏';

/// Time-based attack/release envelope. The alpha is derived from elapsed
/// time so the animation has the same feel at 30 Hz and 144 Hz.
#[derive(Default)]
pub struct SpectrumSmoother {
    levels: Vec<f32>,
    last_update: Option<Duration>,
}

impl SpectrumSmoother {
    pub fn update(&mut self, target: Option<&[f32]>, bands: usize, now: Duration) -> &[f32] {
        self.levels.resize(bands.clamp(4, 32), 0.0);
        let dt = self
            .last_update
            .map_or(0.0, |last| now.saturating_sub(last).as_secs_f32().min(0.25));
        self.last_update = Some(now);
        for (i, level) in self.levels.iter_mut().enumerate() {
            let want = target
                .and_then(|v| v.get(i))
                .copied()
                .unwrap_or(0.0)
                .clamp(0.0, 1.0);
            let tau = if want > *level { ATTACK } else { RELEASE };
            let alpha = 1.0 - (-dt / tau).exp();
            *level += (want - *level) * alpha;
        }
        &self.levels
    }
}

/// Compact vertical spectrum bars. The thin glyph leaves visual space
/// between adjacent bands while each band still owns one cell column.
pub struct SpectrumMeter<'a> {
    pub levels: &'a [f32],
}

/// Reserve a one-cell-per-band strip at the player's right edge without
/// letting it consume more than half the pane on narrow terminals.
pub fn player_spectrum_layout(area: Rect, bands: usize) -> (Rect, Rect) {
    let strip_width = (bands.min(32) as u16).min(area.width / 2);
    let content = Rect {
        width: area.width - strip_width,
        ..area
    };
    let strip_height = area.height.min(BAR_HEIGHT);
    let spectrum = Rect {
        x: content.x + content.width,
        y: area.y + area.height - strip_height,
        width: strip_width,
        height: strip_height,
    };
    (content, spectrum)
}

impl Widget for SpectrumMeter<'_> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        if area.width == 0 || area.height == 0 || self.levels.is_empty() {
            return;
        }
        let columns = (area.width as usize).min(self.levels.len());
        let rows = area.height as usize;
        for col in 0..columns {
            let band = if columns == self.levels.len() {
                col
            } else {
                col * (self.levels.len() - 1) / columns.saturating_sub(1).max(1)
            };
            let level = self.levels[band].clamp(0.0, 1.0);
            let steps = (level * rows as f32).round() as usize;
            for row in 0..rows {
                let from_bottom = rows - row - 1;
                if let Some(cell) = buf.cell_mut((area.x + col as u16, area.y + row as u16)) {
                    if steps > from_bottom {
                        cell.set_char(BAR_GLYPH);
                        cell.set_fg(shade(0.25 + 0.65 * level));
                    } else {
                        cell.set_char(' ');
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::testbuf::{buffer, grid};

    #[test]
    fn attack_and_release_are_time_based_and_silence_decays_to_zero() {
        let mut a = SpectrumSmoother::default();
        let mut b = SpectrumSmoother::default();
        a.update(None, 10, Duration::ZERO);
        b.update(None, 10, Duration::ZERO);
        let high = [1.0; 10];
        let va = a.update(Some(&high), 10, Duration::from_millis(50))[0];
        let mut vb = 0.0;
        for ms in (10..=50).step_by(10) {
            vb = b.update(Some(&high), 10, Duration::from_millis(ms))[0];
        }
        assert!((va - vb).abs() < 1e-6);
        assert!(va > 0.5);
        for ms in (100..=5000).step_by(50) {
            a.update(None, 10, Duration::from_millis(ms));
        }
        assert!(a.update(None, 10, Duration::from_millis(5050))[0] < 0.001);
    }

    #[test]
    fn meter_draws_one_vertical_cell_column_per_band() {
        let levels = [0.0, 0.5, 1.0, 0.0];
        let mut buf = buffer(6, 5);
        SpectrumMeter { levels: &levels }.render(Rect::new(1, 0, 4, 5), &mut buf);
        assert_eq!(buf.cell((0, 0)).unwrap().symbol(), ".");
        assert_eq!(buf.cell((5, 4)).unwrap().symbol(), ".");
        assert_eq!(buf.cell((1, 4)).unwrap().symbol(), " ");
        assert_eq!(buf.cell((2, 4)).unwrap().symbol(), BAR_GLYPH.to_string());
        assert_eq!(buf.cell((3, 0)).unwrap().symbol(), BAR_GLYPH.to_string());
        assert_eq!(buf.cell((3, 4)).unwrap().symbol(), BAR_GLYPH.to_string());
        assert_eq!(grid(&buf).lines().count(), 5);
    }

    #[test]
    fn player_layout_glues_one_cell_bars_to_the_right_edge() {
        let area = Rect::new(4, 2, 80, 20);
        let (content, spectrum) = player_spectrum_layout(area, 30);
        assert_eq!(content.width, 50);
        assert_eq!(spectrum.x, content.x + content.width);
        assert_eq!(spectrum.width, 30);
        assert_eq!(spectrum.x + spectrum.width, area.x + area.width);
        assert_eq!(spectrum.y, area.y + area.height - BAR_HEIGHT);
        assert_eq!(spectrum.height, BAR_HEIGHT);
    }
}
