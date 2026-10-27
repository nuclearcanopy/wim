//! Pure application state + key-handling, decoupled from rendering so it
//! can be unit-tested without a real terminal (raw mode needs a tty we
//! don't have in this headless build/test context).

use crossterm::event::KeyCode;

use crate::eq;
use crate::keymap::{KeyAction, Keymap};
use crate::search::{Corpus, SearchItem};

const MIN_SPEED: f64 = 0.25;
const MAX_SPEED: f64 = 3.0;
const SPEED_STEP: f64 = 0.1;

/// `#[must_use]`: an action dropped on the floor is a keypress that
/// silently does nothing, which is exactly how enter-while-typing
/// stayed broken (see `dispatch_action` in main.rs); make it a
/// compile error instead of a bug report.
#[must_use]
pub enum Action {
    None,
    Quit,
    Play(SearchItem),
    Queued(SearchItem),
    /// Shuffle was toggled to the carried state. The queue itself lives
    /// in `playqueue::PlayQueue` (mpv's playlist, resolved by index),
    /// so turning shuffle on means the caller reshuffles *that* and
    /// resyncs mpv; `App` only owns the flag.
    ShuffleToggled(bool),
    SetLoop(LoopMode),
    SetSpeed(f64),
    SetEq(&'static str, [f64; 10]),
}

/// `One` loops just the current track forever (maps to mpv's own
/// `loop-file`); `All` replays the whole queue/album from the top once
/// it empties, a separate concept main.rs tracks itself (mpv has no
/// built-in notion of "the queue we built", only its own playlist).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoopMode {
    Off,
    One,
    All,
}

impl LoopMode {
    pub fn next(self) -> Self {
        match self {
            LoopMode::Off => LoopMode::One,
            LoopMode::One => LoopMode::All,
            LoopMode::All => LoopMode::Off,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            LoopMode::Off => "off",
            LoopMode::One => "one",
            LoopMode::All => "all",
        }
    }
}

pub struct App {
    pub corpus: Corpus,
    pub query: String,
    pub results: Vec<SearchItem>,
    pub selected: usize,
    pub editing_query: bool,
    pub loop_mode: LoopMode,
    pub shuffle: bool,
    pub speed: f64,
    /// index into `eq::PRESETS`; meaningful only while `eq_custom` is
    /// false (see `eq_custom`'s own doc).
    pub eq_preset: usize,
    /// True once any individual band has been hand-adjusted (via the
    /// eq menu) away from whatever named preset was active; the
    /// bottom bar/player view show "custom" instead of a preset name
    /// while this is set, rather than keeping credit on a preset that
    /// no longer matches `eq_gains`.
    pub eq_custom: bool,
    /// The actual gains currently applied to mpv's `af` filter: either
    /// a copy of `eq::PRESETS[eq_preset].gains`, or a hand-tuned set
    /// once `eq_custom` is true. Always kept in sync with what's
    /// actually audible, so callers never have to re-derive it.
    pub eq_gains: [f64; 10],
    /// Whether the corpus is still being filled from the background
    /// library scan / the first navidrome sync; purely so an empty
    /// results list can say "loading" instead of "nothing found" (see
    /// `main::loading_message`). Both start false: nothing is pending
    /// until main.rs has actually spawned the work.
    pub scanning_library: bool,
    pub syncing_servers: bool,
    /// vim-style pending leader key, currently only used for `gg`.
    pending: Option<char>,
}

impl App {
    pub fn new(corpus: Corpus) -> Self {
        App {
            corpus,
            query: String::new(),
            results: Vec::new(),
            selected: 0,
            editing_query: false,
            loop_mode: LoopMode::Off,
            shuffle: false,
            speed: 1.0,
            eq_preset: 0,
            eq_custom: false,
            eq_gains: eq::PRESETS[0].gains,
            scanning_library: false,
            syncing_servers: false,
            pending: None,
        }
    }

    /// Cycles loop mode (off -> one -> all -> off); shared by the
    /// `CycleLoop` keymap path and `Focus::BottomBar`'s enter-to-tweak
    /// on the Loop field, so both apply the exact same mutation.
    pub fn cycle_loop(&mut self) -> LoopMode {
        self.loop_mode = self.loop_mode.next();
        self.loop_mode
    }

    /// Toggles shuffle and reports the new state; shared by the
    /// `ToggleShuffle` keymap path and `Focus::BottomBar`'s
    /// enter-to-tweak, same as `cycle_loop`. Reshuffling the actual
    /// queue is the caller's job (see `Action::ShuffleToggled`).
    pub fn toggle_shuffle(&mut self) -> bool {
        self.shuffle = !self.shuffle;
        self.shuffle
    }

