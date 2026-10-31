//! The app's map from an mpv playlist index to the `SearchItem` it was
//! built from, so "what is playing" is *resolved* out of mpv's own
//! `playlist-pos` rather than guessed at by counting events.
//!
//! Invariant: `entries` is mpv's playlist (same items, same order, same
//! length) and `pos` is the index mpv is playing. Advancing a track
//! only moves `pos`; entries are never removed, so index `i` always
//! resolves to the item mpv actually loaded there. Every rewrite of
//! mpv's playlist goes through `reset`/`rebuild`/`append`/
//! `replace_upcoming`, each paired with its ipc call in exactly one
//! place (`core::load_playlist`, `core::enqueue_track`,
//! `core::resync_upcoming`), so the two sides can only diverge for the
//! length of one of those calls.
//!
//! Stated the other way round, which is the form that actually caught
//! a bug: **every entry here is something mpv accepted.** An item mpv
//! was never given, or refused, must not be stored, however harmless
//! it looks; it shifts every later index and `playlist-pos` then
//! resolves to the wrong track (wrong title, wrong scrobble, wrong
//! art). Two of those three callers used to append what resolved and
//! store what did not, which was unreachable only because every call
//! site pre-expanded its items. `core::resolve_all` pairs each item
//! with the path mpv is handed, so the caller cannot store a list mpv
//! did not get.

use rand::Rng;

use crate::search::SearchItem;

/// What a `playlist-pos` mpv just reported means for the current track.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PosChange {
    /// mpv moved to a different, known entry; the caller runs its
    /// track-change hooks for the newly resolved `current()`.
    Advanced(usize),
    /// The index we already had: mpv confirming a position we set
    /// ourselves, or a repeat notification. Not a track change, so
    /// hooks must not fire again (that double-counted plays).
    Unchanged,
    /// No current entry (mpv reports -1 while idle, and transiently
    /// mid-`loadfile`), or an index past anything we know about. The
    /// map is left exactly as it was rather than inventing a track;
    /// end-of-playlist is reported separately, by `idle-active`.
    Unresolved,
}

/// Where an `append` landed and, while shuffle is on, where it should
/// be moved to; `entries` already reflects the final order, the caller
/// just has to issue the matching mpv `playlist-move`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Appended {
    pub appended_at: usize,
    pub moved_to: Option<usize>,
}

#[derive(Debug, Default)]
pub struct PlayQueue {
    entries: Vec<SearchItem>,
    pos: Option<usize>,
    /// Everything played or queued this session, kept purely so "loop
    /// all" has something to restart from. Deliberately *not*
    /// index-mapped to mpv: `replace_upcoming` drops the already-played
    /// prefix from both mpv and `entries`, and loop-all should still
    /// replay the whole thing.
    session: Vec<SearchItem>,
}

impl PlayQueue {
    /// A brand new play session: `items` is mpv's whole playlist, index
    /// 0 is what's playing now, and loop-all restarts from this list.
    pub fn reset(&mut self, items: Vec<SearchItem>) {
        self.session = items.clone();
        self.rebuild(items);
    }

    /// The same session re-laid-out (back/previous, a loop-all
    /// restart): index 0 is what's playing now, but the loop-all
    /// restart point is left alone.
    pub fn rebuild(&mut self, items: Vec<SearchItem>) {
        self.pos = (!items.is_empty()).then_some(0);
        self.entries = items;
    }

    /// Appends one track. With `shuffle` on it then moves the new entry
    /// to a random spot inside the upcoming region, so a later addition
    /// doesn't always land dead last in an already-shuffled queue (what
    /// `App::enqueue` used to do to its own copy of the queue only,
    /// desyncing it from mpv's append-only playlist on the spot).
    pub fn append(&mut self, item: SearchItem, shuffle: bool) -> Appended {
        self.session.push(item.clone());
        self.entries.push(item);
        let appended_at = self.entries.len() - 1;
        // the first upcoming slot and the appended slot being the same
        // means there's nowhere else for it to go.
        let first_upcoming = self.pos.map(|p| p + 1).unwrap_or(0);
        if !shuffle || first_upcoming >= appended_at {
            return Appended {
                appended_at,
                moved_to: None,
            };
        }
        let to = rand::thread_rng().gen_range(first_upcoming..appended_at);
        let item = self.entries.remove(appended_at);
        self.entries.insert(to, item);
        Appended {
            appended_at,
            moved_to: Some(to),
        }
    }

