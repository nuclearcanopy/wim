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

/// A text box drawn inside a visible pane, rather than over it; the
/// two tiers of `Modal` that do not swallow a click, named again here
/// because a click *in* one of them is how it gets the keyboard.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TextBox {
    /// The sidebar's search box.
    Query,
    /// The artist profile's song filter.
    BrowseFilter,
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
    /// Text boxes and the rect each was drawn in; clicking one puts
    /// the keyboard in it, the way clicking a field does anywhere
    /// else, instead of the surrounding pane taking the click and
    /// closing it.
    pub boxes: Vec<(TextBox, Rect)>,
    /// The cells the notification toasts own. They are painted last,
    /// over panes whose lists this map already recorded, and they own
    /// every cell of their own box; a click on one is therefore a click
    /// on something the user cannot see, the same as under a covering
    /// tier (`ui::notify::boxes`).
    pub toasts: Vec<Rect>,
    /// The scrubber's track cells, per surface that drew one; both are
    /// on screen while the player is maximized. This, not the whole
    /// progress element, is what an x offset is a fraction of: the time
    /// readouts either side are part of the element but are not
    /// positions on the track.
    pub tracks: Vec<(Transport, Rect)>,
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
    /// Focus this text box's pane and put the keyboard in the box.
    Edit(TextBox),
    /// Focus `which.pane()` and put its selection on this item.
    Select(Scrollable, usize),
    /// Do exactly what enter on this control does. The fraction is
    /// `Some` only for a press on the scrubber's track, and says where
    /// the drag opens, which is the one thing a pointer knows about a
    /// control and a key does not.
    Press(Transport, Field, Option<f64>),
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

