//! Where the last frame actually painted things, and what a mouse
//! event at a given cell therefore means.
//!
//! Every rect in this app is computed inside the draw closure, so the
//! input handler cannot know where anything is. Rather than recompute
//! the layout outside it (a second copy of the maths, free to drift),
//! the draw pass fills in a `HitMap` as it goes and the next mouse
//! event resolves against that. The map is therefore always a
//! description of the frame the user was looking at when they clicked,
//! which is the only frame their click was aimed at.
//!
//! `act` is pure: given a recorded map it decides everything, so the
//! event plumbing in main.rs only applies the answer. See
//! `docs/input-precedence.md` for how this stays in step with the
//! keyboard's own tier order.

use ratatui::layout::Rect;

use crate::ui::controlsbar::Field;
use crate::Focus;

/// Rows one wheel notch moves. Three, the usual terminal convention;
/// `j` moves one, and a wheel that also moved one would take a dozen
/// notches to cross a pane.
pub const WHEEL_ROWS: usize = 3;

/// The surfaces that own the keyboard outright while they are up, in
/// the order main.rs's key cascade checks them. The cascade is a chain
/// of early exits and cannot be turned into a table without rewriting
/// it, so this is the one other place that order is written down; the
/// comment above `editing_query` in main.rs says why it is
/// load-bearing, and `docs/input-precedence.md` records both readers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Modal {
    Onboarding,
    NavManager,
    Help,
    Console,
    EditingQuery,
    BrowseFilter,
    SubtitleMenu,
    SettingsMenu,
    EqMenu,
    KeybindsMenu,
}

/// Same order, as data, so `front` cannot answer in a different one.
pub const TIERS: [Modal; 10] = [
    Modal::Onboarding,
    Modal::NavManager,
    Modal::Help,
    Modal::Console,
    Modal::EditingQuery,
    Modal::BrowseFilter,
    Modal::SubtitleMenu,
    Modal::SettingsMenu,
    Modal::EqMenu,
    Modal::KeybindsMenu,
];

impl Modal {
    /// Whether this tier is painted over the panes behind it, which is
    /// what makes a click on one of them a click on something the user
    /// cannot see. The two that are not are text boxes drawn *inside* a
    /// visible pane: a keystroke has no location and so has to be
    /// claimed outright (that is the bug the cascade's ordering comment
    /// is about), but a click names its target, so clicking a pane
    /// while typing is an instruction rather than a leak.
    pub fn covers_screen(self) -> bool {
        match self {
            Modal::EditingQuery | Modal::BrowseFilter => false,
            Modal::Onboarding
            | Modal::NavManager
            | Modal::Help
            | Modal::Console
            | Modal::SubtitleMenu
            | Modal::SettingsMenu
            | Modal::EqMenu
            | Modal::KeybindsMenu => true,
        }
    }
}

/// Which tiers are currently up, as main.rs sees them. One struct
/// literal per frame, built next to the flags themselves, so the field
/// names are the only thing that has to match.
#[derive(Debug, Clone, Copy, Default)]
pub struct Tiers {
    pub onboarding: bool,
    pub nav_manager: bool,
    pub help: bool,
    pub console: bool,
    pub editing_query: bool,
    pub browse_filter: bool,
    pub subtitle_menu: bool,
    pub settings_menu: bool,
    pub eq_menu: bool,
    pub keybinds_menu: bool,
}

impl Tiers {
    fn up(self, modal: Modal) -> bool {
        match modal {
            Modal::Onboarding => self.onboarding,
            Modal::NavManager => self.nav_manager,
            Modal::Help => self.help,
            Modal::Console => self.console,
            Modal::EditingQuery => self.editing_query,
            Modal::BrowseFilter => self.browse_filter,
            Modal::SubtitleMenu => self.subtitle_menu,
            Modal::SettingsMenu => self.settings_menu,
            Modal::EqMenu => self.eq_menu,
            Modal::KeybindsMenu => self.keybinds_menu,
        }
    }
}

