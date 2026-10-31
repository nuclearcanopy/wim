//! Pure application state + key-handling, decoupled from rendering so it
//! can be unit-tested without a real terminal (raw mode needs a tty we
//! don't have in this headless build/test context).

use std::cell::OnceCell;

use crossterm::event::KeyCode;

use crate::eq;
use crate::keymap::{KeyAction, Keymap};
use crate::search::{Corpus, Ranking, SearchItem};

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

impl Action {
    /// The undo tree's row label for a keypress, in the same voice as
    /// `Op::describe()` so a tree mixing both reads as one history.
    ///
    /// Not every action has an op to borrow a label from: `Queued`
    /// carries the item, where `Op::Enqueue` carries a target that was
    /// already resolved by the time it got here.
    pub fn describe(&self) -> String {
        match self {
            Action::None => "nothing".into(),
            Action::Quit => "quit".into(),
            Action::Play(item) => format!("played {}", item.primary_name()),
            Action::Queued(item) => format!("queued {}", item.primary_name()),
            Action::ShuffleToggled(on) => {
                format!("shuffle {}", if *on { "on" } else { "off" })
            }
            Action::SetLoop(mode) => format!("loop {}", mode.label()),
            Action::SetSpeed(speed) => format!("speed {speed}x"),
            Action::SetEq(name, _) => format!("eq {name}"),
        }
    }
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
    /// Never assigned from outside this file; `replace_corpus` is the
    /// swap, because `ranking` below is indices into one corpus state.
    /// Pinned by `only_app_replaces_the_whole_corpus`.
    pub corpus: Corpus,
    pub query: String,
    /// The ranked order for `query`: corpus indices plus the revision
    /// they address, never rows. A broad query at 40k items used to
    /// spend 3-5ms of a ~7ms keystroke cloning a `SearchItem` per
    /// match, most of them for rows nobody scrolls to; the draw path
    /// now resolves a screenful through `results_list::Ranked`.
    ranking: Ranking,
    /// `results()`'s rows, built on first ask and dropped on every
    /// re-rank. Only a caller that wants *every* row (a shellsong
    /// env, shellswim's result echo) pays the clone, and at most once
    /// per re-rank rather than once per keystroke.
    materialized: OnceCell<Vec<SearchItem>>,
    /// Which row the cursor is on. Private, with `select` as the only
    /// writer, because `anchor` below has to move with it.
    selected: usize,
    /// The identity of the row `selected` names, remembered as the
    /// selection moved.
    ///
    /// `resync_results` runs *after* the corpus has already been
    /// mutated under the order, which is the one moment the ranking
    /// can no longer resolve a row to ask its identity; the anchor is
    /// what survives that. One identity per selection move, not one
    /// clone per match per keystroke.
    anchor: Option<(u8, String)>,
    pub editing_query: bool,
    pub loop_mode: LoopMode,
    pub shuffle: bool,
    pub speed: f64,
    /// Where the `CycleEq` key's cycle currently sits in
    /// `eq::PRESETS`; `eq_source` is what the live gains actually
    /// came from, which may be a saved preset instead.
    pub eq_preset: usize,
    /// Which named curve `eq_gains` came from, a built-in or one of
    /// `config::Config::eq_custom_presets`, or `None` for a
    /// hand-tuned curve that never had a name. Kept across a band
    /// nudge, so the eq menu and the bottom bar can say "rock
    /// (modified)" instead of losing the name to an anonymous
    /// "custom".
    pub eq_source: Option<eq::Source>,
    /// True once any individual band has been hand-adjusted (via the
    /// eq menu) away from `eq_source`'s curve, i.e. the gains no
    /// longer match the name they came from; the third applied state,
    /// see `eq::applied_label`.
    pub eq_custom: bool,
    /// The actual gains currently applied to mpv's `af` filter: a
    /// copy of `eq_source`'s curve, or a hand-tuned set once
    /// `eq_custom` is true. The *requested* gains, never the
    /// compensated ones (see `eq::compensated_gains`). Always kept in
    /// sync with what's actually audible, so callers never have to
    /// re-derive it.
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
            // a default `Ranking` carries revision 0, which
            // `next_revision` never hands out, so it addresses no
            // corpus at all and reads as no rows: exactly right for
            // an app that has not been given a query yet.
            ranking: Ranking::default(),
            materialized: OnceCell::new(),
            selected: 0,
            anchor: None,
            editing_query: false,
            loop_mode: LoopMode::Off,
            shuffle: false,
            speed: 1.0,
            eq_preset: 0,
            eq_source: Some(eq::Source::Builtin(0)),
            eq_custom: false,
            eq_gains: eq::PRESETS[0].gains,
            scanning_library: false,
            syncing_servers: false,
            pending: None,
        }
    }

    /// Every row, as a slice, cloned out of the corpus on first ask
    /// and cached until the next re-rank.
    ///
    /// The only accessor that hands out the whole list, so it is the
    /// only one that materializes anything. The others ask about one
    /// row or a count, which the ranked index list answers without
    /// building a `SearchItem` at all; keeping them separate is the
    /// point of this seam, not ceremony. Two callers genuinely want
    /// every row (a shellsong env's per-row kinds, shellswim's result
    /// echo) and neither is on the keystroke path.
    pub fn results(&self) -> &[SearchItem] {
        self.materialized
            .get_or_init(|| self.ranking.rows(&self.corpus).cloned().collect())
    }

    /// The ranked order itself, for the draw path
    /// (`ui::results_list::Ranked`), which resolves the rows on screen
    /// against the corpus instead of taking a list.
    pub fn ranking(&self) -> &Ranking {
        &self.ranking
    }

    /// One row by index, or `None` past the end. Takes an index rather
    /// than letting callers slice, so an out-of-range row stays a
    /// `None` and never a panic.
    pub fn result(&self, at: usize) -> Option<&SearchItem> {
        self.ranking.row(&self.corpus, at)
    }

    pub fn result_count(&self) -> usize {
        if self.ranking.addresses(&self.corpus) {
            self.ranking.len()
        } else {
            0
        }
    }

    pub fn has_results(&self) -> bool {
        self.result_count() > 0
    }

    /// The row the cursor is on.
    ///
    /// `selected` is clamped by `move_selection`, but nothing stops it
    /// pointing past the end after the corpus shrinks under it, so
    /// this is `Option` and the three key paths that play or enqueue
    /// the selection all read it here rather than indexing themselves.
    pub fn selected_result(&self) -> Option<&SearchItem> {
        self.result(self.selected)
    }

    /// Which row the cursor is on, for the draw path and the hit map.
    pub fn selected(&self) -> usize {
        self.selected
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
                if let Some(item) = self.selected_result() {
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
                    self.select_top();
                    self.pending = None;
                } else {
                    self.pending = Some('g');
                }
                return Action::None;
            }
            KeyCode::Char('G') => {
                self.select_bottom();
                return Action::None;
            }
            KeyCode::Enter => {
                if let Some(item) = self.selected_result() {
                    return Action::Play(item.clone());
                }
                return Action::None;
            }
            _ => {}
        }

        if key == keymap.get(KeyAction::StartSearch) {
            self.editing_query = true;
        } else if key == keymap.get(KeyAction::Enqueue) {
            if let Some(item) = self.selected_result() {
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
            self.eq_source = Some(eq::Source::Builtin(self.eq_preset));
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

    /// The three selection primitives j/k and gg/G go through, public
    /// so shellsong's `up()`/`down()`/`top()`/`bottom()` move the
    /// selection by the exact same mutation the keyboard does rather
    /// than a second copy of the clamping.
    pub fn move_selection(&mut self, delta: isize) {
        if !self.has_results() {
            self.select(0);
            return;
        }
        let len = self.result_count() as isize;
        let new = (self.selected as isize + delta).clamp(0, len - 1);
        self.select(new as usize);
    }

    pub fn select_top(&mut self) {
        self.select(0);
    }

    pub fn select_bottom(&mut self) {
        self.select(self.result_count().saturating_sub(1));
    }

    /// The only writer of `selected`: the index and the identity
    /// anchor move together, so a later `resync_results` can name the
    /// row the cursor was on even once the corpus has shifted under
    /// the order.
    fn select(&mut self, at: usize) {
        self.selected = at;
        let identity = self.result(at).map(|item| {
            let (tag, key) = item.identity();
            (tag, key.into_owned())
        });
        match identity {
            Some(id) => self.anchor = Some(id),
            // no such row *and* the order is current, so this is a
            // real "nothing is selected" rather than a stale read;
            // keeping the anchor would restore onto it later.
            None if self.ranking.addresses(&self.corpus) => self.anchor = None,
            None => {}
        }
    }

    /// Replaces the query wholesale and re-filters, exactly as typing
    /// it would; backs `shellsong`'s `search(...)`. The caller still
    /// has to kick the debounced remote search (`notify_query_changed`),
    /// same as every other path that changes the query.
    pub fn set_query(&mut self, query: &str) {
        self.query = query.to_string();
        self.refresh_results();
    }

    /// Re-filters for a query the user just changed, which genuinely
    /// should jump back to the best match; anything that re-filters
    /// *under* an unchanged query goes through `resync_results`
    /// instead, which keeps the selection where it was.
    fn refresh_results(&mut self) {
        self.rerank();
        self.select(0);
    }

    /// Re-ranks against the corpus as it is now and throws away
    /// whatever `results()` had materialized, which belongs to the
    /// order just replaced. The one place `ranking` is written, so a
    /// fresh order and a stale row cache cannot coexist.
    fn rerank(&mut self) {
        self.ranking = self.corpus.rank(&self.query);
        self.materialized = OnceCell::new();
    }

    /// Puts the cursor back on the anchored row, or clamps if that row
    /// is genuinely gone. The scan walks the ranked order and borrows
    /// each row out of the corpus; finding one row again never needed
    /// a cloned list.
    fn restore_selection(&mut self) {
        let last = self.result_count().saturating_sub(1);
        let found = self.anchor.as_ref().and_then(|(tag, key)| {
            self.ranking.rows(&self.corpus).position(|item| {
                let (t, k) = item.identity();
                t == *tag && k.as_ref() == key.as_str()
            })
        });
        let at = match found {
            Some(at) => at,
            None if self.anchor.is_some() => self.selected.min(last),
            None => 0,
        };
        self.select(at);
    }

    /// Replaces the corpus and re-ranks against it, keeping the
    /// selection by identity exactly as `resync_results` does.
    ///
    /// The only way to swap a corpus, and that is the point: `ranking`
    /// is indices into one corpus state, so assigning the field and
    /// forgetting to re-rank leaves an order addressing a corpus that
    /// no longer exists. `Ranking` refuses such a read, so the
    /// symptom would be an empty results list rather than wrong rows,
    /// but onboarding's library-paths step did exactly that and an
    /// empty list after setting up your library is still a bug.
    pub fn replace_corpus(&mut self, corpus: Corpus) {
        self.corpus = corpus;
        self.rerank();
        self.restore_selection();
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
    /// previously selected item is found again by identity (`anchor`,
    /// since by here the order can no longer resolve a row); if it's
    /// genuinely gone from the new list, the index is clamped instead.
    pub fn resync_results(&mut self) {
        self.rerank();
        self.restore_selection();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::library::{LocalTrack, MediaKind};
    use std::path::PathBuf;

    /// `n` tracks all matching the fixture query, each indexed as its
    /// own batch so the corpus order is the obvious one.
    fn tracks(n: usize) -> Vec<LocalTrack> {
        (0..n)
            .map(|i| LocalTrack {
                path: PathBuf::from(format!("/music/track{i}.flac")),
                title: format!("matchable track {i}"),
                artist: None,
                album: None,
                duration_secs: None,
                kind: MediaKind::Music,
            })
            .collect()
    }

    fn corpus_of(n: usize) -> Corpus {
        let mut corpus = Corpus::new();
        for track in tracks(n) {
            corpus.add_local_tracks([track]);
        }
        corpus
    }

    fn app_with_three_results() -> App {
        app_with_results(3)
    }

    fn app_with_results(n: usize) -> App {
        let corpus = corpus_of(n);
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
        assert_eq!(app.result_count(), 3);
        assert_eq!(app.selected, 0);
        assert!(!app.editing_query);
    }

    fn selected_title(app: &App) -> String {
        match app.selected_result() {
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

        assert_eq!(app.result_count(), 4);
        assert_eq!(
            selected_title(&app),
            "matchable track 2",
            "a sync landing mid-scroll must not move the selection to another item"
        );
    }

    #[test]
    fn resync_clamps_when_the_selected_item_is_gone() {
        let mut app = app_with_results(4);
        app.select_bottom();
        assert_eq!(selected_title(&app), "matchable track 3");
        // a rescan that comes back one track shorter: the selected row
        // is not in the new list at all, so resync has nothing to
        // restore onto and clamps instead of jumping to the top.
        app.corpus.drop_local();
        app.corpus.add_local_tracks(tracks(3));
        app.resync_results();
        assert_eq!(app.result_count(), 3);
        assert_eq!(app.selected, 2);
    }

    /// `results()` clones rows out of the corpus on demand and caches
    /// them, so a re-rank has to drop that cache: a kept one answers
    /// the next query with the previous query's rows, which is not one
    /// stale row but the whole list wrong.
    #[test]
    fn the_materialized_rows_do_not_survive_a_rerank() {
        let mut app = app_with_three_results();
        assert_eq!(app.results().len(), 3, "materialized under the first query");

        app.set_query("zzzqqqxyw");
        assert_eq!(app.result_count(), 0);
        assert!(
            app.results().is_empty(),
            "results() answered from the order it was built for, not the current one"
        );

        app.set_query("matchable");
        // two rows the fixture's three did not have, so the count
        // moves and a cache kept across the resync would show 3.
        app.corpus.add_local_tracks(tracks(5).into_iter().skip(3));
        app.resync_results();
        assert_eq!(app.result_count(), 5);
        assert_eq!(app.results().len(), 5);
    }

    /// Replacing the corpus without re-ranking leaves an order
    /// addressing a corpus that no longer exists. `Ranking` refuses
    /// such a read, so onboarding pointing wim at a library would have
    /// emptied the results list rather than filling it.
    #[test]
    fn replacing_the_corpus_reranks_against_the_new_one() {
        let mut app = app_with_three_results();
        assert_eq!(app.result_count(), 3);

        app.replace_corpus(corpus_of(5));
        assert_eq!(app.result_count(), 5);
        assert_eq!(app.results().len(), 5);
        for at in 0..5 {
            assert!(app.result(at).is_some(), "row {at} of the new corpus");
        }
    }

    /// Same contract as a resync: the swap is not a query change, so
    /// the cursor stays on the row it was on even though that row's
    /// index moved.
    #[test]
    fn replacing_the_corpus_keeps_the_selection_on_the_same_item() {
        let mut app = app_with_three_results();
        app.select_bottom();
        assert_eq!(selected_title(&app), "matchable track 2");

        // the same three rows behind one that outranks all of them, so
        // the old index 2 names a different row in the new order.
        let mut fresh = Corpus::new();
        fresh.add_local_tracks([LocalTrack {
            path: PathBuf::from("/music/exact.flac"),
            title: "matchable".to_string(),
            artist: None,
            album: None,
            duration_secs: None,
            kind: MediaKind::Music,
        }]);
        fresh.add_local_tracks(tracks(3));
        app.replace_corpus(fresh);

        assert_eq!(app.result_count(), 4);
        assert_eq!(
            selected_title(&app),
            "matchable track 2",
            "a corpus swap must not move the selection to another item"
        );
    }

    /// The corpus may only be swapped through `replace_corpus`, and a
    /// source scan rather than a runtime assertion because what needs
    /// preventing is a *new* assignment: `ranking` is indices into one
    /// corpus state, so an assignment anywhere else leaves an order
    /// addressing a corpus that no longer exists, which reads as an
    /// empty list at best and is why the revision guard exists.
    #[test]
    fn only_app_replaces_the_whole_corpus() {
        fn rust_sources(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
            for entry in std::fs::read_dir(dir).expect("a readable dir").flatten() {
                let path = entry.path();
                if path.is_dir() {
                    rust_sources(&path, out);
                } else if path.extension().is_some_and(|e| e == "rs") {
                    out.push(path);
                }
            }
        }
        let src = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut sources = Vec::new();
        rust_sources(&src, &mut sources);
        assert!(sources.len() > 20, "the scan found almost nothing");

        let mut offenders = Vec::new();
        for path in sources {
            // this file: `replace_corpus` is the assignment.
            if path.file_name().is_some_and(|f| f == "app.rs") {
                continue;
            }
            let text = std::fs::read_to_string(&path).expect("a readable source file");
            for (n, line) in text.lines().enumerate() {
                if line.contains(".corpus =") {
                    offenders.push(format!("{}:{}", path.display(), n + 1));
                }
            }
        }
        assert!(
            offenders.is_empty(),
            "assign a corpus through App::replace_corpus: {offenders:?}"
        );
    }

    /// The count and the slice are two answers to the same question,
    /// and the lazy `results` this seam exists for would compute them
    /// from different things: the count from the ranked index list,
    /// the slice from the prefix actually built. Pinned now, while
    /// they cannot disagree, so the day they can this fails.
    #[test]
    fn the_count_and_the_slice_agree() {
        let app = app_with_three_results();
        assert_eq!(app.result_count(), app.results().len());
        assert_eq!(app.has_results(), !app.results().is_empty());
        for at in 0..app.result_count() {
            let (tag, key) = app.result(at).expect("a row in range").identity();
            let (t, k) = app.results()[at].identity();
            assert_eq!((tag, key), (t, k), "row {at} differs between the two");
        }
        assert!(
            app.result(app.result_count()).is_none(),
            "nothing past the end"
        );
    }

    /// `selected` can point past the end (the corpus shrank under it),
    /// so the accessor the play and enqueue paths read is `Option`.
    /// Returning a row here would play whatever landed at the clamped
    /// index instead, which is the wrong song, not a missing one.
    #[test]
    fn a_selection_past_the_end_reads_as_no_row() {
        let mut app = app_with_three_results();
        app.selected = 9;
        assert!(app.selected_result().is_none());
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
            app.result_count(),
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