    /// Mirrors mpv's `playlist-clear`, which keeps the currently-
    /// playing entry and drops everything else: both sides end up as
    /// `[current] + items` at position 0. The already-played prefix
    /// goes from mpv, so it goes from the index map too.
    pub fn replace_upcoming(&mut self, items: Vec<SearchItem>) {
        let mut next = Vec::with_capacity(items.len() + 1);
        if let Some(current) = self.current().cloned() {
            next.push(current);
        }
        next.extend(items);
        self.rebuild(next);
    }

    /// Everything upcoming goes, and *stops counting* as part of the
    /// session.
    ///
    /// `replace_upcoming(vec![])` looks like this and is not: it
    /// relays the same session out again, which is right for a shuffle
    /// (mpv is cleared and refilled with the same tracks) and wrong
    /// here. the session is what loop-all restarts from, so clearing
    /// the queue through that left loop-all replaying the tracks the
    /// user had just deleted, seconds after deleting them.
    ///
    /// Already-played tracks stay in the session: the user emptied the
    /// *queue*, not the history, so loop-all still restarts from what
    /// actually played.
    pub fn clear_upcoming(&mut self) {
        let dropped: Vec<SearchItem> = self.upcoming().to_vec();
        for item in &dropped {
            self.forget(item);
        }
        let next: Vec<SearchItem> = self.current().cloned().into_iter().collect();
        self.rebuild(next);
    }

    /// Drops one occurrence of `item` from the session.
    ///
    /// One, not all: a track queued twice and deleted once is still
    /// queued once, and loop-all should still see it.
    fn forget(&mut self, item: &SearchItem) {
        let (tag, key) = item.identity();
        if let Some(at) = self.session.iter().position(|other| {
            let (t, k) = other.identity();
            t == tag && k == key
        }) {
            self.session.remove(at);
        }
    }

    /// Drops the entry at `index`, keeping `pos` pointing at the same
    /// *track* rather than the same number. Removing the playing entry
    /// leaves `pos` where it is, which is correct: mpv moves on to the
    /// next file, and that file has just shifted into this slot. The
    /// observed `playlist-pos` corrects anything this gets wrong on
    /// the next notification either way.
    pub fn remove(&mut self, index: usize) -> bool {
        if index >= self.entries.len() {
            return false;
        }
        // out of the session too, or loop-all restarts from a list
        // still holding the track the user just deleted.
        let gone = self.entries[index].clone();
        self.forget(&gone);
        self.entries.remove(index);
        if let Some(p) = self.pos {
            if index < p {
                self.pos = Some(p - 1);
            } else if self.entries.is_empty() {
                self.pos = None;
            }
        }
        true
    }

    /// Moves the entry at `from` to land at index `to`, carrying `pos`
    /// with it so the same track stays current. Mirrors what the caller
    /// must ask mpv for via `Mpv::playlist_move`.
    pub fn move_entry(&mut self, from: usize, to: usize) -> bool {
        let len = self.entries.len();
        if from >= len || to >= len || from == to {
            return false;
        }
        let item = self.entries.remove(from);
        self.entries.insert(to, item);
        if let Some(p) = self.pos {
            self.pos = Some(if p == from {
                to
            } else if from < p && to >= p {
                p - 1
            } else if from > p && to <= p {
                p + 1
            } else {
                p
            });
        }
        true
    }

    /// The upcoming tracks in a fresh random order, for the caller to
    /// hand straight back to `replace_upcoming` (Fisher-Yates, same as
    /// the old in-place queue shuffle).
    pub fn shuffled_upcoming(&self) -> Vec<SearchItem> {
        let mut items = self.upcoming().to_vec();
        let mut rng = rand::thread_rng();
        for i in (1..items.len()).rev() {
            items.swap(i, rng.gen_range(0..=i));
        }
        items
    }

