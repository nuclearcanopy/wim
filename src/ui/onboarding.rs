use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::widgets::Widget;

use crate::theme::{shade, DIM, NEAR_WHITE};
use crate::ui::textfit::{tail_fit, truncate_with_ellipsis};

const CONTROLS: &[&str] = &[
    "/        start or edit the live search query",
    "j / k    move selection   (down/up also work)",
    "gg / G   jump to first / last result",
    "enter    play the selected result immediately",
    "a        add the selected result to the queue",
    "l        toggle loop on the current track",
    "s        toggle shuffle (reshuffles the queue)",
    "[ / ]    speed down / up",
    "e        cycle eq preset",
    "L        fetch and show lyrics for what's playing",
    "n        reopen navidrome setup",
    "q / esc  quit",
];

pub struct WelcomeView;

impl Widget for WelcomeView {
    fn render(self, area: Rect, buf: &mut Buffer) {
        draw_centered(buf, area, area.y, "welcome to wim", shade(NEAR_WHITE));
        draw_centered(
            buf,
            area,
            area.y + 2,
            "a terminal omniplayer for your music and video",
            shade(DIM),
        );
        draw_centered(buf, area, area.y + 4, "press any key to set things up", shade(DIM));
    }
}

pub struct ControlsView;

impl Widget for ControlsView {
    fn render(self, area: Rect, buf: &mut Buffer) {
        draw_line(buf, area, area.y, "controls", shade(NEAR_WHITE));
        for (i, line) in CONTROLS.iter().enumerate() {
            draw_line(buf, area, area.y + 2 + i as u16, line, shade(DIM));
        }
        draw_line(
            buf,
            area,
            area.y + 3 + CONTROLS.len() as u16,
            "press any key to continue",
            shade(DIM),
        );
    }
}

pub struct LibraryPathsView<'a> {
    pub paths: &'a [String],
    pub input: &'a str,
}

impl Widget for LibraryPathsView<'_> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        let width = area.width as usize;
        draw_line(buf, area, area.y, "where's your media?", shade(NEAR_WHITE));
        let mut y = area.y + 2;
        if self.paths.is_empty() {
            draw_line(buf, area, y, "(no paths found — type one below, or skip to browse nothing yet)", shade(DIM));
            y += 1;
        } else {
            for p in self.paths {
                draw_line(buf, area, y, &format!("  {}", truncate_with_ellipsis(p, width.saturating_sub(2))), shade(DIM));
                y += 1;
            }
        }
        y += 1;
        let prefix = "add path: ";
        let value_width = width.saturating_sub(prefix.chars().count());
        draw_line(buf, area, y, &format!("{prefix}{}", tail_fit(self.input, value_width)), shade(NEAR_WHITE));
        y += 2;
        draw_line(buf, area, y, "enter to add, enter on empty to continue", shade(DIM));
    }
}

pub struct DoneView;

impl Widget for DoneView {
    fn render(self, area: Rect, buf: &mut Buffer) {
        draw_centered(buf, area, area.y, "you're all set", shade(NEAR_WHITE));
        draw_centered(buf, area, area.y + 2, "press any key to start", shade(DIM));
    }
}

fn draw_line(buf: &mut Buffer, area: Rect, y: u16, text: &str, color: ratatui::style::Color) {
    if y >= area.y + area.height {
        return;
    }
    let fitted = truncate_with_ellipsis(text, area.width as usize);
    for (col, ch) in fitted.chars().enumerate() {
        let x = area.x + col as u16;
        if x >= area.x + area.width {
            break;
        }
        if let Some(cell) = buf.cell_mut((x, y)) {
            cell.set_char(ch);
            cell.set_fg(color);
        }
    }
}

fn draw_centered(buf: &mut Buffer, area: Rect, y: u16, text: &str, color: ratatui::style::Color) {
    if y >= area.y + area.height {
        return;
    }
    let text = truncate_with_ellipsis(text, area.width as usize);
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