    /// One speed step, `dir` positive (`SpeedUp`) or negative
    /// (`SpeedDown`); shared the same way `cycle_loop` is.
    pub fn bump_speed(&mut self, dir: f64) -> f64 {
        self.speed = (self.speed + dir * SPEED_STEP).clamp(MIN_SPEED, MAX_SPEED);
        self.speed
    }

    pub fn handle_key(&mut self, key: KeyCode, keymap: &Keymap) -> Action {
        if self.editing_query {
            return self.handle_key_editing(key);
        }
        self.handle_key_normal(key, keymap)
    }

    fn handle_key_editing(&mut self, key: KeyCode) -> Action {
        match key {
            KeyCode::Char(c) => {
                self.query.push(c);
                self.refresh_results();
            }
            KeyCode::Backspace => {
                self.query.pop();
                self.refresh_results();
            }
            KeyCode::Esc => {
                self.editing_query = false;
            }
            // esc cancels (stop typing, do nothing); enter commits
            // (stop typing AND act on whatever's selected), same as
            // every other search box anywhere. Treating them the
            // same here meant the first enter right after typing
            // silently did nothing but close the text box, and only
            // a *second* enter actually played/opened anything,
            // which just read as "enter doesn't work."
            KeyCode::Enter => {
                self.editing_query = false;
                if let Some(item) = self.results.get(self.selected) {
                    return Action::Play(item.clone());
                }
            }
            _ => {}
        }
        Action::None
    }

    fn handle_key_normal(&mut self, key: KeyCode, keymap: &Keymap) -> Action {
        // any key other than a second 'g' cancels a pending `gg`.
        if self.pending == Some('g') && key != KeyCode::Char('g') {
            self.pending = None;
        }

        // universal navigation primitives, deliberately not part of
        // the remappable keymap (see keymap.rs's own module doc).
        match key {
            KeyCode::Char('j') | KeyCode::Down => {
                self.move_selection(1);
                return Action::None;
            }
            KeyCode::Char('k') | KeyCode::Up => {
                self.move_selection(-1);
                return Action::None;
            }
            KeyCode::Char('g') => {
                if self.pending == Some('g') {
                    self.selected = 0;
                    self.pending = None;
                } else {
                    self.pending = Some('g');
                }
                return Action::None;
            }
            KeyCode::Char('G') => {
                self.selected = self.results.len().saturating_sub(1);
                return Action::None;
            }
            KeyCode::Enter => {
                if let Some(item) = self.results.get(self.selected) {
                    return Action::Play(item.clone());
                }
                return Action::None;
            }
            _ => {}
        }

        if key == keymap.get(KeyAction::StartSearch) {
            self.editing_query = true;
        } else if key == keymap.get(KeyAction::Enqueue) {
            if let Some(item) = self.results.get(self.selected) {
                return Action::Queued(item.clone());
            }
        } else if key == keymap.get(KeyAction::CycleLoop) {
            return Action::SetLoop(self.cycle_loop());
        } else if key == keymap.get(KeyAction::ToggleShuffle) {
            return Action::ShuffleToggled(self.toggle_shuffle());
        } else if key == keymap.get(KeyAction::SpeedUp) {
            return Action::SetSpeed(self.bump_speed(1.0));
        } else if key == keymap.get(KeyAction::SpeedDown) {
            return Action::SetSpeed(self.bump_speed(-1.0));
        } else if key == keymap.get(KeyAction::CycleEq) {
            self.eq_preset = (self.eq_preset + 1) % eq::PRESETS.len();
            self.eq_custom = false;
            let preset = &eq::PRESETS[self.eq_preset];
            self.eq_gains = preset.gains;
            return Action::SetEq(preset.name, preset.gains);
        } else if key == keymap.get(KeyAction::Quit) {
            // not Esc: it's overloaded everywhere as "back/close" for
            // every modal/menu (help, subtitle/settings menus, nav
            // manager, onboarding, browse view, lyrics), so it has to
            // mean that consistently rather than quitting the whole
            // app the one time nothing else is open to close.
            return Action::Quit;
        }
        Action::None
    }

    fn move_selection(&mut self, delta: isize) {
        if self.results.is_empty() {
            self.selected = 0;
            return;
        }
        let len = self.results.len() as isize;
        let new = (self.selected as isize + delta).clamp(0, len - 1);
        self.selected = new as usize;
    }

