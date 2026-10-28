use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::widgets::Widget;

use crate::theme::{shade, DIM, NEAR_BLACK, NEAR_WHITE};
use crate::ui::header::draw_header;
use crate::ui::textfit::{tail_fit, truncate_with_ellipsis, width};

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
        LoginForm {
            url: String::new(),
            username: String::new(),
            password: String::new(),
            field: 0,
        }
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
    /// `Some(step)` while this is part of first-run onboarding, which
    /// wraps it in that flow's frame and step rail so it does not read
    /// as a different screen mid-flow. `None` when reopened standalone
    /// (the navidrome key, or the profile manager's add form), where
    /// there is no flow for it to be a step of.
    pub step: Option<usize>,
}

impl Widget for LoginView<'_> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        let area = match self.step {
            Some(step) => crate::ui::onboarding::chrome(buf, area, step),
            None => area,
        };
        if area.width == 0 || area.height == 0 {
            return;
        }

        let masked_password: String = "*".repeat(self.form.password.chars().count());
        let rows: [(&str, &str); 3] = [
            ("server", &self.form.url),
            ("username", &self.form.username),
            ("password", &masked_password),
        ];

        let area_width = area.width as usize;
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
            let active_color = if active {
                shade(NEAR_WHITE)
            } else {
                shade(DIM)
            };

            if i == 0 {
                // url field: "https://" renders as a ghost prefix baked
                // into the box itself, like it's already typed; not a
                // separate label note. skipped if the user typed a
                // scheme themselves, so it never doubles up.
                let has_own_scheme = value.to_ascii_lowercase().starts_with("http://")
                    || value.to_ascii_lowercase().starts_with("https://");
                let ghost = if has_own_scheme { "" } else { "https://" };
                let prefix_width = width(&prefix) + width(ghost);
                let value_width = area_width.saturating_sub(prefix_width);
                let fitted_value = tail_fit(value, value_width);

                // three runs in two brightness levels on one row, so
                // each starts where the previous writer stopped; a char
                // count would land the url inside the ghost's last
                // glyph for any wide character.
                let after_prefix = draw_run(buf, area, area.x, y, &prefix, active_color);
                let after_ghost = draw_run(buf, area, after_prefix, y, ghost, shade(DIM));
                draw_run(buf, area, after_ghost, y, &fitted_value, active_color);
            } else {
                // the value is what's actively being typed; keep its
                // tail (cursor end) visible rather than truncating it
                // away, same as the search query in the status bar.
                let value_width = area_width.saturating_sub(width(&prefix));
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
        draw_line(
            buf,
            area,
            y,
            "paste with your terminal's shortcut (often ctrl+shift+v)",
            shade(DIM),
        );
        y += 1;
        draw_line(
            buf,
            area,
            y,
            &truncate_with_ellipsis(self.message, area_width),
            shade(DIM),
        );
    }
}

fn draw_line(buf: &mut Buffer, area: Rect, y: u16, text: &str, color: ratatui::style::Color) {
    crate::ui::draw::line(buf, area, y, text, color);
}

