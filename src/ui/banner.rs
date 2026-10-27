use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::widgets::Widget;

use crate::theme::{lerp, shade, BRIGHT, DIM, NEAR_WHITE, WHITE};
use crate::ui::logos::LOGOS;

/// `figlet -f "ANSI Shadow" WIM`; the same font LazyVim's own
/// snacks.nvim dashboard header uses (confirmed against LazyVim's
/// actual `lua/lazyvim/plugins/ui.lua` source, not just recalled from
/// memory), baked in as static art rather than a runtime figlet call.
/// No figlet dependency at runtime; "WIM" never changes. This is
/// specifically index 0 of the logo picker (`<`/`>` while the
/// workspace has focus, see `logo_lines`/`logo_name`); kept as its
/// own const rather than folded into `logos::LOGOS` since it's the
/// permanent default a 'reset' always returns to, not just another
/// item in that generated list.
const LOGO: &[&str] = &[
    r"██╗    ██╗██╗███╗   ███╗",
    r"██║    ██║██║████╗ ████║",
    r"██║ █╗ ██║██║██╔████╔██║",
    r"██║███╗██║██║██║╚██╔╝██║",
    r"╚███╔███╔╝██║██║ ╚═╝ ██║",
    r" ╚══╝╚══╝ ╚═╝╚═╝     ╚═╝",
];

/// 1 (the default) plus every alternative; `logo_index` cycles
/// `0..logo_count()`.
pub fn logo_count() -> usize {
    1 + LOGOS.len()
}

pub fn logo_lines(index: usize) -> &'static [&'static str] {
    if index == 0 {
        LOGO
    } else {
        LOGOS[index - 1].1
    }
}

/// `None` for the default (nothing to save, config just omits it);
/// `Some(name)` otherwise, the stable key persisted to config.toml
/// instead of the index itself (which would shift if this generated
/// list is ever regenerated with different fonts in a different
/// order).
pub fn logo_name(index: usize) -> Option<&'static str> {
    (index != 0).then(|| LOGOS[index - 1].0)
}

pub fn logo_index_for_name(name: Option<&str>) -> usize {
    match name {
        None => 0,
        Some(name) => LOGOS
            .iter()
            .position(|(n, _)| *n == name)
            .map(|i| i + 1)
            .unwrap_or(0),
    }
}

/// Key, description; same `snacks.dashboard.Item` idea LazyVim's own
/// dashboard menu uses below its header, minus the nerd-font icons
/// (same monochrome/plain-ascii reasoning as the result-row `[sk]`
/// codes: a patched-font glyph the user's terminal doesn't have would
/// just show as a broken box, not a silent icon).
const MENU: &[(&str, &str)] = &[
    ("/", "search"),
    ("n", "navidrome profiles"),
    ("S", "starred songs"),
    ("I", "index breakdown"),
    ("?", "help"),
    ("q", "quit"),
];

/// Logo with a brightness sweep plus a slower overall pulse layered on
/// top (two independent motions read as more "alive" than a single
/// linear sweep) and a key/description menu below it (LazyVim's own
/// dashboard structure). No item count here: it used to sit as a
/// permanent footer line, which read as the app announcing a number
/// nobody asked for every time this screen was on; 'I' surfaces it on
/// demand instead. `phase`/`elapsed` are advanced by the caller each
/// tick. `logo_index` selects which of `logo_lines`' entries to draw;
/// `focused` shows the `<`/`>` cycle hint, only meaningful while the
/// workspace actually has navigation focus (see `main::Focus`).
pub struct Banner {
    pub phase: f32,
    pub elapsed_secs: f32,
    pub logo_index: usize,
    pub focused: bool,
}

/// 0..1, slow breathing.
pub fn pulse_level(elapsed_secs: f32) -> f32 {
    (elapsed_secs * 1.3).sin() * 0.5 + 0.5
}

impl Widget for Banner {
    fn render(self, area: Rect, buf: &mut Buffer) {
        let lines = logo_lines(self.logo_index);
        let glyph_height = lines.len() as u16;
        let total_width = lines.iter().map(|l| l.chars().count()).max().unwrap_or(0) as u16;
        let start_x = area.x + area.width.saturating_sub(total_width) / 2;

        // logo, one gap row, the menu.
        let frame_height = glyph_height + 2 + MENU.len() as u16;
        let logo_start_y = area.y + area.height.saturating_sub(frame_height) / 2;

        let pulse = pulse_level(self.elapsed_secs);

        if self.focused {
            let arrow_y = logo_start_y + glyph_height / 2;
            if start_x > area.x {
                put(buf, start_x - 1, arrow_y, '<', shade(NEAR_WHITE));
            }
            let right_x = start_x + total_width;
            if right_x < area.x + area.width {
                put(buf, right_x, arrow_y, '>', shade(NEAR_WHITE));
            }
        }

        for (row_idx, row) in lines.iter().enumerate() {
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

        // LazyVim's own menu is a left-aligned column (key, two
        // spaces, description) centered as one block; centering each
        // row independently (by that row's own text length) is what
        // produced the jagged, staircase-looking menu seen before this
        // fix, since "n  navidrome profiles" and "q  quit" don't share
        // a length. Pad every key to the longest key's width first so
        // the description column itself lines up too.
        let menu_start_y = logo_start_y + glyph_height + 2;
        let key_width = MENU.iter().map(|(key, _)| key.len()).max().unwrap_or(0);
        let lines: Vec<String> = MENU
            .iter()
            .map(|(key, desc)| format!("{key:<key_width$}  {desc}"))
            .collect();
        let block_width = lines.iter().map(|l| l.chars().count()).max().unwrap_or(0) as u16;
        let block_start_x = area.x + area.width.saturating_sub(block_width) / 2;
        for (i, line) in lines.iter().enumerate() {
            draw_left(
                buf,
                area,
                block_start_x,
                menu_start_y + i as u16,
                line,
                shade(DIM),
            );
        }
    }
}

fn put(buf: &mut Buffer, x: u16, y: u16, ch: char, color: ratatui::style::Color) {
    if let Some(cell) = buf.cell_mut((x, y)) {
        cell.set_char(ch);
        cell.set_fg(color);
    }
}

fn draw_left(
    buf: &mut Buffer,
    area: Rect,
    start_x: u16,
    y: u16,
    text: &str,
    color: ratatui::style::Color,
) {
    if y >= area.y + area.height {
        return;
    }
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
