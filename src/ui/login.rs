use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::widgets::Widget;

use crate::theme::{shade, DIM, NEAR_BLACK, NEAR_WHITE};
use crate::ui::header::draw_header;
use crate::ui::textfit::{tail_fit, truncate_with_ellipsis};

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

    /// Tab cycles with wraparound (standard tab-navigation convention).
    pub fn next_field(&mut self) {
        self.field = (self.field + 1) % 3;
    }

    /// Arrow-key navigation is non-wrapping; stops at the first/last
    /// field rather than cycling, the more expected behavior for
    /// up/down specifically (and for backspace-on-empty going back to
    /// fix a typo without having to tab all the way around).
    pub fn prev_field(&mut self) {
        self.field = self.field.saturating_sub(1);
    }

    pub fn next_field_clamped(&mut self) {
        self.field = (self.field + 1).min(2);
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
            [("server", &self.form.url), ("username", &self.form.username), ("password", &masked_password)];

        let width = area.width as usize;
        let mut y = draw_header(
            buf,
            area,
            area.y,
            "navidrome setup",
            "enter to advance, esc to skip (local files only)",
            shade(DIM),
            shade(NEAR_BLACK),
        );
        y += 1;

        for (i, (label, value)) in rows.iter().enumerate() {
            let active = i == self.form.field;
            let marker = if active { "> " } else { "  " };
            let prefix = format!("{marker}{label}: ");
            let active_color = if active { shade(NEAR_WHITE) } else { shade(DIM) };

            if i == 0 {
                // url field: "https://" renders as a ghost prefix baked
                // into the box itself, like it's already typed; not a
                // separate label note. skipped if the user typed a
                // scheme themselves, so it never doubles up.
                let has_own_scheme = value.to_ascii_lowercase().starts_with("http://")
                    || value.to_ascii_lowercase().starts_with("https://");
                let ghost = if has_own_scheme { "" } else { "https://" };
                let prefix_width = prefix.chars().count() + ghost.chars().count();
                let value_width = width.saturating_sub(prefix_width);
                let fitted_value = tail_fit(value, value_width);

                draw_line(buf, area, y, &prefix, active_color);
                draw_line_at(buf, area, prefix.chars().count() as u16, y, ghost, shade(DIM));
                draw_line_at(
                    buf,
                    area,
                    (prefix.chars().count() + ghost.chars().count()) as u16,
                    y,
                    &fitted_value,
                    active_color,
                );
            } else {
                // the value is what's actively being typed; keep its
                // tail (cursor end) visible rather than truncating it
                // away, same as the search query in the status bar.
                let value_width = width.saturating_sub(prefix.chars().count());
                let line = format!("{prefix}{}", tail_fit(value, value_width));
                draw_line(buf, area, y, &line, active_color);
            }
            y += 1;
        }

        y += 1;
        // paste isn't a wim keybind; it's whatever your terminal
        // emulator binds (often ctrl+shift+v, sometimes middle-click).
        // bracketed paste is enabled on our end either way, so if your
        // terminal supports it, it'll just work.
        draw_line(buf, area, y, "paste with your terminal's shortcut (often ctrl+shift+v)", shade(DIM));
        y += 1;
        draw_line(buf, area, y, &truncate_with_ellipsis(self.message, width), shade(DIM));
    }
}

fn draw_line(buf: &mut Buffer, area: Rect, y: u16, text: &str, color: ratatui::style::Color) {
    draw_line_at(buf, area, 0, y, text, color);
}

fn draw_line_at(buf: &mut Buffer, area: Rect, x_offset: u16, y: u16, text: &str, color: ratatui::style::Color) {
    if y >= area.y + area.height {
        return;
    }
    for (col, ch) in text.chars().enumerate() {
        let x = area.x + x_offset + col as u16;
        if x >= area.x + area.width {
            break;
        }
        if let Some(cell) = buf.cell_mut((x, y)) {
            cell.set_char(ch);
            cell.set_fg(color);
        }
    }
}