/// The frontmost tier that is up, by the keyboard's own order.
pub fn front(tiers: Tiers) -> Option<Modal> {
    TIERS.iter().copied().find(|m| tiers.up(*m))
}

/// Which of the two surfaces drawing `controlsbar::Field` a control
/// belongs to; they are on screen at the same time and carry separate
/// cursors, so a hit has to say which cursor it moved.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Transport {
    Bar,
    Player,
}

/// A surface the wheel can scroll and a click can pick a row in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scrollable {
    SidebarResults,
    BrowseAlbums,
    BrowseTracks,
    Queue,
    Suggestions,
    /// The one surface with no selection at all: `j`/`k` move a content
    /// offset there, so the wheel does too.
    Lyrics,
}

impl Scrollable {
    pub fn pane(self) -> Focus {
        match self {
            Scrollable::SidebarResults => Focus::Sidebar,
            Scrollable::BrowseAlbums
            | Scrollable::BrowseTracks
            | Scrollable::Queue
            | Scrollable::Suggestions
            | Scrollable::Lyrics => Focus::Workspace,
        }
    }
}

/// One control as the row actually drew it. A narrow bar drops whole
/// trailing elements (`controlsbar::segments_that_fit`), so a control
/// that did not fit simply has no entry here and cannot be clicked.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Control {
    pub transport: Transport,
    pub field: Field,
    pub rect: Rect,
}

/// A scrollable surface and its row geometry. `rows` holds the item
/// index per visible row of `rect`, `None` where that row is a section
/// header, a gap or past the end of the list; it is recorded from the
/// same layout call the render drew from, so a click lands on the row
/// the user saw. Empty `rows` means the surface scrolls content rather
/// than a selection, and `len` is then its line count.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct List {
    pub which: Scrollable,
    pub rect: Rect,
    pub rows: Vec<Option<usize>>,
    pub len: usize,
}

/// Filled in by the draw pass, read by the next mouse event.
#[derive(Debug, Clone, Default)]
pub struct HitMap {
    /// The tier that owned the keyboard for this frame, if any.
    pub modal: Option<Modal>,
    pub sidebar: Option<Rect>,
    pub workspace: Option<Rect>,
    pub bottom: Option<Rect>,
    pub lists: Vec<List>,
    pub controls: Vec<Control>,
    /// The scrubber's track cells, which is what an x offset is a
    /// fraction of; the time readouts either side are part of the
    /// element but are not positions on the track.
    pub track: Option<(Transport, Rect)>,
}

/// A mouse event reduced to the parts a decision needs. Raw-mode input
/// cannot be driven headlessly, so nothing below this line ever sees a
/// crossterm event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Click {
    pub kind: Kind,
    pub col: u16,
    pub row: u16,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// Left button down; the only press that does anything.
    Down,
    /// Left button held and moved.
    Drag,
    Up,
    WheelUp,
    WheelDown,
    /// Pointer motion, or a button this app has no use for. Named
    /// rather than dropped at the call site so the match below stays
    /// the only place that decides what is ignored.
    Other,
}

/// What a click means. Each variant is something main.rs already knows
/// how to do; nothing here performs an action.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Act {
    Nothing,
    /// A tier painted over the panes owns this cell; the click stops
    /// here rather than reaching whatever is underneath.
    Swallowed(Modal),
    Focus(Focus),
    /// Focus `which.pane()` and put its selection on this item.
    Select(Scrollable, usize),
    /// Do exactly what enter on this control does.
    Activate(Transport, Field),
    /// Open a drag at this fraction of the duration and seek there.
    ScrubOpen(Transport, f64),
    ScrubTo(f64),
    ScrubCommit(f64),
    /// One notch over the list under the pointer, whether or not it has
    /// focus.
    Wheel(Scrollable, bool),
}

/// Fraction of `track` the column `col` lands on. Cell `i` of an
/// `n`-cell track is `i / (n - 1)`, the inverse of what
/// `controlsbar::track` draws the head at; anything off either end
/// clamps to that end rather than wrapping or overshooting.
fn frac_at(track: Rect, col: u16) -> f64 {
    if track.width <= 1 {
        return 0.0;
    }
    let off = col.saturating_sub(track.x).min(track.width - 1);
    off as f64 / (track.width - 1) as f64
}