/// One run of a multi-brightness row, returning the x it stopped at so
/// the next run starts on a cell boundary rather than a char count.
fn draw_run(
    buf: &mut Buffer,
    area: Rect,
    x: u16,
    y: u16,
    text: &str,
    color: ratatui::style::Color,
) -> u16 {
    if y < area.y || y >= area.y + area.height {
        return x;
    }
    let right = area.x + area.width;
    crate::ui::draw::at(buf, x, y, text, color, right.saturating_sub(x) as usize)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::testbuf::{assert_outside_untouched, buffer, grid};

    fn form(url: &str, username: &str, password: &str, field: usize) -> LoginForm {
        LoginForm {
            url: url.to_string(),
            username: username.to_string(),
            password: password.to_string(),
            field,
        }
    }

    fn render(area: Rect, buf: &mut Buffer, form: &LoginForm, message: &str) {
        LoginView {
            form,
            message,
            step: None,
        }
        .render(area, buf);
    }

    #[test]
    fn the_three_fields_render_with_the_ghost_scheme_and_a_masked_password() {
        let area = Rect::new(0, 0, 54, 9);
        let mut buf = buffer(54, 9);
        render(area, &mut buf, &form("", "ana", "hunter2", 0), "nope");
        assert_eq!(
            grid(&buf),
            [
                "navidrome setup │ enter to advance, esc to skip (loca…",
                "────────────────┴─────────────────────────────────────",
                "......................................................",
                "> server: https://....................................",
                "  username: ana.......................................",
                "  password: *******...................................",
                "......................................................",
                "paste with your terminal's shortcut (often ctrl+shift+",
                "nope..................................................",
            ]
            .join("\n")
        );
    }

    /// Typing a scheme drops the ghost rather than doubling it up.
    #[test]
    fn a_typed_scheme_replaces_the_ghost_instead_of_stacking_on_it() {
        let area = Rect::new(0, 0, 54, 9);
        let mut buf = buffer(54, 9);
        render(area, &mut buf, &form("https://a.example", "", "", 0), "");
        let row = grid(&buf).lines().nth(3).unwrap().to_string();
        assert!(row.starts_with("> server: https://a.example"), "{row}");
        assert!(!row.contains("https://https://"), "{row}");
    }

    /// The url is typed, so it can be non-ascii; it is written as a
    /// third run after the ghost, and must start where the ghost's last
    /// cell actually ended.
    #[test]
    fn a_wide_url_starts_after_the_ghost_and_does_not_shift_itself() {
        let area = Rect::new(0, 0, 54, 9);
        let mut buf = buffer(54, 9);
        render(area, &mut buf, &form("日本x", "", "", 0), "");
        let row: Vec<char> = grid(&buf).lines().nth(3).unwrap().chars().collect();
        // "> server: " is 10 cells, "https://" another 8.
        let start = 18;
        assert_eq!(
            (row[start], row[start + 1], row[start + 2], row[start + 3]),
            ('日', ' ', '本', ' ')
        );
        assert_eq!(row[start + 4], 'x');
    }

    /// A non-ascii username lives on a single-run row, so this covers
    /// the plain `draw::line` path alongside the ghost-prefixed one.
    #[test]
    fn a_wide_username_does_not_shift_the_glyphs_after_it() {
        let area = Rect::new(0, 0, 54, 9);
        let mut buf = buffer(54, 9);
        render(area, &mut buf, &form("", "日本x", "", 1), "");
        let row: Vec<char> = grid(&buf).lines().nth(4).unwrap().chars().collect();
        let start = 12; // "> username: "
        assert_eq!((row[start], row[start + 1]), ('日', ' '));
        assert_eq!(row[start + 4], 'x');
    }

    #[test]
    fn a_wide_url_stays_inside_the_rect() {
        let mut buf = buffer(40, 12);
        let area = Rect::new(2, 1, 20, 8);
        let f = form("日本語テスト曲日本語テスト曲", "日本語", "日本語", 0);
        render(area, &mut buf, &f, "日本語テスト曲日本語テスト曲");
        assert_outside_untouched(&buf, area);
    }

    #[test]
    fn degenerate_sizes_do_not_panic() {
        let f = form("日本語", "日本", "pw", 0);
        for (w, h) in [(1, 1), (1, 40), (2, 1), (40, 0), (0, 0), (3, 2)] {
            let mut buf = buffer(w.max(1), h.max(1));
            render(Rect::new(0, 0, w, h), &mut buf, &f, "日本語");
        }
    }

    /// Step 3 of onboarding used to be the one screen with no frame
    /// and no rail, so the flow visibly changed shape in the middle.
    #[test]
    fn the_onboarding_step_carries_the_flows_chrome() {
        let mut buf = buffer(60, 16);
        LoginView {
            form: &form("", "ana", "", 0),
            message: "",
            step: Some(crate::ui::onboarding::NAVIDROME_STEP),
        }
        .render(Rect::new(0, 0, 60, 16), &mut buf);
        let g = grid(&buf);
        let first = g.lines().next().unwrap();
        assert!(
            first.starts_with('\u{250c}') && first.ends_with('\u{2510}'),
            "no frame:\n{g}"
        );
        assert!(g.contains("step 3 of 6"), "no rail:\n{g}");
        assert!(g.contains("navidrome setup"), "content gone:\n{g}");
        assert!(g.contains("username: ana"), "fields gone:\n{g}");
    }

    /// Reopened standalone there is no flow, so claiming "step 3 of 6"
    /// would be a lie; it must look exactly as it always has.
    #[test]
    fn the_standalone_form_has_no_frame_or_rail() {
        let mut buf = buffer(60, 16);
        render(
            Rect::new(0, 0, 60, 16),
            &mut buf,
            &form("", "ana", "", 0),
            "",
        );
        let g = grid(&buf);
        assert!(!g.contains("step 3 of 6"), "{g}");
        assert!(!g.contains('\u{250c}'), "{g}");
        assert!(g.contains("navidrome setup"), "{g}");
    }

    #[test]
    fn the_chromed_form_stays_inside_its_rect_and_survives_tiny_sizes() {
        let rect = Rect::new(2, 1, 30, 10);
        let mut buf = buffer(40, 14);
        LoginView {
            form: &form("\u{65e5}\u{672c}\u{8a9e}", "\u{65e5}\u{672c}", "pw", 0),
            message: "\u{65e5}\u{672c}\u{8a9e}",
            step: Some(3),
        }
        .render(rect, &mut buf);
        assert_outside_untouched(&buf, rect);

        for (w, h) in [(1, 1), (2, 1), (5, 3), (6, 4), (40, 0), (0, 0)] {
            let mut buf = buffer(w.max(1), h.max(1));
            LoginView {
                form: &form("\u{65e5}", "a", "pw", 0),
                message: "m",
                step: Some(3),
            }
            .render(Rect::new(0, 0, w, h), &mut buf);
        }
    }
}