    /// Resolves a raw `playlist-pos` (mpv sends -1 for "nothing") back
    /// to a known entry. Absolute, not incremental: a notification that
    /// never arrived can only delay the next one, never leave the map
    /// permanently off by one the way counting end-of-file events did.
    pub fn set_pos(&mut self, raw: i64) -> PosChange {
        let Ok(idx) = usize::try_from(raw) else {
            return PosChange::Unresolved;
        };
        if idx >= self.entries.len() {
            return PosChange::Unresolved;
        }
        if self.pos == Some(idx) {
            return PosChange::Unchanged;
        }
        self.pos = Some(idx);
        PosChange::Advanced(idx)
    }

    pub fn current(&self) -> Option<&SearchItem> {
        self.entries.get(self.pos?)
    }

    /// Everything after the current track, i.e. what the bottom bar
    /// counts as "the queue"; played entries stay in `entries` for
    /// index resolution but are not queued any more.
    pub fn upcoming(&self) -> &[SearchItem] {
        match self.pos {
            Some(p) => self.entries.get(p + 1..).unwrap_or(&[]),
            None => &self.entries,
        }
    }

    /// The whole mpv playlist, played prefix included, so a queue view
    /// can show where you are inside it rather than only what's left.
    /// Index into this is the same index mpv reports as `playlist-pos`.
    pub fn entries(&self) -> &[SearchItem] {
        &self.entries
    }

    /// Index into `entries` mpv says it's playing, if anything.
    pub fn pos(&self) -> Option<usize> {
        self.pos
    }

    pub fn upcoming_len(&self) -> usize {
        self.upcoming().len()
    }

    pub fn peek_next(&self) -> Option<&SearchItem> {
        self.upcoming().first()
    }

    /// The list loop-all restarts from: everything played or queued
    /// this session, surviving the `playlist-clear` a shuffle does.
    /// Empty when nothing has played.
    pub fn session(&self) -> &[SearchItem] {
        &self.session
    }