    /// Re-filters for a query the user just changed, which genuinely
    /// should jump back to the best match; anything that re-filters
    /// *under* an unchanged query goes through `resync_results`
    /// instead, which keeps the selection where it was.
    fn refresh_results(&mut self) {
        self.results = self.corpus.search(&self.query);
        self.selected = 0;
    }

    /// Bracketed paste delivers the whole pasted string as one event,
    /// not synthesized keystrokes; this is the paste-equivalent of
    /// `handle_key_editing`'s char-push path, including the same
    /// results refresh. No-ops if the query isn't actually being
    /// edited (paste while browsing results does nothing, same as
    /// typing a bare character would).
    pub fn paste_into_query(&mut self, text: &str) {
        if self.editing_query {
            self.query.push_str(text);
            self.refresh_results();
        }
    }

    /// Re-filters `results` against the current query; call this
    /// after anything that mutates `corpus` out from under an already-
    /// typed query (e.g. navidrome connecting mid-session), or the
    /// displayed results silently go stale: they'd keep showing
    /// whatever matched the *old*, smaller corpus until the user
    /// happens to type or backspace a character, which they may never
    /// do if they already have the query they want.
    ///
    /// The query itself hasn't changed here, so the selection must
    /// survive: this runs off the 45s background sync and off every
    /// debounced remote-search answer, and zeroing it (what
    /// `refresh_results` does, correctly, for a *typed* query) meant
    /// scrolling to row 7 and then having a sync land silently snapped
    /// you back to row 0, with enter playing the wrong thing. The
    /// previously selected item is found again by identity; if it's
    /// genuinely gone from the new list, the index is clamped instead.
    pub fn resync_results(&mut self) {
        let previous = self.results.get(self.selected).map(|item| {
            let (tag, key) = item.identity();
            (tag, key.into_owned())
        });
        self.results = self.corpus.search(&self.query);
        let last = self.results.len().saturating_sub(1);
        self.selected = match previous {
            Some((tag, key)) => self
                .results
                .iter()
                .position(|item| {
                    let (t, k) = item.identity();
                    t == tag && k.as_ref() == key.as_str()
                })
                .unwrap_or(self.selected.min(last)),
            None => 0,
        };
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::library::{LocalTrack, MediaKind};
    use std::path::PathBuf;

    fn app_with_three_results() -> App {
        let mut corpus = Corpus::new();
        for i in 0..3 {
            corpus.add_local_tracks([LocalTrack {
                path: PathBuf::from(format!("/music/track{i}.flac")),
                title: format!("matchable track {i}"),
                artist: None,
                album: None,
                duration_secs: None,
                kind: MediaKind::Music,
            }]);
        }
        let keymap = Keymap::default();
        let mut app = App::new(corpus);
        app.editing_query = true;
        for c in "matchable".chars() {
            let _ = app.handle_key(KeyCode::Char(c), &keymap);
        }
        let _ = app.handle_key(KeyCode::Enter, &keymap); // exit editing mode
        app
    }

    /// Same corpus and query as `app_with_three_results`, but left
    /// mid-typing (`editing_query` still true), which is the state the
    /// enter/esc tests below are actually about.
    fn app_still_typing() -> App {
        let mut app = app_with_three_results();
        let keymap = Keymap::default();
        let _ = app.handle_key(KeyCode::Char('/'), &keymap);
        assert!(app.editing_query);
        app
    }

    #[test]
    fn enter_while_typing_plays_the_highlighted_result() {
        // the "i search bladee, press enter, nothing happens" bug: this
        // action exists, it just used to get discarded at the call site.
        let mut app = app_still_typing();
        let keymap = Keymap::default();
        match app.handle_key(KeyCode::Enter, &keymap) {
            Action::Play(SearchItem::LocalMusic { title, .. }) => {
                assert_eq!(title, "matchable track 0");
            }
            _ => panic!("expected Action::Play"),
        }
        assert!(!app.editing_query, "enter also commits the text box");
    }

    #[test]
    fn esc_while_typing_cancels_without_acting_on_the_selection() {
        let mut app = app_with_three_results();
        let keymap = Keymap::default();
        let _ = app.handle_key(KeyCode::Char('/'), &keymap);
        assert!(matches!(
            app.handle_key(KeyCode::Esc, &keymap),
            Action::None
        ));
        assert!(!app.editing_query);
    }

    #[test]
    fn typing_a_query_populates_and_resets_selection() {
        let app = app_with_three_results();
        assert_eq!(app.results.len(), 3);
        assert_eq!(app.selected, 0);
        assert!(!app.editing_query);
    }

    fn selected_title(app: &App) -> String {
        match app.results.get(app.selected) {
            Some(SearchItem::LocalMusic { title, .. }) => title.clone(),
            other => panic!("expected a local music result, got {other:?}"),
        }
    }

    #[test]
    fn resync_keeps_the_selection_on_the_same_item_even_if_the_list_reorders() {
        let mut app = app_with_three_results();
        let keymap = Keymap::default();
        let _ = app.handle_key(KeyCode::Char('j'), &keymap);
        let _ = app.handle_key(KeyCode::Char('j'), &keymap);
        assert_eq!(selected_title(&app), "matchable track 2");

        // what a background sync / remote-search answer does: new
        // items land in the corpus, and the exact-title one outranks
        // the fixture's rows, so the old index points elsewhere.
        app.corpus.add_local_tracks([LocalTrack {
            path: PathBuf::from("/music/exact.flac"),
            title: "matchable".to_string(),
            artist: None,
            album: None,
            duration_secs: None,
            kind: MediaKind::Music,
        }]);
        app.resync_results();

        assert_eq!(app.results.len(), 4);
        assert_eq!(
            selected_title(&app),
            "matchable track 2",
            "a sync landing mid-scroll must not move the selection to another item"
        );
    }

    #[test]
    fn resync_clamps_when_the_selected_item_is_gone() {
        let mut app = app_with_three_results();
        // a row that the corpus can no longer produce: resync has
        // nothing to restore onto, so it clamps instead of panicking
        // or silently jumping to the top.
        app.results.push(SearchItem::LocalMusic {
            path: PathBuf::from("/music/vanished.flac"),
            title: "matchable vanished".to_string(),
            artist: None,
            album: None,
        });
        app.selected = 3;
        app.resync_results();
        assert_eq!(app.selected, 2);
    }

    #[test]
    fn typing_still_resets_the_selection_to_the_top() {
        let mut app = app_with_three_results();
        let keymap = Keymap::default();
        let _ = app.handle_key(KeyCode::Char('G'), &keymap);
        assert_eq!(app.selected, 2);
        let _ = app.handle_key(KeyCode::Char('/'), &keymap);
        let _ = app.handle_key(KeyCode::Char(' '), &keymap);
        assert_eq!(
            app.selected, 0,
            "a genuinely new query should rank fresh and select the best match"
        );
    }

    #[test]
    fn paste_into_query_appends_and_refreshes_results() {
        // fixture already types "matchable" into the query, then exits
        // editing mode; re-enter it to test paste appending to the
        // existing text, same as a real "type some, then paste more".
        let mut app = app_with_three_results();
        let keymap = Keymap::default();
        // appends a word that's genuinely present in every fixture
        // title ("matchable track N"), so the assertion exercises
        // paste-triggers-a-real-refresh rather than leaning on the
        // typo fallback's edit-distance slack (which got intentionally
        // tightened; see search.rs's `edit_distance_budget`).
        let before = app.query.clone();
        let _ = app.handle_key(KeyCode::Char('/'), &keymap);
        app.paste_into_query(" track");
        assert_eq!(app.query, format!("{before} track"));
        assert_eq!(
            app.results.len(),
            3,
            "pasted text must refresh results same as typing"
        );
    }

    #[test]
    fn paste_into_query_is_a_noop_when_not_editing() {
        let mut app = app_with_three_results();
        let before = app.query.clone();
        assert!(!app.editing_query);
        app.paste_into_query("should not append");
        assert_eq!(
            app.query, before,
            "paste while not editing must not change the query"
        );
    }

    #[test]
    fn j_and_k_move_selection_and_clamp_at_bounds() {
        let mut app = app_with_three_results();
        let keymap = Keymap::default();
        let _ = app.handle_key(KeyCode::Char('j'), &keymap);
        assert_eq!(app.selected, 1);
        let _ = app.handle_key(KeyCode::Char('j'), &keymap);
        let _ = app.handle_key(KeyCode::Char('j'), &keymap);
        assert_eq!(
            app.selected, 2,
            "clamps at the last result, doesn't wrap/overflow"
        );
        let _ = app.handle_key(KeyCode::Char('k'), &keymap);
        assert_eq!(app.selected, 1);
    }

    #[test]
    fn gg_jumps_to_top_g_jumps_to_bottom() {
        let mut app = app_with_three_results();
        let keymap = Keymap::default();
        let _ = app.handle_key(KeyCode::Char('G'), &keymap);
        assert_eq!(app.selected, 2);
        let _ = app.handle_key(KeyCode::Char('g'), &keymap);
        let _ = app.handle_key(KeyCode::Char('g'), &keymap);
        assert_eq!(app.selected, 0);
    }

    #[test]
    fn single_g_then_other_key_does_not_jump() {
        let mut app = app_with_three_results();
        let keymap = Keymap::default();
        let _ = app.handle_key(KeyCode::Char('G'), &keymap); // selected = 2
        let _ = app.handle_key(KeyCode::Char('g'), &keymap);
        let _ = app.handle_key(KeyCode::Char('j'), &keymap); // cancels pending g, moves down instead
        assert_eq!(
            app.selected, 2,
            "already at bottom; j is clamped, not a stray top-jump"
        );
    }

    #[test]
    fn enter_on_a_result_yields_play_action() {
        let mut app = app_with_three_results();
        let keymap = Keymap::default();
        match app.handle_key(KeyCode::Enter, &keymap) {
            Action::Play(SearchItem::LocalMusic { title, .. }) => {
                assert_eq!(title, "matchable track 0");
            }
            _ => panic!("expected Action::Play"),
        }
    }

    #[test]
    fn q_quits() {
        let mut app = app_with_three_results();
        let keymap = Keymap::default();
        assert!(matches!(
            app.handle_key(KeyCode::Char('q'), &keymap),
            Action::Quit
        ));
    }

    #[test]
    fn a_enqueues_the_selected_item_without_playing_it() {
        let mut app = app_with_three_results();
        let keymap = Keymap::default();
        let _ = app.handle_key(KeyCode::Char('j'), &keymap); // select track 1
        match app.handle_key(KeyCode::Char('a'), &keymap) {
            Action::Queued(SearchItem::LocalMusic { title, .. }) => {
                assert_eq!(title, "matchable track 1");
            }
            _ => panic!("expected Action::Queued"),
        }
    }

    #[test]
    fn l_cycles_through_off_one_all_and_back_to_off() {
        let mut app = app_with_three_results();
        let keymap = Keymap::default();
        assert!(matches!(
            app.handle_key(KeyCode::Char('l'), &keymap),
            Action::SetLoop(LoopMode::One)
        ));
        assert_eq!(app.loop_mode, LoopMode::One);
        assert!(matches!(
            app.handle_key(KeyCode::Char('l'), &keymap),
            Action::SetLoop(LoopMode::All)
        ));
        assert_eq!(app.loop_mode, LoopMode::All);
        assert!(matches!(
            app.handle_key(KeyCode::Char('l'), &keymap),
            Action::SetLoop(LoopMode::Off)
        ));
        assert_eq!(app.loop_mode, LoopMode::Off);
    }

    #[test]
    fn speed_adjusts_and_clamps_at_bounds() {
        let mut app = app_with_three_results();
        let keymap = Keymap::default();
        assert!(
            matches!(app.handle_key(KeyCode::Char(']'), &keymap), Action::SetSpeed(s) if (s - 1.1).abs() < 1e-9)
        );
        for _ in 0..50 {
            let _ = app.handle_key(KeyCode::Char(']'), &keymap);
        }
        assert!((app.speed - MAX_SPEED).abs() < 1e-9, "clamped at max speed");
        for _ in 0..100 {
            let _ = app.handle_key(KeyCode::Char('['), &keymap);
        }
        assert!((app.speed - MIN_SPEED).abs() < 1e-9, "clamped at min speed");
    }

    #[test]
    fn s_toggles_shuffle_and_reports_the_new_state() {
        let mut app = app_with_three_results();
        let keymap = Keymap::default();
        assert!(matches!(
            app.handle_key(KeyCode::Char('s'), &keymap),
            Action::ShuffleToggled(true)
        ));
        assert!(app.shuffle);
        assert!(matches!(
            app.handle_key(KeyCode::Char('s'), &keymap),
            Action::ShuffleToggled(false)
        ));
        assert!(!app.shuffle);
    }

    #[test]
    fn e_cycles_eq_presets_and_wraps() {
        let mut app = app_with_three_results();
        let keymap = Keymap::default();
        assert_eq!(app.eq_preset, 0);
        for expected_idx in 1..eq::PRESETS.len() {
            match app.handle_key(KeyCode::Char('e'), &keymap) {
                Action::SetEq(name, gains) => {
                    assert_eq!(name, eq::PRESETS[expected_idx].name);
                    assert_eq!(gains, eq::PRESETS[expected_idx].gains);
                }
                _ => panic!("expected Action::SetEq"),
            }
            assert_eq!(app.eq_preset, expected_idx);
        }
        // one more press wraps back to the first preset (flat)
        let _ = app.handle_key(KeyCode::Char('e'), &keymap);
        assert_eq!(app.eq_preset, 0);
    }
}
