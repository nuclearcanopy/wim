use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::widgets::Widget;

use crate::theme::{shade, BRIGHT, DIM, NEAR_BLACK, WHITE};

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

impl Widget for Banner {
    fn render(self, area: Rect, buf: &mut Buffer) {
        let total_width = LOGO[0].chars().count() as u16;
        let tagline = "terminal omniplayer";
        let frame_height = GLYPH_HEIGHT as u16 + 4; // rule, gap, logo, gap, rule+tagline
        let start_x = area.x + area.width.saturating_sub(total_width) / 2;
        let block_start_y = area.y + area.height.saturating_sub(frame_height) / 2;

        let pulse = (self.elapsed_secs * 1.3).sin() * 0.5 + 0.5; // 0..1, slow breathing

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
                let sweep_level = WHITE + (BRIGHT - WHITE) * sweep;
                let level = (sweep_level * (0.8 + 0.2 * pulse)).clamp(0.0, 1.0);
                if let Some(cell) = buf.cell_mut((x, y)) {
                    cell.set_char(ch);
                    cell.set_fg(shade(level));
                }
            }
        }

        let rule_y = logo_start_y + GLYPH_HEIGHT as u16;
        draw_rule(buf, area, rule_y, pulse);
        draw_centered(buf, area, rule_y + 1, tagline, shade(DIM));
    }
}

fn draw_rule(buf: &mut Buffer, area: Rect, y: u16, pulse: f32) {
    if y >= area.y + area.height {
        return;
    }
    // deliberately dim and low-contrast against the logo; a divider
    // that competes with the content it's dividing isn't doing its job.
    let level = NEAR_BLACK + (DIM * 0.5 - NEAR_BLACK) * pulse;
    let color = shade(level);
    for x in area.x..area.x + area.width {
        if let Some(cell) = buf.cell_mut((x, y)) {
            cell.set_char('─');
            cell.set_fg(color);
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

