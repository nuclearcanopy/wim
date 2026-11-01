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

/// What the meter costs per frame, which is the question worth having
/// an answer to for a widget that repaints continuously: one cell per
/// visible column, no allocation, and nothing carried between frames.
///
/// A measurement, not an assertion: the numbers move with the box and
/// a ceiling here would be a flake. Run it with
/// `cargo test --release --lib ui::visualizer::frame_budget -- --ignored --nocapture`;
/// the dev profile is `opt-level = 0` and reads several times the
/// shipping cost, which is why the build is printed first.
#[cfg(test)]
mod frame_budget {
    use super::*;
    use std::time::Instant;

    /// Alternating batches in one process, best of each; sequential
    /// runs on this box differ 1.6-1.8x under load. Same harness as
    /// `framesource::frame_budget`.
    fn best_each(rounds: usize, iters: usize, a: &mut dyn FnMut(), b: &mut dyn FnMut()) -> (f64, f64) {
        let mut best = (f64::MAX, f64::MAX);
        let time = |f: &mut dyn FnMut()| {
            let at = Instant::now();
            for _ in 0..iters {
                f();
            }
            at.elapsed().as_secs_f64() * 1e6 / iters as f64
        };
        for _ in 0..rounds {
            best.0 = best.0.min(time(a));
            best.1 = best.1.min(time(b));
        }
        best
    }

    /// Levels that actually differ per column: a flat history would
    /// make the one candidate optimization below (reusing the last
    /// column's `Color`) look free when real audio never is.
    fn history(n: usize) -> Vec<f32> {
        (0..n)
            .map(|i| ((i as f32 * 0.37).sin() * 0.5 + 0.5).clamp(0.0, 1.0))
            .collect()
    }

    #[test]
    #[ignore = "a measurement; run it explicitly"]
    fn what_the_level_meter_costs_per_frame() {
        // 144hz is `anim::MAX_HZ`, the fastest the poll loop redraws,
        // so it is the tightest deadline this widget has.
        let budget = 1e6 / crate::anim::MAX_HZ as f64;
        println!(
            "--- level meter, budget {budget:.0}us at {}hz, {} build",
            crate::anim::MAX_HZ,
            if cfg!(debug_assertions) {
                "DEBUG (not the shipping cost; re-run with --release)"
            } else {
                "release"
            }
        );
        // the pair is a bottom-bar-sized meter against a full-width
        // one, which is what says the cost is per visible column and
        // not per history sample: the history is the same length for
        // both, and only the rect differs.
        let samples = history(400);
        for (narrow, wide) in [(24u16, 120u16), (40, 320)] {
            let mut small = crate::ui::testbuf::buffer(wide, 3);
            let mut large = crate::ui::testbuf::buffer(wide, 3);
            let mut thin = || {
                LevelMeter { history: &samples }.render(Rect::new(0, 0, narrow, 3), &mut small);
            };
            let mut full = || {
                LevelMeter { history: &samples }.render(Rect::new(0, 0, wide, 3), &mut large);
            };
            let (a, b) = best_each(5, 2000, &mut thin, &mut full);
            println!(
                "  {narrow:>3} cols {a:>6.3}us ({:>5.3}% of budget)   {wide:>3} cols {b:>6.3}us ({:>5.3}%)   {:.1}x for {:.1}x the columns",
                a / budget * 100.0,
                b / budget * 100.0,
                b / a,
                f64::from(wide) / f64::from(narrow),
            );
            // the floor, so this is a test and not just a printer: a
            // meter that painted nothing would measure beautifully.
            // `testbuf::buffer` fills with '.', so an unpainted
            // column still reads as one.
            let row = 3 / 2;
            for (label, buf, cols) in [("narrow", &small, narrow), ("wide", &large, wide)] {
                for x in 0..cols {
                    let ch = crate::ui::testbuf::char_at(buf, x, row);
                    assert!(
                        GLYPHS.contains(&ch),
                        "{label} meter drew {ch:?} in column {x} of {cols}"
                    );
                }
            }
            // and the narrow one stayed inside its own rect.
            for x in narrow..wide {
                assert_eq!(
                    crate::ui::testbuf::char_at(&small, x, row),
                    '.',
                    "a {narrow}-column meter painted column {x}"
                );
            }
        }
    }
}
