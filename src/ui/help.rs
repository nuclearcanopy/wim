use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::widgets::Widget;

use crate::keymap::{key_label, KeyAction, Keymap};
use crate::theme::{shade, DIM, NEAR_BLACK, NEAR_WHITE};
use crate::ui::header::draw_header;

/// Full keybind reference, opened with `?` (hinted by `HelpBox` in the
/// bottom-right corner); takes over the whole screen the same way
/// onboarding/the nav manager do, since it's a brief reference lookup
/// rather than something you'd want to see alongside anything else.
/// Looks up every remappable entry's *current* key from `keymap`
/// live, rather than a hardcoded default string that would silently
/// go stale the moment anything got rebound via 'K'.
pub struct HelpOverlay<'a> {
    pub keymap: &'a Keymap,
}

/// `Fixed` for universal navigation primitives and modal-internal
/// conventions that deliberately aren't part of the remappable
/// keymap (see keymap.rs); `Bound` for anything that is, so its
/// label always reflects whatever it's actually currently bound to.
enum Key {
    Fixed(&'static str),
    Bound(KeyAction),
}

const SECTIONS: &[(&str, &[(Key, &str)])] = &[
    (
        "navigation",
        &[
            (Key::Bound(KeyAction::StartSearch), "search"),
            (Key::Fixed("j / k"), "move selection"),
            (Key::Fixed("gg / G"), "jump to top / bottom"),
            (Key::Fixed("enter"), "play / open"),
            (Key::Fixed("esc"), "back / close"),
            (Key::Fixed("tab"), "enter move mode (focus a pane)"),
            (
                Key::Fixed("wasd / arrows"),
                "move mode: focus pane left/down/up/right",
            ),
            (
                Key::Fixed("enter / esc"),
                "move mode: back to interact mode",
            ),
            (Key::Fixed("left/right"), "cycle home screen logo"),
            (Key::Fixed("0"), "reset logo to default"),
        ],
    ),
    (
        "playback",
        &[
            (Key::Bound(KeyAction::TogglePause), "pause / resume"),
            (Key::Bound(KeyAction::Enqueue), "queue"),
            (Key::Bound(KeyAction::SkipNext), "skip to next"),
            (Key::Bound(KeyAction::SkipBack), "back to previous"),
            (
                Key::Bound(KeyAction::CycleLoop),
                "cycle loop: off / one / all",
            ),
            (Key::Bound(KeyAction::ToggleShuffle), "shuffle"),
            (Key::Fixed("[ / ]"), "speed"),
            (Key::Bound(KeyAction::CycleEq), "cycle eq preset"),
            (Key::Bound(KeyAction::ToggleCrossfade), "toggle crossfade"),
            (Key::Fixed(", / ."), "subtitle delay -/+ 0.5s"),
            (
                Key::Bound(KeyAction::OpenSubtitleMenu),
                "subtitle settings menu",
            ),
        ],
    ),
    (
        "library",
        &[
            (Key::Bound(KeyAction::OpenNavManager), "navidrome profiles"),
            (Key::Bound(KeyAction::OpenStarredView), "starred songs"),
            (Key::Bound(KeyAction::IndexBreakdown), "index breakdown"),
            (Key::Bound(KeyAction::ToggleStar), "star / unstar"),
            (
                Key::Bound(KeyAction::DownloadSelected),
                "download selected (for offline)",
            ),
            (Key::Bound(KeyAction::OpenLyrics), "lyrics"),
            (
                Key::Fixed("left/right"),
                "artist profile: switch albums/songs column",
            ),
            (
                Key::Bound(KeyAction::StartSearch),
                "artist profile: filter songs",
            ),
        ],
    ),
    (
        "view",
        &[
            (
                Key::Bound(KeyAction::ToggleMinimize),
                "minimize / maximize player",
            ),
            (
                Key::Bound(KeyAction::ToggleFullscreenVideo),
                "fullscreen video (covers the sidebar too)",
            ),
            (
                Key::Bound(KeyAction::CycleArtMode),
                "cycle album art: ascii / real / off",
            ),
            (
                Key::Bound(KeyAction::ToggleResizeMode),
                "resize mode (a/w/s/d select, arrows stretch)",
            ),
        ],
    ),
    (
        "other",
        &[
            (Key::Bound(KeyAction::OpenSettingsMenu), "settings"),
            (Key::Fixed("K"), "remap keybinds"),
            (Key::Bound(KeyAction::Quit), "quit"),
            (Key::Fixed("ctrl+c"), "quit, always, from anywhere"),
            (Key::Bound(KeyAction::OpenHelp), "close this"),
        ],
    ),
];

impl Widget for HelpOverlay<'_> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        let mut y = draw_header(
            buf,
            area,
            area.y,
            "keybinds",
            "? or esc to close",
            shade(DIM),
            shade(NEAR_BLACK),
        );
        y += 1;

        for (section, binds) in SECTIONS {
            if y >= area.y + area.height {
                break;
            }
            draw_line(buf, area, y, section, shade(NEAR_WHITE));
            y += 1;
            for (key, desc) in *binds {
                if y >= area.y + area.height {
                    break;
                }
                let key_text = match key {
                    Key::Fixed(s) => s.to_string(),
                    Key::Bound(action) => key_label(self.keymap.get(*action)),
                };
                draw_line(
                    buf,
                    area,
                    y,
                    &format!("  {key_text:<10} {desc}"),
                    shade(DIM),
                );
                y += 1;
            }
            y += 1;
        }
    }
}

fn draw_line(buf: &mut Buffer, area: Rect, y: u16, text: &str, color: ratatui::style::Color) {
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
