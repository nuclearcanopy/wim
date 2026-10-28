use std::time::Duration;

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::widgets::Widget;

use crate::anim;
use crate::keymap::{key_label, KeyAction, Keymap};
use crate::theme::{lerp, shade, BRIGHT, DIM, NEAR_WHITE, WHITE};
use crate::ui::logos::LOGOS;
use crate::ui::textfit::width;

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

/// Action, description; same `snacks.dashboard.Item` idea LazyVim's own
/// dashboard menu uses below its header, minus the nerd-font icons
/// (same monochrome/plain-ascii reasoning as the result-row `[sk]`
/// codes: a patched-font glyph the user's terminal doesn't have would
/// just show as a broken box, not a silent icon). Actions rather than
/// key literals: every one of these is remappable, so the key column
/// is resolved from the live keymap at render time.
const MENU: &[(KeyAction, &str)] = &[
    (KeyAction::StartSearch, "search"),
    (KeyAction::OpenNavManager, "navidrome profiles"),
    (KeyAction::OpenStarredView, "starred songs"),
    (KeyAction::IndexBreakdown, "index breakdown"),
    (KeyAction::OpenSettingsMenu, "settings"),
    (KeyAction::OpenHelp, "help"),
    (KeyAction::Quit, "quit"),
];

/// Logo with a brightness sweep plus a slower overall pulse layered on
/// top (two independent motions read as more "alive" than a single
/// linear sweep) and a key/description menu below it (LazyVim's own
/// dashboard structure). No item count here: it used to sit as a
/// permanent footer line, which read as the app announcing a number
/// nobody asked for every time this screen was on; 'I' surfaces it on
/// demand instead. Both motions come off `elapsed` (time since app
/// start) via `anim`, so they run at the same speed on any display.
/// `logo_index` selects which of `logo_lines`' entries to draw.
pub struct Banner<'a> {
    pub elapsed: Duration,
    pub logo_index: usize,
    pub keymap: &'a Keymap,
}

impl Widget for Banner<'_> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        let lines = logo_lines(self.logo_index);
        let glyph_height = lines.len() as u16;
        let total_width = lines.iter().map(|l| l.chars().count()).max().unwrap_or(0) as u16;
        let start_x = area.x + area.width.saturating_sub(total_width) / 2;

        // logo, one gap row, the menu.
        let frame_height = glyph_height + 2 + MENU.len() as u16;
        let logo_start_y = area.y + area.height.saturating_sub(frame_height) / 2;

        let phase = anim::phase(self.elapsed, anim::BANNER_SWEEP_PERIOD);
        let pulse = anim::pulse(self.elapsed, anim::BANNER_PULSE_PERIOD);

        // a blank column of breathing room on each side; sitting
        // right up against the logo's own edge read as touching it.
        const ARROW_GAP: u16 = 2;
        let arrow_y = logo_start_y + glyph_height / 2;
        if start_x >= area.x + ARROW_GAP {
            put(buf, start_x - ARROW_GAP, arrow_y, '<', shade(NEAR_WHITE));
        }
        let right_x = start_x + total_width + ARROW_GAP - 1;
        if right_x < area.x + area.width {
            put(buf, right_x, arrow_y, '>', shade(NEAR_WHITE));
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
                let sweep = ((rel - phase).rem_euclid(1.0) - 0.5).abs() * 2.0;
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
        // the description column itself lines up too; that width comes
        // from the resolved labels, since a rebound key ("f5") is a
        // different length than the default it replaced.
        let menu_start_y = logo_start_y + glyph_height + 2;
        let menu: Vec<(String, &str)> = MENU
            .iter()
            .map(|(action, desc)| (key_label(self.keymap.get(*action)), *desc))
            .collect();
        // padded by display width, by hand: `{key:<n$}` counts chars
        // and `key.len()` counts bytes, and a key bound to a wide or
        // multi-byte char is neither, which left the description
        // column jagged for exactly the reason f5 once did.
        let key_width = menu.iter().map(|(key, _)| width(key)).max().unwrap_or(0);
        let lines: Vec<String> = menu
            .iter()
            .map(|(key, desc)| {
                let pad = " ".repeat(key_width - width(key));
                format!("{key}{pad}  {desc}")
            })
            .collect();
        // display width, not char count: a key bound to a wide char
        // makes its row two cells wider than it counts, which would
        // centre the whole block left by that overhang.
        let block_width = lines.iter().map(|l| width(l)).max().unwrap_or(0) as u16;
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
    crate::ui::draw::put(buf, x, y, ch, color);
}

