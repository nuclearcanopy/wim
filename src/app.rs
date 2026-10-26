//! Pure application state + key-handling, decoupled from rendering so it
//! can be unit-tested without a real terminal (raw mode needs a tty we
//! don't have in this headless build/test context).

use crossterm::event::KeyCode;

use crate::search::{Corpus, SearchItem};

pub enum Action {
    None,
    Quit,
    Play(SearchItem),
    Queued(SearchItem),
}

pub struct App {
    pub corpus: Corpus,
    pub query: String,
    pub results: Vec<SearchItem>,
    pub selected: usize,
    pub editing_query: bool,
    /// explicit play queue — deliberately *not* auto-populated from
    /// search results on enter (unlike album-browsing players): in a
    /// search-first ui the "next" search result usually isn't a related
    /// track, so implicitly bulk-queuing them would be surprising. `a`
    /// enqueues explicitly instead. See docs/features.md.
    pub queue: Vec<SearchItem>,
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
            pending: None,
        }
    }

    /// Pops and returns the next queued item, for the caller to actually
    /// hand to mpv on end-of-file (auto-advance). `App` doesn't know
    /// whether playback succeeds — that's main.rs's concern.
    pub fn pop_queue(&mut self) -> Option<SearchItem> {
        if self.queue.is_empty() {
            None
        } else {
            Some(self.queue.remove(0))
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
                    return Action::Play(item.clone());
                }
            }
            KeyCode::Char('a') => {
                if let Some(item) = self.results.get(self.selected) {
                    let item = item.clone();
                    self.queue.push(item.clone());
                    return Action::Queued(item);
                }
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
    fn j_and_k_move_selection_and_clamp_at_bounds() {
        let mut app = app_with_three_results();
        app.handle_key(KeyCode::Char('j'));
        assert_eq!(app.selected, 1);
        app.handle_key(KeyCode::Char('j'));
        app.handle_key(KeyCode::Char('j'));
        assert_eq!(app.selected, 2, "clamps at the last result, doesn't wrap/overflow");
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
        assert_eq!(app.selected, 2, "already at bottom; j is clamped, not a stray top-jump");
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
        assert!(matches!(first, Some(SearchItem::LocalMusic { ref title, .. }) if title == "matchable track 0"));
        let second = app.pop_queue();
        assert!(matches!(second, Some(SearchItem::LocalMusic { ref title, .. }) if title == "matchable track 1"));
        assert!(app.pop_queue().is_none());
    }
}
