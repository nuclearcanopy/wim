use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::widgets::Widget;

use crate::theme::{lerp, shade, BRIGHT, DIM, NEAR_BLACK, WHITE};

/// `figlet -f "ANSI Shadow" WIM`; the same font LazyVim's own
/// snacks.nvim dashboard header uses (confirmed against LazyVim's
/// actual `lua/lazyvim/plugins/ui.lua` source, not just recalled from
/// memory), baked in as static art rather than a runtime figlet call.
/// No figlet dependency at runtime; "WIM" never changes.
const LOGO: &[&str] = &[
    r"██╗    ██╗██╗███╗   ███╗",
    r"██║    ██║██║████╗ ████║",
    r"██║ █╗ ██║██║██╔████╔██║",
    r"██║███╗██║██║██║╚██╔╝██║",
    r"╚███╔███╔╝██║██║ ╚═╝ ██║",
    r" ╚══╝╚══╝ ╚═╝╚═╝     ╚═╝",
];
pub const GLYPH_HEIGHT: usize = 6;

/// Key, description; same `snacks.dashboard.Item` idea LazyVim's own
/// dashboard menu uses below its header, minus the nerd-font icons
/// (same monochrome/plain-ascii reasoning as the result-row `[sk]`
/// codes: a patched-font glyph the user's terminal doesn't have would
/// just show as a broken box, not a silent icon).
const MENU: &[(&str, &str)] = &[
    ("/", "search"),
    ("n", "navidrome profiles"),
    ("S", "starred songs"),
    ("?", "help"),
    ("q", "quit"),
];

/// Logo with a brightness sweep plus a slower overall pulse layered on
/// top (two independent motions read as more "alive" than a single
/// linear sweep), a key/description menu below it (LazyVim's own
/// dashboard structure), and the local-library count as a footer line
/// (this is specifically where that count belongs: once, on the idle
/// home screen, not permanently crammed into the status line every
/// time it's on screen). `phase`/`elapsed` are advanced by the caller
/// each tick.
pub struct Banner {
    pub phase: f32,
    pub elapsed_secs: f32,
    pub local_items: usize,
}

/// 0..1, slow breathing.
pub fn pulse_level(elapsed_secs: f32) -> f32 {
    (elapsed_secs * 1.3).sin() * 0.5 + 0.5
}

impl Widget for Banner {
    fn render(self, area: Rect, buf: &mut Buffer) {
        let total_width = LOGO[0].chars().count() as u16;
        let start_x = area.x + area.width.saturating_sub(total_width) / 2;

        // logo, one gap row, the menu, one gap row, the footer line.
        let frame_height = GLYPH_HEIGHT as u16 + 2 + MENU.len() as u16 + 2;
        let logo_start_y = area.y + area.height.saturating_sub(frame_height) / 2;

        let pulse = pulse_level(self.elapsed_secs);

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

        let menu_start_y = logo_start_y + GLYPH_HEIGHT as u16 + 2;
        for (i, (key, desc)) in MENU.iter().enumerate() {
            draw_centered(
                buf,
                area,
                menu_start_y + i as u16,
                &format!("{key}  {desc}"),
                shade(DIM),
            );
        }

        let footer_y = menu_start_y + MENU.len() as u16 + 1;
        let footer = format!("{} local items indexed", self.local_items);
        draw_centered(buf, area, footer_y, &footer, shade(NEAR_BLACK));
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