/// Menu rows only. The logo grid above keeps its own loop on purpose:
/// it is figlet art, one cell per char by construction.
fn draw_left(
    buf: &mut Buffer,
    area: Rect,
    start_x: u16,
    y: u16,
    text: &str,
    color: ratatui::style::Color,
) {
    if y < area.y || y >= area.y + area.height {
        return;
    }
    let right = area.x + area.width;
    crate::ui::draw::at(
        buf,
        start_x,
        y,
        text,
        color,
        right.saturating_sub(start_x) as usize,
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::KeyCode;

    fn render_to_string(keymap: &Keymap) -> String {
        let area = Rect::new(0, 0, 60, 24);
        let mut buf = Buffer::empty(area);
        Banner {
            elapsed: Duration::ZERO,
            logo_index: 0,
            keymap,
        }
        .render(area, &mut buf);
        (0..area.height)
            .map(|y| {
                (0..area.width)
                    .map(|x| buf.cell((x, y)).unwrap().symbol().to_string())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn the_menu_shows_rebound_keys_not_the_defaults() {
        let mut keymap = Keymap::default();
        keymap.set(KeyAction::StartSearch, KeyCode::Char('x'));
        keymap.set(KeyAction::OpenHelp, KeyCode::Char('y'));
        let out = render_to_string(&keymap);
        assert!(out.contains("x  search"), "got:\n{out}");
        assert!(out.contains("y  help"), "got:\n{out}");
        assert!(!out.contains("/  search"), "stale default key:\n{out}");
        assert!(!out.contains("?  help"), "stale default key:\n{out}");
    }

    #[test]
    fn the_key_column_is_padded_from_the_resolved_labels() {
        // "f5" is wider than every default, so every other row has to
        // widen with it or the description column goes jagged again.
        let mut keymap = Keymap::default();
        keymap.set(KeyAction::Quit, KeyCode::F(5));
        let out = render_to_string(&keymap);
        assert!(out.contains("f5  quit"), "got:\n{out}");
        assert!(out.contains("/   search"), "got:\n{out}");
    }

    use crate::ui::testbuf::{assert_outside_untouched, buffer, grid};

    fn render(area: Rect, buf: &mut Buffer, keymap: &Keymap) {
        Banner {
            elapsed: Duration::ZERO,
            logo_index: 0,
            keymap,
        }
        .render(area, buf);
    }

    /// Whole-screen snapshot, logo included: the figlet grid keeps its
    /// own one-cell-per-char loop on purpose, so this also pins that
    /// nothing about it moved.
    #[test]
    fn the_default_banner_renders_logo_arrows_and_menu() {
        let mut buf = buffer(44, 18);
        render(Rect::new(0, 0, 44, 18), &mut buf, &Keymap::default());
        assert_eq!(
            grid(&buf),
            [
                "............................................",
                "..........██╗....██╗██╗███╗...███╗..........",
                "..........██║....██║██║████╗.████║..........",
                "..........██║.█╗.██║██║██╔████╔██║..........",
                "........<.██║███╗██║██║██║╚██╔╝██║.>........",
                "..........╚███╔███╔╝██║██║.╚═╝.██║..........",
                "...........╚══╝╚══╝.╚═╝╚═╝.....╚═╝..........",
                "............................................",
                "............................................",
                ".........../  search........................",
                "...........n  navidrome profiles............",
                "...........S  starred songs.................",
                "...........I  index breakdown...............",
                "...........u  settings......................",
                "...........?  help..........................",
                "...........q  quit..........................",
                "............................................",
                "............................................",
            ]
            .join("\n")
        );
    }

    /// The menu block is centred and left-aligned as one block, with
    /// every row starting at the same column.
    #[test]
    fn the_menu_block_is_one_left_aligned_column() {
        let mut buf = buffer(60, 24);
        render(Rect::new(0, 0, 60, 24), &mut buf, &Keymap::default());
        let g = grid(&buf);
        let rows: Vec<&str> = g
            .lines()
            .filter(|r| MENU.iter().any(|(_, desc)| r.contains(desc)))
            .collect();
        assert_eq!(rows.len(), MENU.len(), "one row per menu item:\n{g}");
        let first = rows[0].find(|c: char| c != '.').unwrap();
        for row in &rows {
            assert_eq!(
                row.find(|c: char| c != '.'),
                Some(first),
                "menu rows are not aligned:\n{g}"
            );
        }
    }

    /// A key bound to a wide char is two cells; its row has to advance
    /// by two, and the block has to stay centred by cells.
    #[test]
    fn a_wide_key_label_does_not_shift_its_own_description() {
        let mut keymap = Keymap::default();
        keymap.set(KeyAction::StartSearch, KeyCode::Char('日'));
        let mut buf = buffer(60, 24);
        render(Rect::new(0, 0, 60, 24), &mut buf, &keymap);
        let g = grid(&buf);
        let row = g.lines().find(|r| r.contains("search")).unwrap();
        let col = row.find('日').expect("the wide key");
        // the wide glyph owns the cell after it, then the two-space
        // gutter, then the description.
        assert_eq!(row.chars().nth(col + 1), Some(' '), "{row}");
        assert_eq!(
            &row[row.char_indices().nth(col + 2).unwrap().0..][..8],
            "  search"
        );
    }

    #[test]
    fn the_menu_stays_inside_the_rect() {
        let mut buf = buffer(60, 30);
        let area = Rect::new(4, 2, 50, 26);
        let mut keymap = Keymap::default();
        keymap.set(KeyAction::StartSearch, KeyCode::Char('日'));
        render(area, &mut buf, &keymap);
        assert_outside_untouched(&buf, area);
    }

    #[test]
    fn degenerate_sizes_do_not_panic() {
        for (w, h) in [(1, 1), (1, 40), (2, 1), (40, 0), (0, 0), (3, 2), (26, 4)] {
            let mut buf = buffer(w.max(1), h.max(1));
            render(Rect::new(0, 0, w, h), &mut buf, &Keymap::default());
        }
    }
}
