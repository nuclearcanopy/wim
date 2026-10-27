//! Every globally-bound, single-purpose command key, remappable.
//!
//! Deliberately excludes the universal navigation primitives used
//! pervasively throughout the app (j/k/arrows for up/down, h/l/arrows
//! for left/right adjustment inside the subtitle/settings menus,
//! enter/esc) and a handful of fixed two-key sequences (`gg`) or
//! contextual arrow uses (the home-screen logo cycle): those are
//! conventions applied the same way everywhere, not individually
//! named commands, and letting them drift out of their own
//! convention per-action would make the rest of the app harder to
//! learn, not easier.
//!
//! Switching focus between the sidebar/workspace/bottom bar is the
//! same kind of fixed convention, not a named command either: `tab`
//! enters move mode, where wasd/arrows move focus directionally
//! (sway/i3-style, so a direction tells you where you'll end up) and
//! nothing else responds. Handled directly in `main.rs` via
//! `InputMode`, not through this keymap. Two earlier attempts are
//! worth not repeating: three absolute `KeyAction`s
//! (focus_sidebar/focus_workspace/focus_bottom_bar, each an unrelated
//! letter) taught you nothing about the layout, and plain
//! shift+direction moved focus with no indication it had, so the next
//! keypress landed somewhere unexpected.
//!
//! `reserved_key_purpose` is the machine-readable form of all of the
//! above: a binding onto one of those keys is refused rather than
//! silently shadowed at runtime.

use std::collections::HashMap;

use crossterm::event::KeyCode;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum KeyAction {
    StartSearch,
    Enqueue,
    CycleLoop,
    ToggleShuffle,
    SpeedUp,
    SpeedDown,
    CycleEq,
    ToggleCrossfade,
    SubtitleDelayDown,
    SubtitleDelayUp,
    TogglePause,
    SkipNext,
    SkipBack,
    ToggleStar,
    DownloadSelected,
    OpenLyrics,
    CycleArtMode,
    ToggleMinimize,
    ToggleFullscreenVideo,
    ToggleResizeMode,
    OpenNavManager,
    OpenStarredView,
    OpenSubtitleMenu,
    OpenSettingsMenu,
    OpenHelp,
    IndexBreakdown,
    Quit,
}

/// Every action, in the order the remap menu lists them; also what
/// `Keymap::default` and `reset_all` iterate to rebuild from scratch.
pub const ALL_ACTIONS: &[KeyAction] = &[
    KeyAction::StartSearch,
    KeyAction::Enqueue,
    KeyAction::CycleLoop,
    KeyAction::ToggleShuffle,
    KeyAction::SpeedUp,
    KeyAction::SpeedDown,
    KeyAction::CycleEq,
    KeyAction::ToggleCrossfade,
    KeyAction::SubtitleDelayDown,
    KeyAction::SubtitleDelayUp,
    KeyAction::TogglePause,
    KeyAction::SkipNext,
    KeyAction::SkipBack,
    KeyAction::ToggleStar,
    KeyAction::DownloadSelected,
    KeyAction::OpenLyrics,
    KeyAction::CycleArtMode,
    KeyAction::ToggleMinimize,
    KeyAction::ToggleFullscreenVideo,
    KeyAction::ToggleResizeMode,
    KeyAction::OpenNavManager,
    KeyAction::OpenStarredView,
    KeyAction::OpenSubtitleMenu,
    KeyAction::OpenSettingsMenu,
    KeyAction::OpenHelp,
    KeyAction::IndexBreakdown,
    KeyAction::Quit,
];

