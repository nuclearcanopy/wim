//! The bottom-bar crossfade popover and its shared slider geometry.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::widgets::{Block, Borders, Clear, Widget};

use crate::config::{crossfade_seconds, CROSSFADE_MAX, CROSSFADE_MIN};
use crate::theme::{shade, BRIGHT, DIM, NEAR_BLACK};

#[derive(Clone, Copy, Debug)]
pub struct PopupLayout {
    pub popup: Rect,
    pub slider: Option<Rect>,
    pub toggle: Option<Rect>,
}

pub fn layout(screen: Rect, bottom: Rect) -> PopupLayout {
    let available = bottom.y.saturating_sub(screen.y);
    let height = available.min(6);
    let width = screen.width.min(48);
    let popup = Rect::new(
        screen.x + (screen.width - width) / 2,
        bottom.y.saturating_sub(height),
        width,
        height,
    );
    let slider =
        (width >= 8 && height >= 5).then(|| Rect::new(popup.x + 2, popup.y + 3, width - 4, 1));
    let toggle =
        (width >= 4 && height >= 3).then(|| Rect::new(popup.x + 1, popup.y + 1, width - 2, 1));
    PopupLayout {
        popup,
        slider,
        toggle,
    }
}

pub fn seconds_at(slider: Rect, column: u16) -> f64 {
    let fraction = if slider.width <= 1 {
        0.0
    } else {
        column.saturating_sub(slider.x).min(slider.width - 1) as f64 / (slider.width - 1) as f64
    };
    crossfade_seconds(
        ((CROSSFADE_MIN + fraction * (CROSSFADE_MAX - CROSSFADE_MIN)) * 2.0).round() / 2.0,
    )
}

pub fn contains(rect: Rect, x: u16, y: u16) -> bool {
    x >= rect.x
        && x < rect.x.saturating_add(rect.width)
        && y >= rect.y
        && y < rect.y.saturating_add(rect.height)
}

/// A drag starts on the slider, then keeps the pointer until release.
pub fn drag_seconds(
    slider: Rect,
    dragging: &mut bool,
    kind: crate::hit::Kind,
    column: u16,
    row: u16,
) -> Option<f64> {
    use crate::hit::Kind;
    if kind == Kind::Down && contains(slider, column, row) {
        *dragging = true;
    }
    if !*dragging || !matches!(kind, Kind::Down | Kind::Drag | Kind::Up) {
        return None;
    }
    if kind == Kind::Up {
        *dragging = false;
    }
    Some(seconds_at(slider, column))
}

pub struct CrossfadePopup {
    pub layout: PopupLayout,
    pub enabled: bool,
    pub seconds: f64,
}

impl Widget for CrossfadePopup {
    fn render(self, _: Rect, buf: &mut Buffer) {
        let popup = self.layout.popup;
        if popup.width == 0 || popup.height == 0 {
            return;
        }
        Clear.render(popup, buf);
        Block::default()
            .borders(Borders::ALL)
            .title(" crossfade ")
            .style(Style::default().fg(shade(BRIGHT)).bg(shade(NEAR_BLACK)))
            .render(popup, buf);
        if let Some(row) = self.layout.toggle {
            let label = format!(
                "{}  ·  {:.1}s  (space: toggle)",
                if self.enabled { "ON" } else { "OFF" },
                self.seconds
            );
            buf.set_stringn(
                row.x,
                row.y,
                label,
                row.width as usize,
                Style::default().fg(shade(BRIGHT)),
            );
        }
        if popup.height >= 5 && popup.width >= 4 {
            buf.set_stringn(
                popup.x + 2,
                popup.y + 2,
                "fade out / in (no overlap)",
                popup.width.saturating_sub(4) as usize,
                Style::default().fg(shade(DIM)),
            );
        }
        if let Some(slider) = self.layout.slider {
            let frac =
                (crossfade_seconds(self.seconds) - CROSSFADE_MIN) / (CROSSFADE_MAX - CROSSFADE_MIN);
            let head = (frac * slider.width.saturating_sub(1) as f64).round() as u16;
            for i in 0..slider.width {
                let ch = if i == head {
                    '●'
                } else if i < head {
                    '━'
                } else {
                    '─'
                };
                buf.set_string(
                    slider.x + i,
                    slider.y,
                    ch.to_string(),
                    Style::default().fg(shade(if i <= head { BRIGHT } else { DIM })),
                );
            }
        }
        if popup.height >= 6 && popup.width >= 4 {
            buf.set_stringn(
                popup.x + 2,
                popup.y + 4,
                "1–15s · drag or ←/→ · esc closes",
                popup.width.saturating_sub(4) as usize,
                Style::default().fg(shade(DIM)),
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slider_maps_and_clamps_both_ends() {
        let track = Rect::new(10, 2, 29, 1);
        assert_eq!(seconds_at(track, 0), CROSSFADE_MIN);
        assert_eq!(seconds_at(track, 24), 8.0);
        assert_eq!(seconds_at(track, 100), CROSSFADE_MAX);
    }

    #[test]
    fn dragging_stays_captured_until_release_and_clamps_outside() {
        use crate::hit::Kind;
        let slider = Rect::new(10, 2, 29, 1);
        let mut dragging = false;
        assert_eq!(drag_seconds(slider, &mut dragging, Kind::Down, 9, 2), None);
        assert_eq!(
            drag_seconds(slider, &mut dragging, Kind::Down, 10, 2),
            Some(1.0)
        );
        assert!(dragging);
        assert_eq!(
            drag_seconds(slider, &mut dragging, Kind::Drag, 100, 20),
            Some(15.0)
        );
        assert_eq!(
            drag_seconds(slider, &mut dragging, Kind::Up, 24, 20),
            Some(8.0)
        );
        assert!(!dragging);
        assert_eq!(drag_seconds(slider, &mut dragging, Kind::Drag, 10, 2), None);
    }

    #[test]
    fn popup_sits_above_the_bottom_bar_even_when_squeezed() {
        for width in 0..50 {
            for height in 0..12 {
                let screen = Rect::new(0, 0, width, height);
                let bottom = Rect::new(0, height.saturating_sub(2), width, height.min(2));
                let l = layout(screen, bottom);
                assert!(l.popup.y + l.popup.height <= bottom.y);
                assert!(l.popup.x + l.popup.width <= screen.width);
                let mut buf = Buffer::empty(screen);
                CrossfadePopup {
                    layout: l,
                    enabled: true,
                    seconds: 3.0,
                }
                .render(screen, &mut buf);
            }
        }
    }

    #[test]
    fn popup_displays_the_state_duration_and_slider() {
        let screen = Rect::new(0, 0, 80, 24);
        let popup = layout(screen, Rect::new(0, 20, 80, 4));
        let mut buf = Buffer::empty(screen);
        CrossfadePopup {
            layout: popup,
            enabled: true,
            seconds: 3.0,
        }
        .render(screen, &mut buf);
        let line = |y| {
            (popup.popup.x..popup.popup.x + popup.popup.width)
                .map(|x| buf.cell((x, y)).unwrap().symbol())
                .collect::<String>()
        };
        assert!(line(popup.popup.y).contains("crossfade"));
        assert!(line(popup.popup.y + 1).contains("ON"));
        assert!(line(popup.popup.y + 1).contains("3.0s"));
        assert!(line(popup.slider.unwrap().y).contains('●'));
    }
}
