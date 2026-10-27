use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::widgets::Widget;

use crate::theme::{shade, DIM, NEAR_WHITE};

/// Interactive Navidrome setup, shown on first run instead of requiring
/// a hand-edited config.toml before the app is even usable. Esc skips
/// it entirely (local files still work with no server configured).
pub struct LoginForm {
    pub url: String,
    pub username: String,
    pub password: String,
    pub field: usize,
}

impl LoginForm {
    pub fn new() -> Self {
        LoginForm { url: String::new(), username: String::new(), password: String::new(), field: 0 }
    }

    pub fn current_mut(&mut self) -> &mut String {
        match self.field {
            0 => &mut self.url,
            1 => &mut self.username,
            _ => &mut self.password,
        }
    }

    pub fn next_field(&mut self) {
        self.field = (self.field + 1) % 3;
    }

    pub fn is_last_field(&self) -> bool {
        self.field == 2
    }
}

impl Default for LoginForm {
    fn default() -> Self {
        Self::new()
    }
}

pub struct LoginView<'a> {
    pub form: &'a LoginForm,
    pub message: &'a str,
}

impl Widget for LoginView<'_> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        let masked_password: String = "*".repeat(self.form.password.chars().count());
        let rows: [(&str, &str); 3] =
            [("navidrome url", &self.form.url), ("username", &self.form.username), ("password", &masked_password)];

        let mut y = area.y;
        draw_line(buf, area, y, "navidrome setup — enter to advance, esc to skip (local files only)", shade(DIM));
        y += 1;
        draw_line(buf, area, y, &"─".repeat(area.width as usize), shade(DIM));
        y += 2;

        for (i, (label, value)) in rows.iter().enumerate() {
            let active = i == self.form.field;
            let marker = if active { "> " } else { "  " };
            let line = format!("{marker}{label}: {value}");
            draw_line(buf, area, y, &line, if active { shade(NEAR_WHITE) } else { shade(DIM) });
            y += 1;
        }

        y += 1;
        draw_line(buf, area, y, self.message, shade(DIM));
    }
}

fn draw_line(buf: &mut Buffer, area: Rect, y: u16, text: &str, color: ratatui::style::Color) {
    if y >= area.y + area.height {
        return;
    }
    for (col, ch) in text.chars().enumerate() {
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