impl KeyAction {
    /// Stable, lowercase-with-underscores name: the key this action
    /// is stored under in config.toml, so a config stays valid across
    /// however the variants above get reordered.
    pub fn name(self) -> &'static str {
        match self {
            KeyAction::StartSearch => "start_search",
            KeyAction::Enqueue => "enqueue",
            KeyAction::CycleLoop => "cycle_loop",
            KeyAction::ToggleShuffle => "toggle_shuffle",
            KeyAction::SpeedUp => "speed_up",
            KeyAction::SpeedDown => "speed_down",
            KeyAction::CycleEq => "cycle_eq",
            KeyAction::ToggleCrossfade => "toggle_crossfade",
            KeyAction::SubtitleDelayDown => "subtitle_delay_down",
            KeyAction::SubtitleDelayUp => "subtitle_delay_up",
            KeyAction::TogglePause => "toggle_pause",
            KeyAction::SkipNext => "skip_next",
            KeyAction::SkipBack => "skip_back",
            KeyAction::ToggleStar => "toggle_star",
            KeyAction::DownloadSelected => "download_selected",
            KeyAction::OpenLyrics => "open_lyrics",
            KeyAction::CycleArtMode => "cycle_art_mode",
            KeyAction::ToggleMinimize => "toggle_minimize",
            KeyAction::ToggleFullscreenVideo => "toggle_fullscreen_video",
            KeyAction::ToggleResizeMode => "toggle_resize_mode",
            KeyAction::OpenNavManager => "open_navidrome_profiles",
            KeyAction::OpenStarredView => "open_starred_songs",
            KeyAction::OpenSubtitleMenu => "open_subtitle_menu",
            KeyAction::OpenSettingsMenu => "open_settings_menu",
            KeyAction::OpenHelp => "open_help",
            KeyAction::IndexBreakdown => "index_breakdown",
            KeyAction::Quit => "quit",
        }
    }

    /// Short label for the remap menu / help overlay.
    pub fn label(self) -> &'static str {
        match self {
            KeyAction::StartSearch => "search",
            KeyAction::Enqueue => "enqueue selected",
            KeyAction::CycleLoop => "cycle loop mode",
            KeyAction::ToggleShuffle => "toggle shuffle",
            KeyAction::SpeedUp => "speed up",
            KeyAction::SpeedDown => "speed down",
            KeyAction::CycleEq => "cycle eq preset",
            KeyAction::ToggleCrossfade => "toggle crossfade",
            KeyAction::SubtitleDelayDown => "subtitle delay -0.5s",
            KeyAction::SubtitleDelayUp => "subtitle delay +0.5s",
            KeyAction::TogglePause => "play / pause",
            KeyAction::SkipNext => "skip to next",
            KeyAction::SkipBack => "back to previous",
            KeyAction::ToggleStar => "star / unstar",
            KeyAction::DownloadSelected => "download selected (for offline)",
            KeyAction::OpenLyrics => "lyrics",
            KeyAction::CycleArtMode => "cycle album art mode",
            KeyAction::ToggleMinimize => "minimize / maximize player",
            KeyAction::ToggleFullscreenVideo => "fullscreen video",
            KeyAction::ToggleResizeMode => "resize mode",
            KeyAction::OpenNavManager => "navidrome profiles",
            KeyAction::OpenStarredView => "starred songs",
            KeyAction::OpenSubtitleMenu => "subtitle settings menu",
            KeyAction::OpenSettingsMenu => "settings menu",
            KeyAction::OpenHelp => "help",
            KeyAction::IndexBreakdown => "index breakdown",
            KeyAction::Quit => "quit",
        }
    }

    fn default_key(self) -> KeyCode {
        match self {
            KeyAction::StartSearch => KeyCode::Char('/'),
            KeyAction::Enqueue => KeyCode::Char('a'),
            KeyAction::CycleLoop => KeyCode::Char('l'),
            KeyAction::ToggleShuffle => KeyCode::Char('s'),
            KeyAction::SpeedUp => KeyCode::Char(']'),
            KeyAction::SpeedDown => KeyCode::Char('['),
            KeyAction::CycleEq => KeyCode::Char('e'),
            KeyAction::ToggleCrossfade => KeyCode::Char('c'),
            KeyAction::SubtitleDelayDown => KeyCode::Char(','),
            KeyAction::SubtitleDelayUp => KeyCode::Char('.'),
            KeyAction::TogglePause => KeyCode::Char(' '),
            KeyAction::SkipNext => KeyCode::Char('f'),
            KeyAction::SkipBack => KeyCode::Char('b'),
            KeyAction::ToggleStar => KeyCode::Char('*'),
            // 'o' ("offline"): every other free-standing letter with a
            // plausible mnemonic (d/g/s/...) was already taken by an
            // existing action (see this file's own default_key list).
            KeyAction::DownloadSelected => KeyCode::Char('o'),
            KeyAction::OpenLyrics => KeyCode::Char('L'),
            KeyAction::CycleArtMode => KeyCode::Char('i'),
            KeyAction::ToggleMinimize => KeyCode::Char('m'),
            KeyAction::ToggleFullscreenVideo => KeyCode::Char('v'),
            KeyAction::ToggleResizeMode => KeyCode::Char('r'),
            KeyAction::OpenNavManager => KeyCode::Char('n'),
            KeyAction::OpenStarredView => KeyCode::Char('S'),
            KeyAction::OpenSubtitleMenu => KeyCode::Char('t'),
            KeyAction::OpenSettingsMenu => KeyCode::Char('u'),
            KeyAction::OpenHelp => KeyCode::Char('?'),
            KeyAction::IndexBreakdown => KeyCode::Char('I'),
            KeyAction::Quit => KeyCode::Char('q'),
        }
    }
}