    pub fn clear(&mut self) {
        self.entries.clear();
        self.session.clear();
        self.pos = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn song(title: &str) -> SearchItem {
        SearchItem::LocalMusic {
            path: std::path::PathBuf::from(format!("/music/{title}.flac")),
            title: title.to_string(),
            artist: None,
            album: None,
        }
    }

    fn title_of(item: Option<&SearchItem>) -> Option<String> {
        match item? {
            SearchItem::LocalMusic { title, .. } => Some(title.clone()),
            _ => None,
        }
    }

    fn titles(items: &[SearchItem]) -> Vec<String> {
        items
            .iter()
            .filter_map(|i| title_of(Some(i)))
            .collect::<Vec<_>>()
    }

    fn queue_of(names: &[&str]) -> PlayQueue {
        let mut q = PlayQueue::default();
        q.reset(names.iter().map(|n| song(n)).collect());
        q
    }

    #[test]
    fn a_fresh_session_starts_on_its_first_track() {
        let q = queue_of(&["obedient", "hex", "for you"]);
        assert_eq!(title_of(q.current()).as_deref(), Some("obedient"));
        assert_eq!(q.upcoming_len(), 2);
        assert_eq!(title_of(q.peek_next()).as_deref(), Some("hex"));
    }

    #[test]
    fn a_reported_position_resolves_to_that_exact_entry() {
        let mut q = queue_of(&["obedient", "hex", "for you"]);
        assert_eq!(q.set_pos(1), PosChange::Advanced(1));
        assert_eq!(title_of(q.current()).as_deref(), Some("hex"));
        assert_eq!(q.upcoming_len(), 1);
    }

    #[test]
    fn re_reporting_the_same_position_is_not_a_track_change() {
        let mut q = queue_of(&["obedient", "hex"]);
        // the position a rewrite already set itself gets echoed back by
        // mpv; firing the track-change hooks again double-counted the
        // play and re-fired "playing now".
        assert_eq!(q.set_pos(0), PosChange::Unchanged);
        assert_eq!(q.set_pos(1), PosChange::Advanced(1));
        assert_eq!(q.set_pos(1), PosChange::Unchanged);
    }

    #[test]
    fn a_missed_advance_corrects_itself_instead_of_desyncing_forever() {
        let mut q = queue_of(&["a", "b", "c", "d"]);
        // mpv advanced 0 -> 1 -> 2 but only the second notification
        // arrived; the absolute index still lands on the right track,
        // which counting events could never do.
        assert_eq!(q.set_pos(2), PosChange::Advanced(2));
        assert_eq!(title_of(q.current()).as_deref(), Some("c"));
        assert_eq!(titles(q.upcoming()), vec!["d"]);
    }

    #[test]
    fn an_out_of_order_notification_does_not_shift_the_mapping() {
        let mut q = queue_of(&["a", "b", "c"]);
        assert_eq!(q.set_pos(2), PosChange::Advanced(2));
        // a stale notification for an earlier index is still just "mpv
        // is at index 1": resolved absolutely, not applied as a delta.
        assert_eq!(q.set_pos(1), PosChange::Advanced(1));
        assert_eq!(title_of(q.current()).as_deref(), Some("b"));
    }

    #[test]
    fn no_current_entry_leaves_the_mapping_untouched() {
        let mut q = queue_of(&["a", "b"]);
        assert_eq!(q.set_pos(1), PosChange::Advanced(1));
        // -1 (idle) and an out-of-range index are both "can't resolve
        // that", not "forget what's playing".
        assert_eq!(q.set_pos(-1), PosChange::Unresolved);
        assert_eq!(q.set_pos(9), PosChange::Unresolved);
        assert_eq!(title_of(q.current()).as_deref(), Some("b"));
    }

    #[test]
    fn shuffling_keeps_the_current_track_resolvable_at_index_zero() {
        let mut q = queue_of(&["a", "b", "c", "d", "e"]);
        assert_eq!(q.set_pos(1), PosChange::Advanced(1));
        let shuffled = q.shuffled_upcoming();
        assert_eq!(shuffled.len(), 3);
        q.replace_upcoming(shuffled);
        // playlist-clear leaves the current file at index 0 on mpv's
        // side, so that's where it has to resolve on ours.
        assert_eq!(title_of(q.current()).as_deref(), Some("b"));
        assert_eq!(q.upcoming_len(), 3);
        let mut after = titles(q.upcoming());
        after.sort();
        assert_eq!(after, vec!["c", "d", "e"], "shuffle must not lose tracks");
    }

    #[test]
    fn a_position_after_a_shuffle_still_resolves_to_the_right_track() {
        let mut q = queue_of(&["a", "b", "c", "d"]);
        q.replace_upcoming(vec![song("d"), song("b"), song("c")]);
        assert_eq!(q.set_pos(2), PosChange::Advanced(2));
        assert_eq!(title_of(q.current()).as_deref(), Some("b"));
    }

    #[test]
    fn going_back_rebuilds_the_playlist_without_losing_the_session() {
        let mut q = queue_of(&["a", "b", "c"]);
        assert_eq!(q.set_pos(1), PosChange::Advanced(1));
        // what main's `go_to_previous` hands over: the previous track,
        // then the interrupted one, then whatever was still upcoming.
        let mut items = vec![song("a"), song("b")];
        items.extend(q.upcoming().to_vec());
        q.rebuild(items);
        assert_eq!(title_of(q.current()).as_deref(), Some("a"));
        assert_eq!(titles(q.upcoming()), vec!["b", "c"]);
        // and a position reported after that back-step resolves against
        // the new layout, not the old one.
        assert_eq!(q.set_pos(1), PosChange::Advanced(1));
        assert_eq!(title_of(q.current()).as_deref(), Some("b"));
        assert_eq!(titles(q.session()), vec!["a", "b", "c"]);
    }

    #[test]
    fn starting_a_new_session_mid_playback_does_not_advance_the_view() {
        // the reported bug, directly: an album is playing, the user
        // hits enter on something else. `loadfile replace` makes mpv
        // emit an end-file for the *evicted* track, which the old
        // event-counting advance treated as "we moved on" and so
        // showed the album's next track ("hex") while mpv played the
        // new one ("obedient"), permanently.
        let mut q = queue_of(&["track one", "hex", "for you"]);
        q.reset(vec![song("obedient")]);
        assert_eq!(q.set_pos(0), PosChange::Unchanged);
        assert_eq!(title_of(q.current()).as_deref(), Some("obedient"));
        assert_eq!(q.upcoming_len(), 0);
    }

    #[test]
    fn a_track_mpv_skipped_over_does_not_shift_everything_after_it() {
        // a file that fails to load leaves mpv on the *next* index; the
        // display has to follow mpv, not assume one advance per track.
        let mut q = queue_of(&["a", "b", "broken", "d"]);
        assert_eq!(q.set_pos(3), PosChange::Advanced(3));
        assert_eq!(title_of(q.current()).as_deref(), Some("d"));
        assert_eq!(q.upcoming_len(), 0);
    }

    #[test]
    fn appending_lands_at_the_end_while_shuffle_is_off() {
        let mut q = queue_of(&["a", "b"]);
        let appended = q.append(song("c"), false);
        assert_eq!(
            appended,
            Appended {
                appended_at: 2,
                moved_to: None
            }
        );
        assert_eq!(titles(q.upcoming()), vec!["b", "c"]);
    }

    #[test]
    fn appending_while_shuffled_lands_somewhere_still_upcoming() {
        let mut q = queue_of(&["a", "b", "c", "d"]);
        let appended = q.append(song("new"), true);
        let to = appended.moved_to.expect("a non-empty queue has room");
        assert!(
            (1..4).contains(&to),
            "landed at {to}, outside the upcoming region"
        );
        assert_eq!(titles(q.upcoming()).len(), 4);
        assert_eq!(title_of(q.current()).as_deref(), Some("a"));
        assert!(titles(q.upcoming()).contains(&"new".to_string()));
    }

    #[test]
    fn appending_to_an_empty_upcoming_region_has_nowhere_to_move_to() {
        let mut q = queue_of(&["a"]);
        assert_eq!(q.append(song("b"), true).moved_to, None);
        assert_eq!(titles(q.upcoming()), vec!["b"]);
    }

    #[test]
    /// The flip side of the test below, and the bug: clearing the
    /// queue used to go through `replace_upcoming(vec![])`, which
    /// relays the *same* session out again. so with loop-all on, the
    /// tracks the user had just deleted came back the moment the
    /// current one ended.
    #[test]
    fn clearing_the_queue_takes_those_tracks_out_of_the_session() {
        let mut q = queue_of(&["a", "b", "c"]);
        q.append(song("queued later"), false);
        assert_eq!(q.set_pos(1), PosChange::Advanced(1));

        q.clear_upcoming();

        assert_eq!(q.upcoming_len(), 0, "nothing is queued any more");
        let mut session = titles(q.session());
        session.sort();
        // "a" played and "b" is playing, so loop-all still restarts
        // from those; the user emptied the queue, not the history.
        assert_eq!(session, vec!["a", "b"], "c and queued later are gone");
    }

    /// Deleting one row in the queue view, same rule.
    #[test]
    fn removing_a_track_takes_it_out_of_the_session_too() {
        let mut q = queue_of(&["a", "b", "c"]);
        assert!(q.remove(2));
        let mut session = titles(q.session());
        session.sort();
        assert_eq!(session, vec!["a", "b"]);
    }

    /// One occurrence, not every match: a track queued twice and
    /// deleted once is still queued once, and loop-all must still see
    /// it.
    #[test]
    fn deleting_one_copy_of_a_twice_queued_track_leaves_the_other() {
        let mut q = queue_of(&["a"]);
        q.append(song("twice"), false);
        q.append(song("twice"), false);
        assert_eq!(
            titles(q.session())
                .iter()
                .filter(|t| **t == "twice")
                .count(),
            2
        );

        assert!(q.remove(1));

        assert_eq!(
            titles(q.session())
                .iter()
                .filter(|t| **t == "twice")
                .count(),
            1,
            "forgot both copies"
        );
    }

    #[test]
    fn loop_all_restarts_from_everything_played_or_queued_this_session() {
        let mut q = queue_of(&["a", "b"]);
        q.append(song("queued later"), false);
        assert_eq!(q.set_pos(1), PosChange::Advanced(1));
        // a shuffle drops the already-played prefix from mpv (and so
        // from `entries`), but loop-all still has the whole session.
        q.replace_upcoming(q.shuffled_upcoming());
        assert_eq!(q.upcoming_len(), 1);
        let mut session = titles(q.session());
        session.sort();
        assert_eq!(session, vec!["a", "b", "queued later"]);
    }

    #[test]
    fn an_empty_queue_has_nothing_current_and_nothing_upcoming() {
        let mut q = queue_of(&["a"]);
        q.clear();
        assert!(q.current().is_none());
        assert_eq!(q.upcoming_len(), 0);
        assert_eq!(q.set_pos(0), PosChange::Unresolved);
    }

    #[test]
    fn removing_before_the_current_track_keeps_the_same_track_current() {
        let mut q = PlayQueue::default();
        q.reset(vec![song("a"), song("b"), song("c")]);
        q.set_pos(2);
        assert!(q.remove(0));
        assert_eq!(titles(q.entries()), vec!["b", "c"]);
        assert_eq!(title_of(q.current()).as_deref(), Some("c"));
    }

    #[test]
    fn removing_after_the_current_track_leaves_pos_alone() {
        let mut q = PlayQueue::default();
        q.reset(vec![song("a"), song("b"), song("c")]);
        q.set_pos(0);
        assert!(q.remove(2));
        assert_eq!(title_of(q.current()).as_deref(), Some("a"));
        assert_eq!(q.upcoming_len(), 1);
    }

    #[test]
    fn removing_the_current_track_lands_on_whatever_shifted_into_it() {
        let mut q = PlayQueue::default();
        q.reset(vec![song("a"), song("b"), song("c")]);
        q.set_pos(1);
        assert!(q.remove(1));
        // mpv moves on to the next file, which is now at this index.
        assert_eq!(title_of(q.current()).as_deref(), Some("c"));
    }

    #[test]
    fn removing_the_last_entry_clears_the_position() {
        let mut q = PlayQueue::default();
        q.reset(vec![song("a")]);
        q.set_pos(0);
        assert!(q.remove(0));
        assert!(q.entries().is_empty());
        assert_eq!(q.pos(), None);
    }

    #[test]
    fn remove_out_of_range_is_refused_not_a_panic() {
        let mut q = PlayQueue::default();
        q.reset(vec![song("a")]);
        assert!(!q.remove(9));
        assert_eq!(q.entries().len(), 1);
    }

    #[test]
    fn moving_an_upcoming_entry_reorders_without_moving_playback() {
        let mut q = PlayQueue::default();
        q.reset(vec![song("a"), song("b"), song("c"), song("d")]);
        q.set_pos(0);
        // pull "d" up to sit right after the current track
        assert!(q.move_entry(3, 1));
        assert_eq!(titles(q.entries()), vec!["a", "d", "b", "c"]);
        assert_eq!(title_of(q.current()).as_deref(), Some("a"));
    }

    #[test]
    fn moving_the_current_entry_carries_the_position_with_it() {
        let mut q = PlayQueue::default();
        q.reset(vec![song("a"), song("b"), song("c")]);
        q.set_pos(0);
        assert!(q.move_entry(0, 2));
        assert_eq!(titles(q.entries()), vec!["b", "c", "a"]);
        assert_eq!(title_of(q.current()).as_deref(), Some("a"));
        assert_eq!(q.pos(), Some(2));
    }

    #[test]
    fn moving_across_the_current_track_shifts_pos_by_one() {
        let mut q = PlayQueue::default();
        q.reset(vec![song("a"), song("b"), song("c")]);
        q.set_pos(1);
        // "a" moves below the current track, so current slides up
        assert!(q.move_entry(0, 2));
        assert_eq!(titles(q.entries()), vec!["b", "c", "a"]);
        assert_eq!(title_of(q.current()).as_deref(), Some("b"));
        assert_eq!(q.pos(), Some(0));

        let mut q = PlayQueue::default();
        q.reset(vec![song("a"), song("b"), song("c")]);
        q.set_pos(1);
        // and the other direction: "c" moves above it
        assert!(q.move_entry(2, 0));
        assert_eq!(titles(q.entries()), vec!["c", "a", "b"]);
        assert_eq!(title_of(q.current()).as_deref(), Some("b"));
        assert_eq!(q.pos(), Some(2));
    }

    #[test]
    fn a_no_op_or_out_of_range_move_is_refused() {
        let mut q = PlayQueue::default();
        q.reset(vec![song("a"), song("b")]);
        assert!(!q.move_entry(0, 0));
        assert!(!q.move_entry(0, 5));
        assert!(!q.move_entry(5, 0));
        assert_eq!(titles(q.entries()), vec!["a", "b"]);
    }
}