/// The whole decision. `dragging` names the surface whose scrubber has
/// a drag open, which is the one thing a click's meaning depends on
/// beyond where it landed: a release has to reach that scrubber even
/// when the pointer wandered off it first.
pub fn act(map: &HitMap, click: Click, dragging: Option<Transport>) -> Act {
    let Click { kind, col, row } = click;

    // an open drag keeps the pointer regardless of where it goes, the
    // same way the keyboard's scrub tier sits above the bar's own
    // selection keys: nothing may move the cursor out from under a
    // seek that has not been committed.
    if let Some(surface) = dragging {
        if let Some((_, track)) = map.tracks.iter().find(|(t, _)| *t == surface) {
            match kind {
                Kind::Drag => return Act::ScrubTo(frac_at(*track, col)),
                Kind::Up => return Act::ScrubCommit(frac_at(*track, col)),
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

    // a toast sits over whatever was recorded underneath it, so it has
    // to answer before any of it. the wheel passed through above on
    // purpose: a notch acts on no particular row, and a toast that
    // appeared mid-scroll must not make the wheel go dead.
    if map.toasts.iter().any(|r| inside(*r, col, row)) {
        return Act::Nothing;
    }

    // controls are drawn inside `bottom` (or inside the workspace, for
    // the maximized player), so they have to be tested before the pane
    // that contains them or the pane would answer for them.
    if let Some(control) = map.controls.iter().rev().find(|c| inside(c.rect, col, row)) {
        if control.field == Field::Progress {
            let own = map
                .tracks
                .iter()
                .find(|(t, _)| *t == control.transport)
                .filter(|(_, track)| inside(*track, col, row));
            if let Some((transport, track)) = own {
                return Act::Press(*transport, Field::Progress, Some(frac_at(*track, col)));
            }
        }
        return Act::Press(control.transport, control.field, None);
    }

    if let Some((which, _)) = map.boxes.iter().find(|(_, r)| inside(*r, col, row)) {
        return Act::Edit(*which);
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
        assert_eq!(act(&m, down(5, 5), None), Act::Focus(Focus::Sidebar));
        assert_eq!(act(&m, down(30, 5), None), Act::Focus(Focus::Workspace));
        assert_eq!(act(&m, down(30, 19), None), Act::Focus(Focus::BottomBar));
    }

    #[test]
    fn a_click_on_nothing_resolves_to_nothing_not_to_the_nearest_thing() {
        let m = map();
        // the divider column between the two panes belongs to neither.
        assert_eq!(act(&m, down(20, 5), None), Act::Nothing);
        // and past the frame entirely.
        assert_eq!(act(&m, down(200, 5), None), Act::Nothing);
        assert_eq!(act(&m, down(5, 90), None), Act::Nothing);
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
                act(&m, down(x, 19), None),
                Act::Press(Transport::Bar, field, None),
                "{field:?} at column {x}"
            );
        }
        // the gaps between controls are still the bar, not a control.
        assert_eq!(act(&m, down(6, 19), None), Act::Focus(Focus::BottomBar));
    }

    #[test]
    fn a_control_a_narrow_bar_dropped_cannot_be_clicked() {
        let mut m = map();
        // the same row, laid out at a width that only fit the first
        // two: everything past them has no rect at all, so the cells
        // they would have occupied are plain bottom bar.
        bar(&mut m, &[(Field::Pause, 4, 1), (Field::Loop, 7, 1)]);
        assert_eq!(
            act(&m, down(4, 19), None),
            Act::Press(Transport::Bar, Field::Pause, None)
        );
        for x in [10, 13, 16, 21] {
            assert_eq!(
                act(&m, down(x, 19), None),
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
        m.tracks = vec![(Transport::Bar, track)];

        let frac = |col| match act(&m, down(col, 19), None) {
            Act::Press(Transport::Bar, Field::Progress, Some(f)) => f,
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
            act(&m, down(29, 19), None),
            Act::Press(Transport::Bar, Field::Progress, None)
        );
        assert_eq!(
            act(&m, down(41, 19), None),
            Act::Press(Transport::Bar, Field::Progress, None)
        );
        // past the element too, and it is just the bar again.
        assert_eq!(act(&m, down(50, 19), None), Act::Focus(Focus::BottomBar));
    }

    #[test]
    fn a_drag_clamps_to_the_track_wherever_the_pointer_wandered() {
        let mut m = map();
        m.tracks = vec![(Transport::Bar, Rect::new(30, 19, 11, 1))];
        let at = |kind, col| act(&m, Click { kind, col, row: 3 }, Some(Transport::Bar));
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
                None
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
            assert_eq!(act(&m, down(5, 5), None), Act::Swallowed(modal));
            assert_eq!(act(&m, down(30, 5), None), Act::Swallowed(modal));
            assert_eq!(act(&m, down(4, 19), None), Act::Swallowed(modal));
            assert_eq!(
                act(
                    &m,
                    Click {
                        kind: Kind::WheelDown,
                        col: 5,
                        row: 5
                    },
                    None
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
            assert_eq!(act(&m, down(30, 5), None), Act::Focus(Focus::Workspace));
        }
    }

    /// Clicking the box puts the keyboard in it. Without this a click
    /// on a visible search box would be a click on the pane around it,
    /// which takes the keyboard *out*: the exact opposite.
    #[test]
    fn a_click_on_a_text_box_claims_the_keyboard_for_it() {
        let mut m = map();
        m.boxes = vec![
            (TextBox::Query, Rect::new(0, 0, 20, 3)),
            (TextBox::BrowseFilter, Rect::new(35, 2, 25, 3)),
        ];
        m.lists = vec![List {
            which: Scrollable::SidebarResults,
            rect: Rect::new(0, 4, 20, 5),
            rows: vec![Some(0); 5],
            len: 5,
        }];
        assert_eq!(act(&m, down(5, 1), None), Act::Edit(TextBox::Query));
        assert_eq!(act(&m, down(40, 3), None), Act::Edit(TextBox::BrowseFilter));
        // one row below the box is the list again, not the box.
        assert_eq!(
            act(&m, down(5, 4), None),
            Act::Select(Scrollable::SidebarResults, 0)
        );
        // and an overlay still swallows a click on a box underneath.
        m.modal = Some(Modal::Help);
        assert_eq!(act(&m, down(5, 1), None), Act::Swallowed(Modal::Help));
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
            act(&m, down(5, 5), None),
            Act::Select(Scrollable::SidebarResults, 7)
        );
        assert_eq!(
            act(&m, down(5, 7), None),
            Act::Select(Scrollable::SidebarResults, 9)
        );
        // the header row and the blank below the last result are the
        // pane, not an item: no selection moves to a row that is not
        // one.
        assert_eq!(act(&m, down(5, 4), None), Act::Focus(Focus::Sidebar));
        assert_eq!(act(&m, down(5, 8), None), Act::Focus(Focus::Sidebar));
    }

    /// A toast owns every cell of its own box and is painted after the
    /// lists under it were recorded, so it has to answer for those
    /// cells; otherwise a click on a visible toast moves a selection
    /// onto a row the toast is hiding.
    #[test]
    fn a_toast_answers_for_the_cells_it_covers_instead_of_what_is_under_them() {
        let mut m = map();
        bar(&mut m, &[(Field::Pause, 4, 1)]);
        m.boxes = vec![(TextBox::Query, Rect::new(0, 0, 20, 3))];
        m.lists = vec![List {
            which: Scrollable::SidebarResults,
            rect: Rect::new(0, 4, 20, 5),
            rows: vec![Some(0), Some(1), Some(2), Some(3), Some(4)],
            len: 5,
        }];
        // one over the top of the results list, one over the search box
        // and one over a live bottom-bar control.
        m.toasts = vec![
            Rect::new(2, 4, 14, 3),
            Rect::new(0, 1, 10, 1),
            Rect::new(4, 19, 6, 1),
        ];
        for (col, row) in [(5, 4), (15, 6), (5, 1), (4, 19)] {
            assert_eq!(
                act(&m, down(col, row), None),
                Act::Nothing,
                "({col},{row}) is under a toast"
            );
        }
        // one row below the toast the list answers again, and one
        // column right of it.
        assert_eq!(
            act(&m, down(5, 7), None),
            Act::Select(Scrollable::SidebarResults, 3)
        );
        assert_eq!(
            act(&m, down(16, 4), None),
            Act::Select(Scrollable::SidebarResults, 0)
        );
        // the wheel still reaches the list under a toast: a notch acts
        // on no particular row, so one appearing mid-scroll must not
        // make the wheel go dead.
        assert_eq!(
            act(
                &m,
                Click {
                    kind: Kind::WheelDown,
                    col: 5,
                    row: 4
                },
                None
            ),
            Act::Wheel(Scrollable::SidebarResults, true)
        );
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
        let wheel = |col, row, kind| act(&m, Click { kind, col, row }, None);
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
                    None
                ),
                Act::Nothing
            );
        }
    }

    /// The index a user can read off a rendered row, parsed digit by
    /// digit rather than by `contains`: "track 3" is a prefix of
    /// "track 39", so a substring test would accept the wrong song.
    fn song_on_row(buf: &ratatui::buffer::Buffer, rect: Rect, row: u16) -> (Option<usize>, String) {
        let text: String = (rect.x..rect.x + rect.width)
            .map(|x| crate::ui::testbuf::char_at(buf, x, rect.y + row))
            .collect();
        let digits: String = text
            .split("track ")
            .nth(1)
            .unwrap_or("")
            .chars()
            .take_while(char::is_ascii_digit)
            .collect();
        (digits.parse().ok(), text)
    }

    fn songs(n: usize) -> Vec<crate::search::SearchItem> {
        (0..n)
            .map(|i| crate::search::SearchItem::LocalMusic {
                path: format!("/m/{i}.flac").into(),
                title: format!("track {i}"),
                artist: None,
                album: None,
            })
            .collect()
    }

    /// The whole resolution path for a real list, with no hand-written
    /// row map: the list is rendered, its map is recorded the way the
    /// draw pass records it, and every click is aimed at the cell a
    /// song's own text landed on. This is the maintainer's "click a
    /// song in the list" end to end, minus the terminal. A map built
    /// off a different scroll offset than the render used, or a lookup
    /// that is off by a row, answers with the neighbouring song here.
    #[test]
    fn clicking_the_cell_a_song_was_painted_on_selects_that_song() {
        use crate::favorites::Favorites;
        use crate::ui::results_list::{row_items, ResultsList};
        use ratatui::widgets::Widget;
        use std::collections::HashSet;
        use std::time::Duration;

        let items = songs(40);
        // a rect at a non-zero origin, with the scroll offset at zero,
        // mid-list, and pinned to the last song.
        for (height, selected) in [(5u16, 0usize), (12, 20), (8, 39)] {
            let rect = Rect::new(2, 3, 26, height);
            let mut buf = crate::ui::testbuf::buffer(rect.x + rect.width, rect.y + rect.height);
            ResultsList {
                items: &items,
                selected,
                empty_message: "",
                favorites: &Favorites::default(),
                remote_starred: &HashSet::new(),
                focused: true,
                elapsed: Duration::ZERO,
            }
            .render(rect, &mut buf);
            let m = HitMap {
                sidebar: Some(Rect::new(0, 0, 30, rect.y + rect.height)),
                lists: vec![List {
                    which: Scrollable::SidebarResults,
                    rect,
                    rows: row_items(&items, selected, rect),
                    len: items.len(),
                }],
                ..Default::default()
            };

            let mut songs_seen = 0;
            for row in 0..height {
                let (painted, text) = song_on_row(&buf, rect, row);
                let what = act(&m, down(rect.x + 1, rect.y + row), None);
                match painted {
                    Some(i) => {
                        songs_seen += 1;
                        assert_eq!(
                            what,
                            Act::Select(Scrollable::SidebarResults, i),
                            "row {row} of a {height}-row list reads {text:?}"
                        );
                    }
                    // a section header, or past the end of the list:
                    // the pane, but no song.
                    None => assert_eq!(
                        what,
                        Act::Focus(Focus::Sidebar),
                        "row {row} of a {height}-row list reads {text:?}"
                    ),
                }
            }
            assert!(songs_seen > 0, "nothing was painted, so nothing is proven");
        }
    }

    /// The other wiring shape the draw pass uses: the rect recorded is
    /// the body rect under a two-row header while the row map is
    /// derived from the whole pane, so row 0 of the map has to be the
    /// first row of that rect and not of the pane.
    #[test]
    fn clicking_the_cell_a_queue_entry_was_painted_on_selects_that_entry() {
        use crate::ui::queue::{body_rect, row_items, QueueView};
        use ratatui::widgets::Widget;
        use std::time::Duration;

        let entries = songs(40);
        for (height, selected) in [(9u16, 0usize), (14, 20), (10, 39)] {
            let area = Rect::new(21, 1, 44, height);
            let mut buf = crate::ui::testbuf::buffer(area.x + area.width, area.y + area.height);
            QueueView {
                entries: &entries,
                pos: Some(0),
                selected,
                focused: true,
                elapsed: Duration::ZERO,
            }
            .render(area, &mut buf);
            let body = body_rect(area);
            let m = HitMap {
                workspace: Some(area),
                lists: vec![List {
                    which: Scrollable::Queue,
                    rect: body,
                    rows: row_items(&entries, Some(0), selected, area),
                    len: entries.len(),
                }],
                ..Default::default()
            };

            let mut entries_seen = 0;
            for row in 0..body.height {
                let (painted, text) = song_on_row(&buf, body, row);
                let what = act(&m, down(body.x + 1, body.y + row), None);
                match painted {
                    Some(i) => {
                        entries_seen += 1;
                        assert_eq!(
                            what,
                            Act::Select(Scrollable::Queue, i),
                            "body row {row} of a {height}-row pane reads {text:?}"
                        );
                    }
                    None => assert_eq!(
                        what,
                        Act::Focus(Focus::Workspace),
                        "body row {row} of a {height}-row pane reads {text:?}"
                    ),
                }
            }
            assert!(
                entries_seen > 0,
                "nothing was painted, so nothing is proven"
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
            act(&m, down(33, 9), None),
            Act::Press(Transport::Player, Field::Shuffle, None)
        );
        // one row off it is the workspace the player is drawn in.
        assert_eq!(act(&m, down(33, 10), None), Act::Focus(Focus::Workspace));
    }
}