fn inside(rect: Rect, col: u16, row: u16) -> bool {
    col >= rect.x && col < rect.x + rect.width && row >= rect.y && row < rect.y + rect.height
}

/// The whole decision. `scrubbing` is whether a drag is already open,
/// which is the one thing a click's meaning depends on beyond where it
/// landed: a release has to reach the scrubber even when the pointer
/// wandered off it first.
pub fn act(map: &HitMap, click: Click, scrubbing: bool) -> Act {
    let Click { kind, col, row } = click;

    // an open drag keeps the pointer regardless of where it goes, the
    // same way the keyboard's scrub tier sits above the bar's own
    // selection keys: nothing may move the cursor out from under a
    // seek that has not been committed.
    if scrubbing {
        if let Some((_, track)) = map.track {
            match kind {
                Kind::Drag => return Act::ScrubTo(frac_at(track, col)),
                Kind::Up => return Act::ScrubCommit(frac_at(track, col)),
                _ => {}
            }
        }
    }

    if let Some(modal) = map.modal.filter(|m| m.covers_screen()) {
        return Act::Swallowed(modal);
    }

    // the wheel goes to the list under the pointer, not the focused
    // one, which is what every other scrollable thing does.
    if matches!(kind, Kind::WheelUp | Kind::WheelDown) {
        let down = kind == Kind::WheelDown;
        return match map.lists.iter().rev().find(|l| inside(l.rect, col, row)) {
            Some(list) => Act::Wheel(list.which, down),
            None => Act::Nothing,
        };
    }
    if kind != Kind::Down {
        return Act::Nothing;
    }

    // controls are drawn inside `bottom` (or inside the workspace, for
    // the maximized player), so they have to be tested before the pane
    // that contains them or the pane would answer for them.
    if let Some(control) = map.controls.iter().rev().find(|c| inside(c.rect, col, row)) {
        if control.field == Field::Progress {
            if let Some((transport, track)) = map.track {
                if transport == control.transport && inside(track, col, row) {
                    return Act::ScrubOpen(transport, frac_at(track, col));
                }
            }
        }
        return Act::Activate(control.transport, control.field);
    }

    if let Some(list) = map.lists.iter().rev().find(|l| inside(l.rect, col, row)) {
        let offset = (row - list.rect.y) as usize;
        if let Some(Some(item)) = list.rows.get(offset) {
            return Act::Select(list.which, *item);
        }
        // a header, a gap or empty space below the last row: still a
        // click on that pane, just not on any of its items.
        return Act::Focus(list.which.pane());
    }

    for (rect, pane) in [
        (map.bottom, Focus::BottomBar),
        (map.sidebar, Focus::Sidebar),
        (map.workspace, Focus::Workspace),
    ] {
        if rect.is_some_and(|r| inside(r, col, row)) {
            return Act::Focus(pane);
        }
    }
    Act::Nothing
}

/// One wheel notch on a list that scrolls by moving its selection,
/// clamped to the list. The selection, not an independent offset: a
/// notch is then the same operation as `j`, the clamp-to-selection rule
/// in `ui::scroll` keeps the selection on screen for free, and there is
/// no second piece of scroll state for the keyboard to disagree with.
pub fn wheel_select(selected: usize, len: usize, down: bool) -> usize {
    if len == 0 {
        return 0;
    }
    if down {
        (selected + WHEEL_ROWS).min(len - 1)
    } else {
        selected.saturating_sub(WHEEL_ROWS)
    }
}

