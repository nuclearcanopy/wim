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
    OpenQueueView,
    OpenSuggestions,
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
    KeyAction::OpenQueueView,
    KeyAction::OpenSuggestions,
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
            KeyAction::OpenQueueView => "open_queue_view",
            KeyAction::OpenSuggestions => "open_suggestions",
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
            KeyAction::OpenQueueView => "the queue",
            KeyAction::OpenSuggestions => "suggestions",
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
            // 'Q' for queue; the shift-letter convention S/L/I/K
            // already uses, and esc closes it so a mis-shift to 'q'
            // is the only risk, same as every other one of those.
            KeyAction::OpenQueueView => KeyCode::Char('Q'),
            // 'R' for recommendations: 's'/'S' were already shuffle
            // and starred, and it keeps the shift-letter convention
            // S/L/I/K/Q already established for view-openers.
            KeyAction::OpenSuggestions => KeyCode::Char('R'),
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
        // chars, not bytes: a key rebound to a wide char ("日") wrote
        // a three-byte name that `len() == 1` then refused to read
        // back, so the menu accepted it and the next launch lost it.
        _ if name.chars().count() == 1 => name.chars().next().map(KeyCode::Char),
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

/// Every key the fixed conventions permanently own, and what for.
/// A table rather than a `match` arm so the help overlay can walk it:
/// these are the keys no remap can ever reach, which makes the help
/// pane the only place to find out what they do, and `:` sat here
/// naming a whole command console the reference never mentioned.
pub const RESERVED_KEYS: &[(KeyCode, &str)] = &[
    (KeyCode::Char('j'), "move selection"),
    (KeyCode::Char('k'), "move selection"),
    (KeyCode::Char('g'), "jump to top / bottom"),
    (KeyCode::Char('G'), "jump to top / bottom"),
    (KeyCode::Char('K'), "the remap menu itself"),
    (KeyCode::Char(':'), "the shellsong console"),
    (KeyCode::Enter, "play / open"),
    (KeyCode::Esc, "back / close"),
    (KeyCode::Tab, "move mode"),
    (KeyCode::Left, "navigation"),
    (KeyCode::Right, "navigation"),
    (KeyCode::Up, "navigation"),
    (KeyCode::Down, "navigation"),
    (KeyCode::Backspace, "editing a query"),
];

/// What a key is permanently used for, if it's one of the fixed
/// navigation conventions rather than a remappable action. These are
/// checked before every input handler that consults the keymap, so an
/// action bound to one of them would simply never fire; refusing the
/// binding and saying why beats a key that silently does nothing.
pub fn reserved_key_purpose(key: KeyCode) -> Option<&'static str> {
    RESERVED_KEYS
        .iter()
        .find(|(k, _)| *k == key)
        .map(|(_, purpose)| *purpose)
}

