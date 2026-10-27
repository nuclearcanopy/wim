//! Pure application state + key-handling, decoupled from rendering so it
//! can be unit-tested without a real terminal (raw mode needs a tty we
//! don't have in this headless build/test context).

use crossterm::event::KeyCode;
use rand::Rng;

use crate::eq;
use crate::search::{Corpus, SearchItem};

const MIN_SPEED: f64 = 0.25;
const MAX_SPEED: f64 = 3.0;
const SPEED_STEP: f64 = 0.1;

pub enum Action {
    None,
    Quit,
    Play(SearchItem),
    Queued(SearchItem),
    /// Queue order changed (currently only from shuffle); caller
    /// should resync mpv's own playlist to match this new order,
    /// without disturbing whatever's currently playing.
    QueueReordered(Vec<SearchItem>),
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
    /// explicit play queue; deliberately *not* auto-populated from
    /// search results on enter (unlike album-browsing players): in a
    /// search-first ui the "next" search result usually isn't a related
    /// track, so implicitly bulk-queuing them would be surprising. `a`
    /// enqueues explicitly instead. See docs/features.md.
    pub queue: Vec<SearchItem>,
    pub loop_mode: LoopMode,
    pub shuffle: bool,
    pub speed: f64,
    pub eq_preset: usize,
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
            queue: Vec::new(),
            loop_mode: LoopMode::Off,
            shuffle: false,
            speed: 1.0,
            eq_preset: 0,
            pending: None,
        }
    }

    /// Pops and returns the next queued item, for the caller to actually
    /// hand to mpv on end-of-file (auto-advance). `App` doesn't know
    /// whether playback succeeds; that's main.rs's concern.
    pub fn pop_queue(&mut self) -> Option<SearchItem> {
        if self.queue.is_empty() {
            None
        } else {
            Some(self.queue.remove(0))
        }
    }

    /// Appends to the queue normally, or inserts at a random position
    /// while shuffle is on, so newly-added items don't just pile up in
    /// strict order after a shuffle.
    fn enqueue(&mut self, item: SearchItem) {
        if self.shuffle && !self.queue.is_empty() {
            let idx = rand::thread_rng().gen_range(0..=self.queue.len());
            self.queue.insert(idx, item);
        } else {
            self.queue.push(item);
        }
    }

    /// Toggles shuffle; turning it on reshuffles the current queue in
    /// place (Fisher-Yates) so it takes effect immediately rather than
    /// only affecting items added afterward.
    fn toggle_shuffle(&mut self) {
        self.shuffle = !self.shuffle;
        if self.shuffle {
            let mut rng = rand::thread_rng();
            for i in (1..self.queue.len()).rev() {
                let j = rng.gen_range(0..=i);
                self.queue.swap(i, j);
            }
        }
    }

    pub fn handle_key(&mut self, key: KeyCode) -> Action {
        if self.editing_query {
            return self.handle_key_editing(key);
        }
        self.handle_key_normal(key)
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
            KeyCode::Esc | KeyCode::Enter => {
                self.editing_query = false;
            }
            _ => {}
        }
        Action::None
    }

    fn handle_key_normal(&mut self, key: KeyCode) -> Action {
        // any key other than a second 'g' cancels a pending `gg`.
        if self.pending == Some('g') && key != KeyCode::Char('g') {
            self.pending = None;
        }

        match key {
            KeyCode::Char('/') => {
                self.editing_query = true;
            }
            KeyCode::Char('j') | KeyCode::Down => self.move_selection(1),
            KeyCode::Char('k') | KeyCode::Up => self.move_selection(-1),
            KeyCode::Char('g') => {
                if self.pending == Some('g') {
                    self.selected = 0;
                    self.pending = None;
                } else {
                    self.pending = Some('g');
                }
            }
            KeyCode::Char('G') => {
                self.selected = self.results.len().saturating_sub(1);
            }
            KeyCode::Enter => {
                if let Some(item) = self.results.get(self.selected) {
                    // playing something fresh replaces mpv's whole
                    // playlist, so our own queue view should match.
                    self.queue.clear();
                    return Action::Play(item.clone());
                }
            }
            KeyCode::Char('a') => {
                if let Some(item) = self.results.get(self.selected) {
                    let item = item.clone();
                    self.enqueue(item.clone());
                    return Action::Queued(item);
                }
            }
            KeyCode::Char('l') => {
                self.loop_mode = self.loop_mode.next();
                return Action::SetLoop(self.loop_mode);
            }
            KeyCode::Char('s') => {
                self.toggle_shuffle();
                return Action::QueueReordered(self.queue.clone());
            }
            KeyCode::Char(']') => {
                self.speed = (self.speed + SPEED_STEP).min(MAX_SPEED);
                return Action::SetSpeed(self.speed);
            }
            KeyCode::Char('[') => {
                self.speed = (self.speed - SPEED_STEP).max(MIN_SPEED);
                return Action::SetSpeed(self.speed);
            }
            KeyCode::Char('e') => {
                self.eq_preset = (self.eq_preset + 1) % eq::PRESETS.len();
                let preset = &eq::PRESETS[self.eq_preset];
                return Action::SetEq(preset.name, preset.gains);
            }
            KeyCode::Char('q') | KeyCode::Esc => return Action::Quit,
            _ => {}
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
    pub fn resync_results(&mut self) {
        self.refresh_results();
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
        let mut app = App::new(corpus);
        app.editing_query = true;
        for c in "matchable".chars() {
            app.handle_key(KeyCode::Char(c));
        }
        app.handle_key(KeyCode::Enter); // exit editing mode
        app
    }

    #[test]
    fn typing_a_query_populates_and_resets_selection() {
        let app = app_with_three_results();
        assert_eq!(app.results.len(), 3);
        assert_eq!(app.selected, 0);
        assert!(!app.editing_query);
    }

    #[test]
    fn paste_into_query_appends_and_refreshes_results() {
        // fixture already types "matchable" into the query, then exits
        // editing mode; re-enter it to test paste appending to the
        // existing text, same as a real "type some, then paste more".
        let mut app = app_with_three_results();
        let before = app.query.clone();
        app.handle_key(KeyCode::Char('/'));
        app.paste_into_query(" more");
        assert_eq!(app.query, format!("{before} more"));
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
        app.handle_key(KeyCode::Char('j'));
        assert_eq!(app.selected, 1);
        app.handle_key(KeyCode::Char('j'));
        app.handle_key(KeyCode::Char('j'));
        assert_eq!(
            app.selected, 2,
            "clamps at the last result, doesn't wrap/overflow"
        );
        app.handle_key(KeyCode::Char('k'));
        assert_eq!(app.selected, 1);
    }

    #[test]
    fn gg_jumps_to_top_g_jumps_to_bottom() {
        let mut app = app_with_three_results();
        app.handle_key(KeyCode::Char('G'));
        assert_eq!(app.selected, 2);
        app.handle_key(KeyCode::Char('g'));
        app.handle_key(KeyCode::Char('g'));
        assert_eq!(app.selected, 0);
    }

    #[test]
    fn single_g_then_other_key_does_not_jump() {
        let mut app = app_with_three_results();
        app.handle_key(KeyCode::Char('G')); // selected = 2
        app.handle_key(KeyCode::Char('g'));
        app.handle_key(KeyCode::Char('j')); // cancels pending g, moves down instead
        assert_eq!(
            app.selected, 2,
            "already at bottom; j is clamped, not a stray top-jump"
        );
    }

    #[test]
    fn enter_on_a_result_yields_play_action() {
        let mut app = app_with_three_results();
        match app.handle_key(KeyCode::Enter) {
            Action::Play(SearchItem::LocalMusic { title, .. }) => {
                assert_eq!(title, "matchable track 0");
            }
            _ => panic!("expected Action::Play"),
        }
    }

    #[test]
    fn enter_clears_the_queue_since_mpvs_whole_playlist_gets_replaced() {
        let mut app = app_with_three_results();
        app.handle_key(KeyCode::Char('a'));
        app.handle_key(KeyCode::Char('j'));
        app.handle_key(KeyCode::Char('a'));
        assert_eq!(app.queue.len(), 2);

        app.handle_key(KeyCode::Enter);
        assert!(
            app.queue.is_empty(),
            "a fresh enter-to-play replaces mpv's whole playlist, so our queue view must match"
        );
    }

    #[test]
    fn q_quits() {
        let mut app = app_with_three_results();
        assert!(matches!(app.handle_key(KeyCode::Char('q')), Action::Quit));
    }

    #[test]
    fn a_enqueues_the_selected_item_without_playing_it() {
        let mut app = app_with_three_results();
        app.handle_key(KeyCode::Char('j')); // select track 1
        match app.handle_key(KeyCode::Char('a')) {
            Action::Queued(SearchItem::LocalMusic { title, .. }) => {
                assert_eq!(title, "matchable track 1");
            }
            _ => panic!("expected Action::Queued"),
        }
        assert_eq!(app.queue.len(), 1);
    }

    #[test]
    fn pop_queue_is_fifo_and_empties_cleanly() {
        let mut app = app_with_three_results();
        app.handle_key(KeyCode::Char('a')); // queues track 0
        app.handle_key(KeyCode::Char('j'));
        app.handle_key(KeyCode::Char('a')); // queues track 1

        let first = app.pop_queue();
        assert!(
            matches!(first, Some(SearchItem::LocalMusic { ref title, .. }) if title == "matchable track 0")
        );
        let second = app.pop_queue();
        assert!(
            matches!(second, Some(SearchItem::LocalMusic { ref title, .. }) if title == "matchable track 1")
        );
        assert!(app.pop_queue().is_none());
    }

    #[test]
    fn l_cycles_through_off_one_all_and_back_to_off() {
        let mut app = app_with_three_results();
        assert!(matches!(
            app.handle_key(KeyCode::Char('l')),
            Action::SetLoop(LoopMode::One)
        ));
        assert_eq!(app.loop_mode, LoopMode::One);
        assert!(matches!(
            app.handle_key(KeyCode::Char('l')),
            Action::SetLoop(LoopMode::All)
        ));
        assert_eq!(app.loop_mode, LoopMode::All);
        assert!(matches!(
            app.handle_key(KeyCode::Char('l')),
            Action::SetLoop(LoopMode::Off)
        ));
        assert_eq!(app.loop_mode, LoopMode::Off);
    }

    #[test]
    fn speed_adjusts_and_clamps_at_bounds() {
        let mut app = app_with_three_results();
        assert!(
            matches!(app.handle_key(KeyCode::Char(']')), Action::SetSpeed(s) if (s - 1.1).abs() < 1e-9)
        );
        for _ in 0..50 {
            app.handle_key(KeyCode::Char(']'));
        }
        assert!((app.speed - MAX_SPEED).abs() < 1e-9, "clamped at max speed");
        for _ in 0..100 {
            app.handle_key(KeyCode::Char('['));
        }
        assert!((app.speed - MIN_SPEED).abs() < 1e-9, "clamped at min speed");
    }

    #[test]
    fn shuffle_preserves_every_queued_item() {
        let mut app = app_with_three_results();
        app.handle_key(KeyCode::Char('a'));
        app.handle_key(KeyCode::Char('j'));
        app.handle_key(KeyCode::Char('a'));
        app.handle_key(KeyCode::Char('j'));
        app.handle_key(KeyCode::Char('a'));
        assert_eq!(app.queue.len(), 3);

        app.handle_key(KeyCode::Char('s')); // shuffle on, reshuffles in place
        assert!(app.shuffle);
        assert_eq!(
            app.queue.len(),
            3,
            "shuffle must not drop or duplicate items"
        );

        let mut titles: Vec<String> = app
            .queue
            .iter()
            .map(|i| match i {
                SearchItem::LocalMusic { title, .. } => title.clone(),
                _ => unreachable!(),
            })
            .collect();
        titles.sort();
        assert_eq!(
            titles,
            vec![
                "matchable track 0",
                "matchable track 1",
                "matchable track 2"
            ]
        );
    }

    #[test]
    fn enqueue_while_shuffled_still_lands_in_the_queue() {
        let mut app = app_with_three_results();
        app.handle_key(KeyCode::Char('a')); // one item, shuffle has nothing to reorder yet
        app.handle_key(KeyCode::Char('s')); // shuffle on
        app.handle_key(KeyCode::Char('j'));
        app.handle_key(KeyCode::Char('a')); // enqueue track 1 while shuffled

        assert_eq!(app.queue.len(), 2);
        assert!(app.queue.iter().any(
            |i| matches!(i, SearchItem::LocalMusic { title, .. } if title == "matchable track 1")
        ));
    }

    #[test]
    fn e_cycles_eq_presets_and_wraps() {
        let mut app = app_with_three_results();
        assert_eq!(app.eq_preset, 0);
        for expected_idx in 1..eq::PRESETS.len() {
            match app.handle_key(KeyCode::Char('e')) {
                Action::SetEq(name, gains) => {
                    assert_eq!(name, eq::PRESETS[expected_idx].name);
                    assert_eq!(gains, eq::PRESETS[expected_idx].gains);
                }
                _ => panic!("expected Action::SetEq"),
            }
            assert_eq!(app.eq_preset, expected_idx);
        }
        // one more press wraps back to the first preset (flat)
        app.handle_key(KeyCode::Char('e'));
        assert_eq!(app.eq_preset, 0);
    }
}