/// One wheel notch on a surface that scrolls its content instead (the
/// lyrics pane has no selection to move), clamped the same way holding
/// `j` there is.
pub fn wheel_scroll(scroll: usize, len: usize, down: bool) -> usize {
    if down {
        crate::ui::scroll::clamp_to_content(scroll + WHEEL_ROWS, len)
    } else {
        scroll.saturating_sub(WHEEL_ROWS)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn down(col: u16, row: u16) -> Click {
        Click {
            kind: Kind::Down,
            col,
            row,
        }
    }

    /// Sidebar 0..20, a one-column divider, workspace 21..60, bottom
    /// bar on rows 18..20; roughly the real proportions.
    fn map() -> HitMap {
        HitMap {
            sidebar: Some(Rect::new(0, 0, 20, 18)),
            workspace: Some(Rect::new(21, 0, 39, 18)),
            bottom: Some(Rect::new(0, 18, 60, 2)),
            ..Default::default()
        }
    }

    fn bar(map: &mut HitMap, fields: &[(Field, u16, u16)]) {
        map.controls = fields
            .iter()
            .map(|&(field, x, width)| Control {
                transport: Transport::Bar,
                field,
                rect: Rect::new(x, 19, width, 1),
            })
            .collect();
    }

    #[test]
    fn a_click_in_a_pane_focuses_that_pane() {
        let m = map();
        assert_eq!(act(&m, down(5, 5), false), Act::Focus(Focus::Sidebar));
        assert_eq!(act(&m, down(30, 5), false), Act::Focus(Focus::Workspace));
        assert_eq!(act(&m, down(30, 19), false), Act::Focus(Focus::BottomBar));
    }

    #[test]
    fn a_click_on_nothing_resolves_to_nothing_not_to_the_nearest_thing() {
        let m = map();
        // the divider column between the two panes belongs to neither.
        assert_eq!(act(&m, down(20, 5), false), Act::Nothing);
        // and past the frame entirely.
        assert_eq!(act(&m, down(200, 5), false), Act::Nothing);
        assert_eq!(act(&m, down(5, 90), false), Act::Nothing);
    }

    #[test]
    fn a_click_on_a_bottom_bar_control_resolves_to_that_control() {
        let mut m = map();
        bar(
            &mut m,
            &[
                (Field::Pause, 4, 1),
                (Field::Loop, 7, 1),
                (Field::Shuffle, 10, 1),
                (Field::Speed, 13, 1),
                (Field::Eq, 16, 3),
                (Field::Crossfade, 21, 2),
            ],
        );
        for (field, x) in [
            (Field::Pause, 4),
            (Field::Loop, 7),
            (Field::Shuffle, 10),
            (Field::Speed, 13),
            (Field::Eq, 17),
            (Field::Crossfade, 22),
        ] {
            assert_eq!(
                act(&m, down(x, 19), false),
                Act::Activate(Transport::Bar, field),
                "{field:?} at column {x}"
            );
        }
        // the gaps between controls are still the bar, not a control.
        assert_eq!(act(&m, down(6, 19), false), Act::Focus(Focus::BottomBar));
    }

    #[test]
    fn a_control_a_narrow_bar_dropped_cannot_be_clicked() {
        let mut m = map();
        // the same row, laid out at a width that only fit the first
        // two: everything past them has no rect at all, so the cells
        // they would have occupied are plain bottom bar.
        bar(&mut m, &[(Field::Pause, 4, 1), (Field::Loop, 7, 1)]);
        assert_eq!(
            act(&m, down(4, 19), false),
            Act::Activate(Transport::Bar, Field::Pause)
        );
        for x in [10, 13, 16, 21] {
            assert_eq!(
                act(&m, down(x, 19), false),
                Act::Focus(Focus::BottomBar),
                "column {x} was a dropped control"
            );
        }
    }

    #[test]
    fn a_click_on_the_track_maps_to_a_fraction_of_the_duration() {
        let mut m = map();
        // an 11-cell track at columns 30..41, inside a progress
        // element that also carries the two time readouts.
        let track = Rect::new(30, 19, 11, 1);
        m.controls = vec![Control {
            transport: Transport::Bar,
            field: Field::Progress,
            rect: Rect::new(24, 19, 24, 1),
        }];
        m.track = Some((Transport::Bar, track));

        let frac = |col| match act(&m, down(col, 19), false) {
            Act::ScrubOpen(Transport::Bar, f) => f,
            other => panic!("column {col} gave {other:?}"),
        };
        assert_eq!(frac(30), 0.0);
        assert_eq!(frac(35), 0.5);
        assert_eq!(frac(40), 1.0);

        // one cell past either end of the track is still inside the
        // element, which is "open the drag where it already is"
        // (exactly what enter does) rather than a seek to an end the
        // user did not click on.
        assert_eq!(
            act(&m, down(29, 19), false),
            Act::Activate(Transport::Bar, Field::Progress)
        );
        assert_eq!(
            act(&m, down(41, 19), false),
            Act::Activate(Transport::Bar, Field::Progress)
        );
        // past the element too, and it is just the bar again.
        assert_eq!(act(&m, down(50, 19), false), Act::Focus(Focus::BottomBar));
    }

    #[test]
    fn a_drag_clamps_to_the_track_wherever_the_pointer_wandered() {
        let mut m = map();
        m.track = Some((Transport::Bar, Rect::new(30, 19, 11, 1)));
        let at = |kind, col| act(&m, Click { kind, col, row: 3 }, true);
        assert_eq!(at(Kind::Drag, 35), Act::ScrubTo(0.5));
        // dragged off the left end, and off the right, and onto a row
        // the track is not even on: an open drag owns the pointer.
        assert_eq!(at(Kind::Drag, 0), Act::ScrubTo(0.0));
        assert_eq!(at(Kind::Drag, 250), Act::ScrubTo(1.0));
        assert_eq!(at(Kind::Up, 35), Act::ScrubCommit(0.5));
        // and with no drag open those same events do nothing.
        assert_eq!(
            act(
                &m,
                Click {
                    kind: Kind::Drag,
                    col: 35,
                    row: 3
                },
                false
            ),
            Act::Nothing
        );
    }

    #[test]
    fn an_overlay_swallows_a_click_that_would_land_on_a_pane_behind_it() {
        let mut m = map();
        bar(&mut m, &[(Field::Pause, 4, 1)]);
        for modal in TIERS.iter().copied().filter(|m| m.covers_screen()) {
            m.modal = Some(modal);
            // the sidebar, the workspace and a live bottom-bar control
            // the menu does not even paint over: all swallowed.
            assert_eq!(act(&m, down(5, 5), false), Act::Swallowed(modal));
            assert_eq!(act(&m, down(30, 5), false), Act::Swallowed(modal));
            assert_eq!(act(&m, down(4, 19), false), Act::Swallowed(modal));
            assert_eq!(
                act(
                    &m,
                    Click {
                        kind: Kind::WheelDown,
                        col: 5,
                        row: 5
                    },
                    false
                ),
                Act::Swallowed(modal)
            );
        }
    }

    #[test]
    fn a_text_box_does_not_swallow_a_click_aimed_somewhere_else() {
        let mut m = map();
        for modal in [Modal::EditingQuery, Modal::BrowseFilter] {
            m.modal = Some(modal);
            assert_eq!(act(&m, down(30, 5), false), Act::Focus(Focus::Workspace));
        }
    }

    #[test]
    fn the_cascades_order_decides_which_tier_answers() {
        let all = Tiers {
            onboarding: true,
            nav_manager: true,
            help: true,
            console: true,
            editing_query: true,
            browse_filter: true,
            subtitle_menu: true,
            settings_menu: true,
            eq_menu: true,
            keybinds_menu: true,
        };
        assert_eq!(front(all), Some(Modal::Onboarding));
        assert_eq!(
            front(Tiers {
                onboarding: false,
                nav_manager: false,
                ..all
            }),
            Some(Modal::Help)
        );
        assert_eq!(front(Tiers::default()), None);
        assert_eq!(
            front(Tiers {
                eq_menu: true,
                ..Default::default()
            }),
            Some(Modal::EqMenu)
        );
        // every tier is in the order exactly once, so none can be up
        // without `front` ever naming it.
        for modal in TIERS {
            assert_eq!(
                TIERS.iter().filter(|m| **m == modal).count(),
                1,
                "{modal:?} is listed more than once"
            );
        }
    }

    #[test]
    fn a_click_on_a_row_selects_that_row_and_focuses_its_pane() {
        let mut m = map();
        m.lists = vec![List {
            which: Scrollable::SidebarResults,
            rect: Rect::new(0, 4, 20, 5),
            // a section header, three results, then past the end.
            rows: vec![None, Some(7), Some(8), Some(9), None],
            len: 10,
        }];
        assert_eq!(
            act(&m, down(5, 5), false),
            Act::Select(Scrollable::SidebarResults, 7)
        );
        assert_eq!(
            act(&m, down(5, 7), false),
            Act::Select(Scrollable::SidebarResults, 9)
        );
        // the header row and the blank below the last result are the
        // pane, not an item: no selection moves to a row that is not
        // one.
        assert_eq!(act(&m, down(5, 4), false), Act::Focus(Focus::Sidebar));
        assert_eq!(act(&m, down(5, 8), false), Act::Focus(Focus::Sidebar));
    }

    #[test]
    fn the_wheel_scrolls_the_list_under_the_pointer_not_the_focused_one() {
        let mut m = map();
        m.lists = vec![
            List {
                which: Scrollable::SidebarResults,
                rect: Rect::new(0, 4, 20, 5),
                rows: vec![Some(0); 5],
                len: 40,
            },
            List {
                which: Scrollable::Queue,
                rect: Rect::new(21, 2, 39, 10),
                rows: vec![Some(0); 10],
                len: 40,
            },
        ];
        let wheel = |col, row, kind| act(&m, Click { kind, col, row }, false);
        assert_eq!(
            wheel(5, 5, Kind::WheelDown),
            Act::Wheel(Scrollable::SidebarResults, true)
        );
        assert_eq!(
            wheel(30, 5, Kind::WheelUp),
            Act::Wheel(Scrollable::Queue, false)
        );
        // over a pane with no list on it, the wheel does nothing at
        // all rather than scrolling something elsewhere.
        assert_eq!(wheel(5, 19, Kind::WheelDown), Act::Nothing);
    }

    #[test]
    fn a_wheel_notch_stops_at_both_ends_of_a_list() {
        assert_eq!(wheel_select(0, 40, true), WHEEL_ROWS);
        assert_eq!(wheel_select(WHEEL_ROWS, 40, false), 0);
        // the ends hold rather than wrapping, unlike the transport
        // cursor's own left/right.
        assert_eq!(wheel_select(0, 40, false), 0);
        assert_eq!(wheel_select(39, 40, true), 39);
        assert_eq!(wheel_select(38, 40, true), 39);
        assert_eq!(wheel_select(1, 40, false), 0);
        // and an empty list has nowhere to go.
        assert_eq!(wheel_select(0, 0, true), 0);
        assert_eq!(wheel_select(5, 0, false), 0);
    }

    #[test]
    fn a_wheel_notch_on_content_stops_on_the_last_line() {
        assert_eq!(wheel_scroll(0, 40, true), WHEEL_ROWS);
        assert_eq!(wheel_scroll(39, 40, true), 39);
        assert_eq!(wheel_scroll(0, 40, false), 0);
        assert_eq!(wheel_scroll(2, 40, false), 0);
        assert_eq!(wheel_scroll(0, 0, true), 0);
    }

    #[test]
    fn pointer_motion_and_unused_buttons_do_nothing() {
        let m = map();
        for kind in [Kind::Other, Kind::Up] {
            assert_eq!(
                act(
                    &m,
                    Click {
                        kind,
                        col: 5,
                        row: 5
                    },
                    false
                ),
                Act::Nothing
            );
        }
    }

    #[test]
    fn the_maximized_players_controls_carry_their_own_surface() {
        let mut m = map();
        m.controls = vec![Control {
            transport: Transport::Player,
            field: Field::Shuffle,
            rect: Rect::new(30, 9, 12, 1),
        }];
        assert_eq!(
            act(&m, down(33, 9), false),
            Act::Activate(Transport::Player, Field::Shuffle)
        );
        // one row off it is the workspace the player is drawn in.
        assert_eq!(act(&m, down(33, 10), false), Act::Focus(Focus::Workspace));
    }
}