/// One `[keybinds]` warning, in the shape config.rs's own corrections
/// use: name the entry, say why it could not be honoured, then say
/// what is in force instead.
fn refused(action: KeyAction, reason: &str) -> String {
    format!(
        "keybinds.{}: {reason}; using \"{}\"",
        action.name(),
        key_label(action.default_key())
    )
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
    /// `from_overrides_reporting` with the warnings dropped, for the
    /// callers that have nowhere to show one (tests, and the
    /// round-trip through `overrides()`, which can only produce
    /// entries this module wrote itself).
    pub fn from_overrides(overrides: &HashMap<String, String>) -> Self {
        Self::from_overrides_reporting(overrides).0
    }

    /// Starts from every default, then applies whatever `overrides`
    /// (config.toml's `[keybinds]` table, action name -> key name)
    /// wim can actually honour, **and says which entries it could
    /// not**, in the same `(value, warnings)` shape
    /// `config::load_reporting` uses so main.rs can push them into the
    /// notification area.
    ///
    /// Four classes, all of them silent before: an action name nothing
    /// reads, a key name that does not round-trip, a key the fixed
    /// conventions own, and a key another action already holds. The
    /// last one was the worst: `quit = "a"` was *applied*, and since
    /// `app.rs` tests `Enqueue` before `Quit`, the first match won and
    /// `Quit` was simply dead.
    pub fn from_overrides_reporting(overrides: &HashMap<String, String>) -> (Self, Vec<String>) {
        let mut warnings = Vec::new();

        // a `HashMap` has no order, so the warnings are sorted by the
        // key nobody reads; the same config has to produce the same
        // lines in the same order every launch.
        let mut unknown: Vec<&str> = overrides
            .keys()
            .map(String::as_str)
            .filter(|name| !ALL_ACTIONS.iter().any(|a| a.name() == *name))
            .collect();
        unknown.sort_unstable();
        for name in unknown {
            warnings.push(format!("keybinds.{name} is not an action; ignored"));
        }

        // the entries that name a real action and parse to a key wim
        // can both store and reach, in `ALL_ACTIONS` order so a
        // collision between two of them resolves the same way twice.
        let mut wanted: Vec<(KeyAction, KeyCode)> = Vec::new();
        for action in ALL_ACTIONS {
            let Some(name) = overrides.get(action.name()) else {
                continue;
            };
            let Some(key) = name_to_key(name) else {
                warnings.push(refused(
                    *action,
                    &format!("\"{name}\" is not a key wim can bind"),
                ));
                continue;
            };
            if let Some(purpose) = reserved_key_purpose(key) {
                warnings.push(refused(
                    *action,
                    &format!("\"{}\" is reserved for {purpose}", key_label(key)),
                ));
                continue;
            }
            if let Some((owner, _)) = wanted.iter().find(|(_, k)| *k == key) {
                warnings.push(refused(
                    *action,
                    &format!("\"{}\" is already {}", key_label(key), owner.label()),
                ));
                continue;
            }
            wanted.push((*action, key));
        }

        // then the collisions against the actions still sitting on
        // their defaults. one at a time, because refusing an override
        // puts its own default back and that can collide in turn; the
        // loop ends because every pass either returns or shortens
        // `wanted`, and an empty `wanted` is `default()`, whose keys
        // are all distinct (pinned by a test).
        loop {
            let mut map = Self::default();
            for (action, key) in &wanted {
                map.bindings.insert(*action, *key);
            }
            // one side of any collision is always an override: no two
            // overrides share a key (deduped above) and no two
            // defaults do.
            let clash = wanted.iter().enumerate().find_map(|(i, (action, key))| {
                ALL_ACTIONS
                    .iter()
                    .find(|other| *other != action && map.bindings[other] == *key)
                    .map(|owner| (i, *owner))
            });
            let Some((i, owner)) = clash else {
                return (map, warnings);
            };
            let (action, key) = wanted.remove(i);
            warnings.push(refused(
                action,
                &format!("\"{}\" is already {}", key_label(key), owner.label()),
            ));
        }
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

    /// Why `key` cannot be bound to `action`, as the one line the
    /// remap menu puts up; `None` means take it.
    ///
    /// Exactly what `from_overrides_reporting` refuses, which is the
    /// point: the menu used to accept a reserved or already-taken key,
    /// write it to config.toml and show the row as rebound, and the
    /// next launch threw it away without a word. A key that does not
    /// round-trip through `key_to_name` is refused for the same
    /// reason: `overrides()` drops it on save.
    pub fn refusal(&self, action: KeyAction, key: KeyCode) -> Option<String> {
        if let Some(purpose) = reserved_key_purpose(key) {
            return Some(format!(
                "{} is reserved for {purpose}; not bound",
                key_label(key)
            ));
        }
        if key_to_name(key).is_none() {
            return Some("that key cannot be stored in config.toml; not bound".to_string());
        }
        match self.action_bound_to(key) {
            Some(owner) if owner != action => Some(format!(
                "{} is already {}; not bound",
                key_label(key),
                owner.label()
            )),
            _ => None,
        }
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
        // not space: that is `TogglePause`'s default, and two actions
        // on one key is now refused rather than applied and left for
        // `app.rs`'s check order to decide (see
        // `every_class_of_bad_keybind_entry_is_both_refused_and_reported`).
        map.set(KeyAction::ToggleShuffle, KeyCode::Char('z'));

        let overrides = map.overrides();
        assert_eq!(overrides.len(), 2);

        let restored = Keymap::from_overrides(&overrides);
        assert_eq!(restored.get(KeyAction::SkipNext), KeyCode::F(5));
        assert_eq!(restored.get(KeyAction::ToggleShuffle), KeyCode::Char('z'));
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

    /// `from_overrides_reporting`'s collision pass assumes one side of
    /// any clash is an override, which is only true while the defaults
    /// themselves are distinct.
    #[test]
    fn no_two_actions_share_a_default_key() {
        for (i, a) in ALL_ACTIONS.iter().enumerate() {
            for b in &ALL_ACTIONS[i + 1..] {
                assert_ne!(
                    a.default_key(),
                    b.default_key(),
                    "{} and {} both default to {}",
                    a.name(),
                    b.name(),
                    key_label(a.default_key())
                );
            }
        }
    }

    fn warnings_for(entries: &[(&str, &str)]) -> Vec<String> {
        let overrides: HashMap<String, String> = entries
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        Keymap::from_overrides_reporting(&overrides).1
    }

    /// The collision that shipped: `quit = "a"` was applied, and since
    /// `app.rs` checks `Enqueue` before `Quit`, the first match won and
    /// quit was dead with nothing said. Every other refusal was silent
    /// too.
    #[test]
    fn every_class_of_bad_keybind_entry_is_both_refused_and_reported() {
        let cases: [(&str, &str, &str); 4] = [
            ("quit", "a", "already enqueue selected"),
            ("quit", "j", "reserved for move selection"),
            ("quit", "not_a_key", "is not a key wim can bind"),
            ("quti", "x", "not an action"),
        ];
        for (name, value, expected) in cases {
            let (map, warnings) = {
                let mut overrides = HashMap::new();
                overrides.insert(name.to_string(), value.to_string());
                Keymap::from_overrides_reporting(&overrides)
            };
            assert_eq!(
                map.get(KeyAction::Quit),
                KeyAction::Quit.default_key(),
                "{name} = {value:?} should have fallen back to the default"
            );
            assert_eq!(warnings.len(), 1, "{name} = {value:?}: {warnings:?}");
            assert!(
                warnings[0].contains(expected),
                "{name} = {value:?} said {:?}, wanted {expected:?}",
                warnings[0]
            );
        }
    }

    /// Two entries asking for the same key is the one collision both
    /// sides of which are overrides; the earlier action in
    /// `ALL_ACTIONS` keeps it, deterministically.
    #[test]
    fn two_overrides_onto_one_key_keep_the_first_and_report_the_second() {
        let mut overrides = HashMap::new();
        overrides.insert(KeyAction::Enqueue.name().to_string(), "z".to_string());
        overrides.insert(KeyAction::Quit.name().to_string(), "z".to_string());
        let (map, warnings) = Keymap::from_overrides_reporting(&overrides);
        assert_eq!(map.get(KeyAction::Enqueue), KeyCode::Char('z'));
        assert_eq!(map.get(KeyAction::Quit), KeyAction::Quit.default_key());
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert!(warnings[0].contains("keybinds.quit"), "{warnings:?}");
    }

    /// Nothing may be refused for colliding with a default the very
    /// same table moved out of the way: swapping two keys is the
    /// obvious thing to write in a config and has to work.
    #[test]
    fn a_swap_between_two_actions_is_honoured_without_a_warning() {
        let mut overrides = HashMap::new();
        overrides.insert(KeyAction::Quit.name().to_string(), "l".to_string());
        overrides.insert(KeyAction::CycleLoop.name().to_string(), "q".to_string());
        let (map, warnings) = Keymap::from_overrides_reporting(&overrides);
        assert_eq!(map.get(KeyAction::Quit), KeyCode::Char('l'));
        assert_eq!(map.get(KeyAction::CycleLoop), KeyCode::Char('q'));
        assert!(warnings.is_empty(), "{warnings:?}");
    }

    /// Refusing an override hands its own default back, which can
    /// collide with an override that was already accepted; the pass
    /// has to keep going until the whole map is one key per action.
    #[test]
    fn a_refusal_that_frees_a_default_cannot_leave_a_second_collision() {
        let mut overrides = HashMap::new();
        // quit wants enqueue's default and loses; that puts "q" back,
        // which is what cycle_loop was given.
        overrides.insert(KeyAction::Quit.name().to_string(), "a".to_string());
        overrides.insert(KeyAction::CycleLoop.name().to_string(), "q".to_string());
        let (map, warnings) = Keymap::from_overrides_reporting(&overrides);
        let mut keys: Vec<String> = ALL_ACTIONS.iter().map(|a| key_label(map.get(*a))).collect();
        keys.sort();
        let before = keys.len();
        keys.dedup();
        assert_eq!(
            before,
            keys.len(),
            "two actions answer to one key: {keys:?}"
        );
        assert_eq!(warnings.len(), 2, "{warnings:?}");
    }

    #[test]
    fn a_clean_keybinds_table_says_nothing() {
        assert!(warnings_for(&[("quit", "x"), ("enqueue", "f5")]).is_empty());
    }

    /// `HashMap` iteration order is not stable, so two launches off the
    /// same config.toml must not shuffle the warnings.
    #[test]
    fn the_warnings_come_out_in_the_same_order_every_time() {
        let entries = [("zzz_bogus", "x"), ("aaa_bogus", "y"), ("quit", "j")];
        let first = warnings_for(&entries);
        for _ in 0..8 {
            assert_eq!(warnings_for(&entries), first);
        }
        assert!(first[0].contains("aaa_bogus"), "{first:?}");
        assert!(first[1].contains("zzz_bogus"), "{first:?}");
    }

    /// The interactive remap wrote these to config.toml and showed the
    /// row as rebound; `from_overrides` then dropped them at the next
    /// launch. The menu refuses on a `Some` instead.
    #[test]
    fn the_remap_refusal_covers_exactly_what_a_config_load_would_drop() {
        let map = Keymap::default();
        assert!(map
            .refusal(KeyAction::Quit, KeyCode::Char('j'))
            .is_some_and(|m| m.contains("reserved for move selection")));
        assert!(map
            .refusal(KeyAction::Quit, KeyCode::Char(':'))
            .is_some_and(|m| m.contains("the shellsong console")));
        assert!(map
            .refusal(KeyAction::Quit, KeyCode::Char('a'))
            .is_some_and(|m| m.contains("already enqueue selected")));
        // representable and free.
        assert_eq!(map.refusal(KeyAction::Quit, KeyCode::Char('x')), None);
        // rebinding an action to the key it already has is a no-op, not
        // a conflict with itself.
        assert_eq!(map.refusal(KeyAction::Quit, KeyCode::Char('q')), None);
        // `overrides()` cannot store this one, so accepting it would
        // last exactly one session.
        assert!(map
            .refusal(KeyAction::Quit, KeyCode::Home)
            .is_some_and(|m| m.contains("cannot be stored")));
    }

    /// Whatever the menu accepts has to survive a save/load round
    /// trip, which is the whole contract the two halves broke.
    #[test]
    fn anything_the_remap_accepts_survives_a_round_trip() {
        let candidates = [
            KeyCode::Char('x'),
            KeyCode::Char(' '),
            KeyCode::F(5),
            KeyCode::Char('日'),
            KeyCode::Char('j'),
            KeyCode::Char(':'),
            KeyCode::Char('a'),
            KeyCode::Home,
            KeyCode::Delete,
            KeyCode::Tab,
        ];
        for key in candidates {
            let mut map = Keymap::default();
            if map.refusal(KeyAction::Quit, key).is_some() {
                continue;
            }
            map.set(KeyAction::Quit, key);
            let (reloaded, warnings) = Keymap::from_overrides_reporting(&map.overrides());
            assert_eq!(
                reloaded.get(KeyAction::Quit),
                key,
                "the menu accepted {key:?} and the next launch lost it: {warnings:?}"
            );
            assert!(warnings.is_empty(), "{key:?}: {warnings:?}");
        }
    }

    /// The help pane, onboarding and the bottom bar all spell the
    /// console's key out of this entry; it has to be the key the
    /// console really opens on.
    #[test]
    fn the_console_is_reserved_under_the_key_it_actually_opens_on() {
        assert_eq!(
            reserved_key_purpose(crate::shellsong::OPEN_KEY),
            Some("the shellsong console")
        );
        assert_eq!(key_label(crate::shellsong::OPEN_KEY), ":");
    }

    #[test]
    fn every_reserved_key_names_a_purpose_and_no_duplicates() {
        for (key, purpose) in RESERVED_KEYS {
            assert_eq!(reserved_key_purpose(*key), Some(*purpose));
        }
        for (i, (key, _)) in RESERVED_KEYS.iter().enumerate() {
            assert!(
                !RESERVED_KEYS[i + 1..].iter().any(|(k, _)| k == key),
                "{} is listed twice",
                key_label(*key)
            );
        }
    }
}