/// `KeyCode` <-> a short name stored in config.toml. Only covers what
/// an action could plausibly be remapped to (printable chars plus a
/// handful of named keys); anything else just doesn't round-trip and
/// falls back to the action's default, same as a missing/unparseable
/// entry would.
fn key_to_name(key: KeyCode) -> Option<String> {
    Some(match key {
        KeyCode::Char(' ') => "space".to_string(),
        KeyCode::Char(c) => c.to_string(),
        KeyCode::Left => "left".to_string(),
        KeyCode::Right => "right".to_string(),
        KeyCode::Up => "up".to_string(),
        KeyCode::Down => "down".to_string(),
        KeyCode::Tab => "tab".to_string(),
        KeyCode::Backspace => "backspace".to_string(),
        KeyCode::F(n) => format!("f{n}"),
        _ => return None,
    })
}

fn name_to_key(name: &str) -> Option<KeyCode> {
    match name {
        "space" => Some(KeyCode::Char(' ')),
        "left" => Some(KeyCode::Left),
        "right" => Some(KeyCode::Right),
        "up" => Some(KeyCode::Up),
        "down" => Some(KeyCode::Down),
        "tab" => Some(KeyCode::Tab),
        "backspace" => Some(KeyCode::Backspace),
        _ if name.len() == 1 => name.chars().next().map(KeyCode::Char),
        _ if name.starts_with('f') && name[1..].parse::<u8>().is_ok() => {
            name[1..].parse().ok().map(KeyCode::F)
        }
        _ => None,
    }
}

/// Human-readable form for the remap menu/help overlay, e.g. "space",
/// "f5", "shift+a" isn't representable here (case carries that for
/// letters already, same as every default above).
pub fn key_label(key: KeyCode) -> String {
    key_to_name(key).unwrap_or_else(|| "?".to_string())
}

/// What a key is permanently used for, if it's one of the fixed
/// navigation conventions rather than a remappable action. These are
/// checked before every input handler that consults the keymap, so an
/// action bound to one of them would simply never fire; refusing the
/// binding and saying why beats a key that silently does nothing.
pub fn reserved_key_purpose(key: KeyCode) -> Option<&'static str> {
    Some(match key {
        KeyCode::Char('j') | KeyCode::Char('k') => "move selection",
        KeyCode::Char('g') | KeyCode::Char('G') => "jump to top / bottom",
        KeyCode::Char('K') => "the remap menu itself",
        KeyCode::Enter => "play / open",
        KeyCode::Esc => "back / close",
        KeyCode::Tab => "move mode",
        KeyCode::Left | KeyCode::Right | KeyCode::Up | KeyCode::Down => "navigation",
        KeyCode::Backspace => "editing a query",
        _ => return None,
    })
}

#[derive(Debug, Clone)]
pub struct Keymap {
    bindings: HashMap<KeyAction, KeyCode>,
}

impl Default for Keymap {
    fn default() -> Self {
        Keymap {
            bindings: ALL_ACTIONS.iter().map(|a| (*a, a.default_key())).collect(),
        }
    }
}

impl Keymap {
    /// Starts from every default, then applies whatever `overrides`
    /// (config.toml's `[keybinds]` table, action name -> key name)
    /// parses successfully; an unrecognized action name or key name
    /// is silently ignored rather than failing the whole config load,
    /// same leniency as every other best-effort config field.
    pub fn from_overrides(overrides: &HashMap<String, String>) -> Self {
        let mut map = Self::default();
        for action in ALL_ACTIONS {
            if let Some(name) = overrides.get(action.name()) {
                if let Some(key) = name_to_key(name) {
                    // a key the fixed conventions already own would be
                    // shadowed by them at runtime, leaving the action
                    // permanently unreachable with nothing to explain
                    // why; keep the default instead.
                    if reserved_key_purpose(key).is_none() {
                        map.bindings.insert(*action, key);
                    }
                }
            }
        }
        map
    }

