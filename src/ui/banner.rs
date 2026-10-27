use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::widgets::Widget;

use crate::theme::{lerp, shade, BRIGHT, DIM, NEAR_BLACK, WHITE};

/// Real bold 3D block lettering (figlet's `larry3d` font,
/// `figlet -f larry3d WIM`), baked in as static art rather than
/// hand-rolled. Tried `isometric1` first, but that font simulates true
/// isometric perspective, which shifts each letter diagonally relative
/// to the ones before it; reads as "stacked behind each other" rather
/// than clean 3D weight. `larry3d` gives bold slanted depth per letter
/// without that progressive diagonal drift across the whole word. No
/// figlet dependency at runtime; "WIM" never changes, so this is just
/// baked-in text.
const LOGO: &[&str] = &[
    r" __      __  ______               ",
    r"/\ \  __/\ \/\__  _\   /'\_/`\    ",
    r"\ \ \/\ \ \ \/_/\ \/  /\      \   ",
    r" \ \ \ \ \ \ \ \ \ \  \ \ \__\ \  ",
    r"  \ \ \_/ \_\ \ \_\ \__\ \ \_/\ \ ",
    r"   \ `\___x___/ /\_____\\ \_\\ \_\",
    r"    '\/__//__/  \/_____/ \/_/ \/_/",
];
pub const GLYPH_HEIGHT: usize = 7;

/// Renders the logo with a brightness sweep plus a slower overall pulse
/// layered on top (two independent motions read as more "alive" than a
/// single linear sweep), framed above/below by unicode rules and a
/// tagline. `phase`/`elapsed` are advanced by the caller each tick.
///
/// Contrast matters as much as the animation: the frame rules and
/// tagline deliberately sit much dimmer than the logo itself (NEAR_BLACK
/// vs. the logo's WHITE/BRIGHT range); everything rendered at similar
/// brightness reads as flat no matter how much is animated.
pub struct Banner {
    pub phase: f32,
    pub elapsed_secs: f32,
}

/// The two rows `draw_rule` paints on, in absolute buffer coordinates;
/// exposed so the caller can tell the main/sidebar `VerticalRule` where
/// to switch from `│` to `├` (see `src/ui/divider.rs`), since those two
/// lines meet at this widget's left edge with zero awareness of each
/// other otherwise.
pub fn rule_rows(area: Rect) -> [u16; 2] {
    let frame_height = GLYPH_HEIGHT as u16 + 4; // rule, gap, logo, gap, rule+tagline
    let block_start_y = area.y + area.height.saturating_sub(frame_height) / 2;
    let rule_y = block_start_y + 1 + GLYPH_HEIGHT as u16;
    [block_start_y, rule_y]
}

/// 0..1, slow breathing; the single source of this animation's phase
/// so the rule's own color and the junction glow (`junction_level`,
/// drawn by a different widget entirely) stay in lockstep instead of
/// drifting out of sync with two independent calculations.
pub fn pulse_level(elapsed_secs: f32) -> f32 {
    (elapsed_secs * 1.3).sin() * 0.5 + 0.5
}

/// The rule's own brightness at a given pulse phase.
fn rule_level(pulse: f32) -> f32 {
    lerp(NEAR_BLACK, DIM * 0.5, pulse)
}

/// Brighter peak for the junction character itself (drawn by
/// `VerticalRule`, one column to the left of where this widget's own
/// rule starts) and the couple of rule characters nearest to it, so
/// the junction doesn't look like the one static, dead spot on an
/// otherwise animated line.
pub fn junction_level(pulse: f32) -> f32 {
    lerp(NEAR_BLACK, BRIGHT, pulse)
}

impl Widget for Banner {
    fn render(self, area: Rect, buf: &mut Buffer) {
        let total_width = LOGO[0].chars().count() as u16;
        let tagline = "terminal omniplayer";
        let start_x = area.x + area.width.saturating_sub(total_width) / 2;
        let [block_start_y, rule_y] = rule_rows(area);

        let pulse = pulse_level(self.elapsed_secs);

        draw_rule(buf, area, block_start_y, pulse);
        let logo_start_y = block_start_y + 1;

        for (row_idx, row) in LOGO.iter().enumerate() {
            let y = logo_start_y + row_idx as u16;
            if y >= area.y + area.height {
                continue;
            }
            for (col_idx, ch) in row.chars().enumerate() {
                if ch == ' ' {
                    continue;
                }
                let x = start_x + col_idx as u16;
                if x >= area.x + area.width {
                    continue;
                }
                let rel = (x - start_x) as f32 / total_width.max(1) as f32;
                let sweep = ((rel - self.phase).rem_euclid(1.0) - 0.5).abs() * 2.0;
                // all math stays in plain 0..1 levels until the one
                // conversion to a Color at the end.
                let sweep_level = lerp(WHITE, BRIGHT, sweep);
                let level = (sweep_level * (0.8 + 0.2 * pulse)).clamp(0.0, 1.0);
                if let Some(cell) = buf.cell_mut((x, y)) {
                    cell.set_char(ch);
                    cell.set_fg(shade(level));
                }
            }
        }

        draw_rule(buf, area, rule_y, pulse);
        draw_centered(buf, area, rule_y + 1, tagline, shade(DIM));
    }
}

/// Tapers the brightness of the first couple of characters toward the
/// junction's own glow level rather than cutting straight to the
/// rule's normal (much dimmer) brightness, so the junction reads as
/// the brightest point of one continuous glow fading outward, not a
/// single lit pixel next to an otherwise flat line.
const GLOW_REACH: u16 = 2;

fn draw_rule(buf: &mut Buffer, area: Rect, y: u16, pulse: f32) {
    if y >= area.y + area.height {
        return;
    }
    // deliberately dim and low-contrast against the logo; a divider
    // that competes with the content it's dividing isn't doing its job.
    let base = rule_level(pulse);
    let peak = junction_level(pulse);
    for (dist, x) in (area.x..area.x + area.width).enumerate() {
        let level = if (dist as u16) <= GLOW_REACH {
            lerp(peak, base, dist as f32 / GLOW_REACH as f32)
        } else {
            base
        };
        if let Some(cell) = buf.cell_mut((x, y)) {
            cell.set_char('─');
            cell.set_fg(shade(level));
        }
    }
}

fn draw_centered(buf: &mut Buffer, area: Rect, y: u16, text: &str, color: ratatui::style::Color) {
    if y >= area.y + area.height {
        return;
    }
    let text_width = text.chars().count() as u16;
    let start_x = area.x + area.width.saturating_sub(text_width) / 2;
    for (i, ch) in text.chars().enumerate() {
        let x = start_x + i as u16;
        if x >= area.x + area.width {
            break;
        }
        if let Some(cell) = buf.cell_mut((x, y)) {
            cell.set_char(ch);
            cell.set_fg(color);
        }
    }
}
