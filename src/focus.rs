//! Which pane the keyboard is aimed at, and the mode that moves it.
//!
//! Split out of main.rs as the first step of its decomposition (see
//! ROADMAP): this is self-contained, pure, and already had tests, so
//! it is the cheapest slice that makes the file shorter without
//! moving anything load-bearing.

use crossterm::event::KeyCode;

use crate::Focus;

/// Whether the keyboard is currently moving focus between panes, or
/// acting on whatever pane already has it. Two explicit modes rather
/// than one modifier-key shortcut: a chord that silently moves focus
/// gives you no way to tell, before pressing anything else, which
/// pane the next keypress is going to land in. In `Move` nothing
/// except focus responds at all (no scrolling, no playback, no
/// keybinds), and both the bottom bar's indicator and the focused
/// pane's own borders say so the whole time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputMode {
    Move,
    Interact,
}

/// wasd and the arrow keys both mean the same directional move;
/// normalizes either into the arrow key `focus_in_direction` matches
/// on, or `None` for a key that isn't a direction at all. wasd
/// because that's what sway is configured with for exactly this job
/// (focus the neighboring window), arrows because they're the obvious
/// guess if you've never read the keybind list. Only consulted in
/// `InputMode::Move`, so these stay free for their normal meaning
/// everywhere else.
pub fn focus_direction_key(code: KeyCode) -> Option<KeyCode> {
    match code {
        KeyCode::Left | KeyCode::Right | KeyCode::Up | KeyCode::Down => Some(code),
        // shift+w arrives as an uppercase char, but match both cases
        // rather than trusting every terminal to agree on that.
        KeyCode::Char(c) => match c.to_ascii_lowercase() {
            'a' => Some(KeyCode::Left),
            'd' => Some(KeyCode::Right),
            'w' => Some(KeyCode::Up),
            's' => Some(KeyCode::Down),
            _ => None,
        },
        _ => None,
    }
}

/// shift+<direction>'s sway/i3-style transition: which neighboring
/// pane (if any) lies in that direction from `current`. Every
/// direction/state combination not listed here has nowhere further
/// to go that way and is deliberately a no-op (returns `current`
/// unchanged), not an arbitrary fallback to some other pane.
pub fn focus_in_direction(current: Focus, direction: KeyCode) -> Focus {
    match (current, direction) {
        (Focus::Sidebar, KeyCode::Right) => Focus::Workspace,
        (Focus::Workspace, KeyCode::Left) => Focus::Sidebar,
        (Focus::Sidebar | Focus::Workspace, KeyCode::Down) => Focus::BottomBar,
        (Focus::BottomBar, KeyCode::Up) => Focus::Sidebar,
        _ => current,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn right_and_left_cross_between_the_sidebar_and_the_workspace() {
        assert_eq!(
            focus_in_direction(Focus::Sidebar, KeyCode::Right),
            Focus::Workspace
        );
        assert_eq!(
            focus_in_direction(Focus::Workspace, KeyCode::Left),
            Focus::Sidebar
        );
    }

    #[test]
    fn down_from_sidebar_or_workspace_reaches_the_bottom_bar() {
        assert_eq!(
            focus_in_direction(Focus::Sidebar, KeyCode::Down),
            Focus::BottomBar
        );
        assert_eq!(
            focus_in_direction(Focus::Workspace, KeyCode::Down),
            Focus::BottomBar
        );
    }

    /// Up from the bar goes to the sidebar rather than the workspace:
    /// one of the two has to be the answer, and the sidebar is where
    /// the keyboard starts.
    #[test]
    fn up_from_the_bottom_bar_returns_to_the_sidebar() {
        assert_eq!(
            focus_in_direction(Focus::BottomBar, KeyCode::Up),
            Focus::Sidebar
        );
    }

    /// A direction with nowhere to go is a no-op, not an arbitrary
    /// jump to some other pane.
    #[test]
    fn a_direction_with_no_neighbour_leaves_focus_alone() {
        for (from, dir) in [
            (Focus::Sidebar, KeyCode::Left),
            (Focus::Sidebar, KeyCode::Up),
            (Focus::Workspace, KeyCode::Right),
            (Focus::Workspace, KeyCode::Up),
            (Focus::BottomBar, KeyCode::Down),
            (Focus::BottomBar, KeyCode::Left),
            (Focus::BottomBar, KeyCode::Right),
        ] {
            assert_eq!(
                focus_in_direction(from, dir),
                from,
                "{from:?} + {dir:?} should not move"
            );
        }
    }

    #[test]
    fn wasd_and_the_arrows_mean_the_same_directions() {
        for (letter, arrow) in [
            ('a', KeyCode::Left),
            ('d', KeyCode::Right),
            ('w', KeyCode::Up),
            ('s', KeyCode::Down),
        ] {
            assert_eq!(focus_direction_key(KeyCode::Char(letter)), Some(arrow));
            assert_eq!(
                focus_direction_key(KeyCode::Char(letter.to_ascii_uppercase())),
                Some(arrow),
                "shift+{letter} arrives uppercase in some terminals"
            );
            assert_eq!(focus_direction_key(arrow), Some(arrow));
        }
    }

    #[test]
    fn anything_that_is_not_a_direction_is_not_one() {
        for code in [
            KeyCode::Char('x'),
            KeyCode::Char(' '),
            KeyCode::Enter,
            KeyCode::Esc,
            KeyCode::Tab,
        ] {
            assert_eq!(focus_direction_key(code), None);
        }
    }
}