    /// The non-default entries, for persisting back to config.toml;
    /// empty means "every action is still at its default," which
    /// config.rs then omits entirely rather than writing out a
    /// redundant full table.
    pub fn overrides(&self) -> HashMap<String, String> {
        ALL_ACTIONS
            .iter()
            .filter_map(|action| {
                let key = self.bindings[action];
                if key == action.default_key() {
                    None
                } else {
                    key_to_name(key).map(|name| (action.name().to_string(), name))
                }
            })
            .collect()
    }

    pub fn get(&self, action: KeyAction) -> KeyCode {
        self.bindings[&action]
    }

    pub fn set(&mut self, action: KeyAction, key: KeyCode) {
        self.bindings.insert(action, key);
    }

    pub fn reset(&mut self, action: KeyAction) {
        self.bindings.insert(action, action.default_key());
    }

    pub fn reset_all(&mut self) {
        *self = Self::default();
    }

    /// Whichever action (if any) is already bound to `key`; the remap
    /// menu shows this as a warning rather than silently creating a
    /// second action that answers to the same key.
    pub fn action_bound_to(&self, key: KeyCode) -> Option<KeyAction> {
        ALL_ACTIONS
            .iter()
            .copied()
            .find(|a| self.bindings[a] == key)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_keymap_matches_every_action_to_its_builtin_default() {
        let map = Keymap::default();
        for action in ALL_ACTIONS {
            assert_eq!(map.get(*action), action.default_key());
        }
    }

    #[test]
    fn overrides_round_trip_through_name_strings() {
        let mut map = Keymap::default();
        map.set(KeyAction::SkipNext, KeyCode::F(5));
        map.set(KeyAction::ToggleShuffle, KeyCode::Char(' '));

        let overrides = map.overrides();
        assert_eq!(overrides.len(), 2);

        let restored = Keymap::from_overrides(&overrides);
        assert_eq!(restored.get(KeyAction::SkipNext), KeyCode::F(5));
        assert_eq!(restored.get(KeyAction::ToggleShuffle), KeyCode::Char(' '));
        // everything untouched is still default.
        assert_eq!(
            restored.get(KeyAction::CycleLoop),
            KeyAction::CycleLoop.default_key()
        );
    }

    #[test]
    fn unrecognized_override_entries_are_ignored_not_fatal() {
        let mut overrides = HashMap::new();
        overrides.insert("not_a_real_action".to_string(), "x".to_string());
        overrides.insert(
            KeyAction::Quit.name().to_string(),
            "not_a_real_key!!".to_string(),
        );
        let map = Keymap::from_overrides(&overrides);
        assert_eq!(map.get(KeyAction::Quit), KeyAction::Quit.default_key());
    }

    #[test]
    fn reset_all_restores_every_default_after_changes() {
        let mut map = Keymap::default();
        map.set(KeyAction::Quit, KeyCode::F(1));
        map.reset_all();
        assert_eq!(map.get(KeyAction::Quit), KeyAction::Quit.default_key());
    }

    #[test]
    fn action_bound_to_finds_the_owner_of_a_key() {
        let map = Keymap::default();
        assert_eq!(
            map.action_bound_to(KeyCode::Char('q')),
            Some(KeyAction::Quit)
        );
        assert_eq!(map.action_bound_to(KeyCode::F(9)), None);
    }

    #[test]
    fn no_default_binding_sits_on_a_reserved_key() {
        // a default that collided with a fixed convention would be
        // dead on arrival, and would also make that key unbindable
        // for everyone via from_overrides.
        for action in ALL_ACTIONS {
            let key = action.default_key();
            assert!(
                reserved_key_purpose(key).is_none(),
                "{} defaults to {}, which is reserved for {:?}",
                action.name(),
                key_label(key),
                reserved_key_purpose(key)
            );
        }
    }

    #[test]
    fn an_override_onto_a_reserved_key_is_refused() {
        let mut overrides = HashMap::new();
        // tab is move mode; an action bound to it would never fire.
        overrides.insert(KeyAction::Quit.name().to_string(), "tab".to_string());
        let map = Keymap::from_overrides(&overrides);
        assert_eq!(
            map.get(KeyAction::Quit),
            KeyAction::Quit.default_key(),
            "a shadowed binding should fall back to the default"
        );
    }

    #[test]
    fn an_override_onto_a_free_key_still_applies() {
        let mut overrides = HashMap::new();
        overrides.insert(KeyAction::Quit.name().to_string(), "x".to_string());
        let map = Keymap::from_overrides(&overrides);
        assert_eq!(map.get(KeyAction::Quit), KeyCode::Char('x'));
    }
}
