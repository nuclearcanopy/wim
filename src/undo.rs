//! Undo and redo as a tree, not a stack.
//!
//! The maintainer's ask was "nice decision trees that should be super
//! smart and dynamic". A tree means undoing three steps and then doing
//! something new does not destroy the branch you left: it becomes a
//! sibling you can walk back to, the way vim's undo tree works. For an
//! app called wim that is the right shape.
//!
//! The full design, including the op-by-op classification, is in
//! `docs/undo.md`. Two things here differ from what that document said
//! and the difference was found by writing this:
//!
//! **Snapshots are taken *after* the op, not before.** The doc said
//! before, and before works for undo alone: restore the state the op
//! replaced. But redo then has nothing to restore, because the state
//! *after* an op is recorded nowhere. Storing the after-state makes
//! the two symmetric (undo restores the parent's, redo the child's)
//! and the root holds the state the session started in.
//!
//! **Coalescing keeps the latest snapshot, not the first.** The doc's
//! reasoning ("that is the state the user wants back") is right and the
//! conclusion inverted: with after-states, the state before a run of
//! volume nudges is the *parent's* snapshot, which coalescing never
//! touches. The run's own node should hold where the run ended, so redo
//! lands after it rather than in the middle.
//!
//! The tree is generic over the snapshot so none of this logic depends
//! on what wim's undoable state turns out to be; see `classify` for the
//! half that does know about ops.

use std::time::Duration;

use crate::app::{Action, LoopMode};
use crate::core::{
    apply_eq_gains, load_playlist, notify_query_changed, resync_upcoming, CoreCtx, SessionStart,
};
use crate::eq;
use crate::search::SearchItem;
use crate::shellsong::Op;
use crate::{shellsong, shellsong_host};

/// Everything undo restores.
///
/// Small on purpose: this is what makes snapshotting viable at all
/// instead of maintaining a correct inverse per op, which is the one
/// design call `docs/undo.md` has never had to revisit.
///
/// **What is deliberately not in here**, each for its own reason:
///
/// - **`paused`.** `TogglePause` is classified as undoable and it is,
///   but *restoring* it is not what anyone wants: undo would start or
///   stop the music as a side effect of reversing something else,
///   which reads as a bug. Pressing play is already the undo of
///   pressing pause.
/// - **a live fade.** A `Ramp` is a position in a transition, not
///   state; restoring one mid-flight would resume a fade towards a
///   volume nobody asked for any more.
/// - **mpv's instantaneous volume.** `base_volume` is the volume that
///   was *asked for*; the crossfade ramp moves the real one around it
///   every frame. Restoring the instantaneous value would capture a
///   ramp artefact as if it were a decision.
///
/// `mute` *is* in here despite also living only in mpv, because unlike
/// `paused` it cannot drift out of band: `mute()`/`unmute()` are its
/// only mutation and both are recorded ops. See docs/undo.md.
#[derive(Debug, Clone)]
pub struct Snapshot {
    /// What was playing, so a restore can tell whether the current
    /// track survived and avoid restarting it if it did.
    pub current: Option<SearchItem>,
    pub upcoming: Vec<SearchItem>,
    /// What skip-back goes to. Restored because it is state the ops
    /// move: `next` makes the track it left the previous one, so an
    /// undo that puts the track back and not this leaves skip-back
    /// pointing at the track already playing.
    pub previous_track: Option<SearchItem>,
    pub base_volume: f64,
    /// `None` when mpv would not say, so a restore leaves it alone
    /// rather than guessing "unmuted" and unmuting.
    pub muted: Option<bool>,
    pub loop_mode: LoopMode,
    pub shuffle: bool,
    pub speed: f64,
    pub eq_source: Option<eq::Source>,
    pub eq_gains: [f64; 10],
    /// Whether the gains have been nudged away from `eq_source`'s
    /// curve. Both halves or neither: the name alone reads "flat" for
    /// a modified curve, and the flag alone reads "flat (modified)"
    /// for a restored clean one.
    pub eq_custom: bool,
    pub crossfade: bool,
    pub gap_secs: f64,
    pub query: String,
    pub sleep_due: Option<Duration>,
    /// Vars and bindings are in the snapshot because `classify` files
    /// `define`/`unset`/`remember`/`forget` as undoable; see
    /// `docs/undo.md` for why those are not barriers.
    pub vars: shellsong::Scope,
    pub bindings: shellsong_host::Bindings,
}

impl Snapshot {
    /// Reads the current state. One ipc read, for `mute`, which has no
    /// host-side mirror to read instead; the rest is memory. That read
    /// is once per recorded op rather than per frame, which is the
    /// budget `paused` and the live volume were refused on.
    pub fn capture(core: &CoreCtx) -> Snapshot {
        Snapshot {
            current: core.queue.current().cloned(),
            upcoming: core.queue.upcoming().to_vec(),
            previous_track: core.previous_track.clone(),
            base_volume: *core.base_volume,
            muted: core
                .player
                .get_property("mute")
                .ok()
                .and_then(|v| v.as_bool()),
            loop_mode: core.app.loop_mode,
            shuffle: core.app.shuffle,
            speed: core.app.speed,
            eq_source: core.app.eq_source,
            eq_gains: core.app.eq_gains,
            eq_custom: core.app.eq_custom,
            crossfade: *core.crossfade_enabled,
            gap_secs: core.between_tracks.gap_secs(),
            query: core.app.query.clone(),
            sleep_due: *core.sleep_due,
            vars: core.vars.clone(),
            bindings: core.bindings.clone(),
        }
    }

    /// Puts the state back, and says what it could not.
    ///
    /// The queue is the delicate half, and the rule is the one
    /// `docs/undo.md` sets: **do not restart the current track if it
    /// survives the restore**, or undo reads as a skip. So the two
    /// cases go through the two functions that already write mpv's
    /// playlist and the index map together:
    ///
    /// - same track playing: `resync_upcoming`, which keeps mpv's
    ///   current file and rewrites only what comes after it.
    /// - a different track (or nothing) playing: `load_playlist` with
    ///   `SessionStart::Keep`, which does restart playback, and has to.
    ///
    /// **Known gap, and it needs a `Surface` to close**: the restart
    /// case loads the playlist without firing the `track_started`
    /// hooks, so a restored track gets no subtitle fetch, no
    /// playing-now submission and no bound settings. See docs/undo.md.
    pub fn restore(&self, core: &mut CoreCtx) -> Result<(), String> {
        core.app.loop_mode = self.loop_mode;
        core.app.shuffle = self.shuffle;
        core.app.speed = self.speed;
        core.app.eq_source = self.eq_source;
        core.app.eq_gains = self.eq_gains;
        core.app.eq_custom = self.eq_custom;
        *core.crossfade_enabled = self.crossfade;
        core.between_tracks.set_gap(self.gap_secs);
        *core.sleep_due = self.sleep_due;
        *core.base_volume = self.base_volume;
        // `set_query` only re-filters the corpus already indexed; the
        // debounced remote search is the caller's to kick, and
        // `settle` has to be told or it reports a query settled that
        // nothing was ever fired for (`src/app.rs`'s set_query).
        core.app.set_query(&self.query);
        let fired = notify_query_changed(core.app, core.remote_search_tx);
        core.settle.notified(fired, core.now);

        if *core.vars != self.vars {
            *core.vars = self.vars.clone();
            shellsong_host::save_vars(core.vars);
        }
        if *core.bindings != self.bindings {
            *core.bindings = self.bindings.clone();
            shellsong_host::save_bindings(core.bindings);
        }

        let mpv_loop = if self.loop_mode == LoopMode::One {
            "inf"
        } else {
            "no"
        };
        let _ = core.player.set_loop_file(mpv_loop);
        let _ = core.player.set_speed(self.speed);
        let _ = core.player.set_volume(self.base_volume);
        if let Some(muted) = self.muted {
            let _ = core.player.set_property("mute", serde_json::json!(muted));
        }
        apply_eq_gains(core.app, core.player, core.notifications);

        let restored = self.restore_queue(core);
        // after the queue: `load_playlist`'s own caller
        // (`start_session`) writes this slot while it rewrites the
        // playlist, so restoring it first would be overwritten.
        *core.previous_track = self.previous_track.clone();
        restored
    }

    fn restore_queue(&self, core: &mut CoreCtx) -> Result<(), String> {
        // every branch below rewrites the queue, so anything still
        // being resolved for the old one has nowhere to go; hoisted
        // out of the branches because the branch that forgot it
        // appended a whole discography over a finished undo.
        core.track_resolver.invalidate();
        let playing = core.queue.current().map(|i| i.identity());
        let wanted = self.current.as_ref().map(|i| i.identity());
        if playing == wanted {
            // the track survives, so only what comes after it moves.
            let status =
                resync_upcoming(core.player, core.cache, core.queue, self.upcoming.clone());
            if status.starts_with("error") {
                return Err(status);
            }
            return Ok(());
        }
        let Some(current) = self.current.clone() else {
            // nothing was playing then, and something is now. stopping
            // playback is a bigger side effect than leaving it, so the
            // queue after it is restored and the track is left alone.
            let status =
                resync_upcoming(core.player, core.cache, core.queue, self.upcoming.clone());
            if status.starts_with("error") {
                return Err(status);
            }
            return Ok(());
        };
        let mut items = Vec::with_capacity(self.upcoming.len() + 1);
        items.push(current);
        items.extend(self.upcoming.iter().cloned());
        let (status, played) = load_playlist(
            core.player,
            core.cache,
            core.queue,
            items,
            SessionStart::Keep,
        );
        if played {
            Ok(())
        } else {
            Err(status)
        }
    }
}

/// A run of the same adjusting op inside this window is one node.
///
/// Without coalescing the tree fills with noise and the feature dies:
/// holding `+` through twelve `SpeedUp`s, or walking the volume up, is
/// one decision and should cost one row.
pub const COALESCE_WINDOW: Duration = Duration::from_millis(600);

/// Whether an op can be undone, and if not, why not.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reversibility {
    /// Restoring the snapshot is the whole story.
    Restorable,
    /// The op left the process. Restoring a snapshot around it would
    /// restore the *rest* of the state and leave the effect in place,
    /// which is worse than refusing, because the user believes undo
    /// worked. Undo may walk up to one of these and stop, saying what
    /// it cannot reverse; it may not step over it and pretend.
    Barrier(&'static str),
    /// Navigation, not an edit. Recording these buries the real
    /// changes under noise, which is how undo trees become useless.
    Ignored,
}

/// Which class `op` falls into.
///
/// Exhaustive on purpose: a new op has to be classified here or this
/// stops compiling, which is the only way the tree stays honest as the
/// language grows.
pub fn classify(op: &Op) -> Reversibility {
    use Reversibility::{Barrier, Ignored, Restorable};
    match op {
        // --- plain state, restoring the snapshot is the whole story
        Op::TogglePause
        | Op::SkipNext(_)
        | Op::SkipBack
        | Op::SeekAbsolute(_)
        | Op::SeekRelative(_)
        | Op::SetVolume(_)
        | Op::Mute(_)
        | Op::PauseFor(_)
        | Op::CycleLoop
        | Op::SetLoop(_)
        | Op::ToggleShuffle
        | Op::SetShuffle(_)
        | Op::SetSpeed(_)
        | Op::SpeedUp
        | Op::SpeedDown
        | Op::SetEqPreset(_)
        | Op::ToggleCrossfade
        | Op::Search(_)
        | Op::Play(_)
        | Op::Enqueue { .. }
        | Op::ClearQueue
        | Op::RemoveFromQueue(_)
        | Op::GotoQueue(_)
        | Op::ShuffleQueue
        | Op::LoadQueue(_)
        | Op::Sleep(_)
        | Op::Fade { .. }
        | Op::SetGap(_)
        | Op::Silence { .. }
        // `Go` reads like navigation and is not: it jumps the queue.
        | Op::Go(_) => Restorable,
        // wim's own named state, and reversible *because* it is wim's:
        // restoring the scope and writing it back leaves nothing
        // behind, unlike a file in the user's music directory. this
        // does mean the snapshot has to carry vars and bindings, which
        // is the price of being able to undo an accidental `unset`.
        Op::Define { .. } | Op::Unset(_) | Op::Remember { .. } | Op::Forget { .. } => Restorable,

        // --- left the process; undo refuses with a reason
        Op::Star { .. } => Barrier("a star is a write to the server"),
        Op::Download(_) => Barrier("a download wrote a file"),
        Op::SaveQueue(_) => Barrier("a saved queue wrote a file"),
        Op::OpenPage(_) => Barrier("a page was handed to a browser"),
        Op::Rescan => Barrier("a rescan rebuilt the library from disk"),

        // --- never recorded
        Op::MoveSelection(_)
        | Op::SelectTop
        | Op::SelectBottom
        | Op::View(_)
        | Op::ClearConsole
        // walking the tree is not an edit to it. recording an undo
        // would turn undoing-the-undo into a new branch rather than a
        // redo, and the tree would fork on every change of mind.
        | Op::Undo
        | Op::Redo
        | Op::Quit => Ignored,
    }
}

/// `classify`, for the keyboard's own vocabulary.
///
/// Every mutating `Action` is restorable: none of them leaves the
/// process, because the two that would (`Star`, `Download`) are ops
/// with no `Action` of their own. `Quit` is not recorded because
/// there is nothing left to undo into, and `None` did nothing.
pub fn classify_action(action: &Action) -> Reversibility {
    match action {
        Action::None | Action::Quit => Reversibility::Ignored,
        Action::Play(_)
        | Action::Queued(_)
        | Action::ShuffleToggled(_)
        | Action::SetLoop(_)
        | Action::SetSpeed(_)
        | Action::SetEq(..) => Reversibility::Restorable,
    }
}

/// `coalesce_kind`, for actions. Held speed keys are the one run worth
/// folding; the rest are one decision per press.
pub fn coalesce_action(action: &Action) -> Option<&'static str> {
    match action {
        Action::SetSpeed(_) => Some("speed"),
        _ => None,
    }
}

/// The coalescing key for `op`, or `None` for an op that always earns
/// its own node.
///
/// Only the ops someone *adjusts* coalesce: a dragged scrubber, a
/// volume walked up, a speed nudged. Two separate `play` calls are two
/// decisions however fast they came.
pub fn coalesce_kind(op: &Op) -> Option<&'static str> {
    match op {
        Op::SetVolume(_) => Some("volume"),
        Op::SeekAbsolute(_) | Op::SeekRelative(_) => Some("seek"),
        Op::SetSpeed(_) | Op::SpeedUp | Op::SpeedDown => Some("speed"),
        // one node per settled query, not per keystroke.
        Op::Search(_) => Some("search"),
        _ => None,
    }
}

/// What `undo`/`redo` did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Moved<'a, S> {
    /// Restore this.
    To(&'a S),
    /// A barrier op is in the way; this is why it cannot be reversed.
    Refused(&'static str),
    /// Already at the oldest state, or nothing to redo.
    Nowhere,
}

struct Node<S> {
    /// State *after* this node's op; see the module doc.
    snapshot: S,
    /// `Op::describe()`, which every op already has, so the tree gets
    /// human-readable rows for free.
    label: String,
    kind: Option<&'static str>,
    at: Duration,
    parent: Option<usize>,
    children: Vec<usize>,
    /// Which child redo goes to: the one most recently walked into,
    /// not the first. That is what makes a tree feel intelligent
    /// rather than annoying.
    last_visited: Option<usize>,
    barrier: Option<&'static str>,
}

pub struct Tree<S> {
    nodes: Vec<Node<S>>,
    current: usize,
}

impl<S> Tree<S> {
    /// A tree holding only where the session started.
    pub fn new(initial: S, at: Duration) -> Self {
        Tree {
            nodes: vec![Node {
                snapshot: initial,
                label: "start".to_string(),
                kind: None,
                at,
                parent: None,
                children: Vec::new(),
                last_visited: None,
                barrier: None,
            }],
            current: 0,
        }
    }

    /// Records a state reached by an op.
    ///
    /// `snapshot` is the state *after* it applied. Adding a node while
    /// not at a leaf makes a new child and never truncates, so the
    /// branch just left stays reachable.
    pub fn record(
        &mut self,
        snapshot: S,
        label: String,
        kind: Option<&'static str>,
        barrier: Option<&'static str>,
        now: Duration,
    ) {
        if self.coalesces(kind, barrier, now) {
            let node = &mut self.nodes[self.current];
            node.snapshot = snapshot;
            node.label = label;
            node.at = now;
            return;
        }
        let id = self.nodes.len();
        self.nodes.push(Node {
            snapshot,
            label,
            kind,
            at: now,
            parent: Some(self.current),
            children: Vec::new(),
            last_visited: None,
            barrier,
        });
        let parent = &mut self.nodes[self.current];
        parent.children.push(id);
        parent.last_visited = Some(id);
        self.current = id;
    }

    /// Whether this op folds into the node already current.
    ///
    /// A barrier never coalesces: each irreversible effect is its own
    /// row, because undo has to be able to name the one it stopped at.
    /// Neither does the root, which is a state and not an op.
    fn coalesces(&self, kind: Option<&'static str>, barrier: Option<&str>, now: Duration) -> bool {
        let Some(kind) = kind else { return false };
        if barrier.is_some() || self.current == 0 {
            return false;
        }
        let node = &self.nodes[self.current];
        // only at a leaf: coalescing into a node that has been walked
        // past would silently rewrite history a redo could reach.
        node.children.is_empty()
            && node.kind == Some(kind)
            && now.saturating_sub(node.at) <= COALESCE_WINDOW
    }

    pub fn undo(&mut self) -> Moved<'_, S> {
        if let Some(why) = self.nodes[self.current].barrier {
            return Moved::Refused(why);
        }
        let Some(parent) = self.nodes[self.current].parent else {
            return Moved::Nowhere;
        };
        self.current = parent;
        Moved::To(&self.nodes[parent].snapshot)
    }

    pub fn redo(&mut self) -> Moved<'_, S> {
        let Some(child) = self.nodes[self.current].last_visited else {
            return Moved::Nowhere;
        };
        self.current = child;
        Moved::To(&self.nodes[child].snapshot)
    }

    /// The current node's siblings, in creation order, and which of
    /// them is current. The one operation that needs the tree view to
    /// be visible to be usable.
    pub fn siblings(&self) -> (Vec<usize>, usize) {
        let Some(parent) = self.nodes[self.current].parent else {
            return (vec![0], 0);
        };
        let kin = self.nodes[parent].children.clone();
        let at = kin.iter().position(|id| *id == self.current).unwrap_or(0);
        (kin, at)
    }

    /// Moves to another branch off the same parent. `delta` wraps, so
    /// a two-branch split toggles.
    pub fn switch_branch(&mut self, delta: isize) -> Moved<'_, S> {
        let (kin, at) = self.siblings();
        if kin.len() < 2 {
            return Moved::Nowhere;
        }
        let len = kin.len() as isize;
        let next = kin[(((at as isize + delta) % len) + len) as usize % kin.len()];
        self.current = next;
        // remember the branch actually walked into, so redo retraces
        // the path taken rather than the first one ever made.
        if let Some(parent) = self.nodes[next].parent {
            self.nodes[parent].last_visited = Some(next);
        }
        Moved::To(&self.nodes[next].snapshot)
    }

    pub fn current(&self) -> usize {
        self.current
    }

    pub fn len(&self) -> usize {
        self.nodes.len()
    }

    pub fn is_empty(&self) -> bool {
        // the root is always there, so a tree is never empty; spelled
        // out because clippy asks for it beside `len`.
        false
    }

    /// One row per node for the tree view: depth, label, age, whether
    /// it is current, and whether it is a barrier.
    pub fn rows(&self, now: Duration) -> Vec<Row<'_>> {
        let mut out = Vec::with_capacity(self.nodes.len());
        let path = self.path_to_current();
        self.walk(0, 0, true, now, &path, &mut out);
        out
    }

    /// Every node from the root down to the current one. The view
    /// spends brightness on this: current brightest, its ancestors
    /// mid, everything else dim, since there is no colour to spend.
    fn path_to_current(&self) -> Vec<usize> {
        let mut path = vec![self.current];
        let mut at = self.current;
        while let Some(parent) = self.nodes[at].parent {
            path.push(parent);
            at = parent;
        }
        path
    }

    fn walk<'a>(
        &'a self,
        id: usize,
        depth: usize,
        last_child: bool,
        now: Duration,
        path: &[usize],
        out: &mut Vec<Row<'a>>,
    ) {
        let node = &self.nodes[id];
        out.push(Row {
            id,
            depth,
            label: &node.label,
            age: now.saturating_sub(node.at),
            current: id == self.current,
            on_path: path.contains(&id),
            last_child,
            barrier: node.barrier,
            branches: node.children.len(),
        });
        let last = node.children.len().saturating_sub(1);
        for (i, child) in node.children.iter().enumerate() {
            self.walk(*child, depth + 1, i == last, now, path, out);
        }
    }
}

/// A node as the tree view draws it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Row<'a> {
    pub id: usize,
    pub depth: usize,
    pub label: &'a str,
    pub age: Duration,
    pub current: bool,
    /// On the root-to-current path: this node or one of its
    /// ancestors.
    pub on_path: bool,
    /// Last of its parent's children, which is what decides whether
    /// the tree glyph is a corner or a tee.
    pub last_child: bool,
    pub barrier: Option<&'static str>,
    /// How many children; more than one is a fork, which the view has
    /// to show or branch navigation is unusable.
    pub branches: usize,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(base: Duration, ms: u64) -> Duration {
        base + Duration::from_millis(ms)
    }

    fn tree() -> (Tree<&'static str>, Duration) {
        let t0 = Duration::ZERO;
        (Tree::new("start", t0), t0)
    }

    fn rec(t: &mut Tree<&'static str>, s: &'static str, now: Duration) {
        t.record(s, s.to_string(), None, None, now);
    }

    #[test]
    fn undo_restores_the_state_before_the_last_op() {
        let (mut t, t0) = tree();
        rec(&mut t, "a", at(t0, 10));
        rec(&mut t, "b", at(t0, 20));
        assert_eq!(t.undo(), Moved::To(&"a"));
        assert_eq!(t.undo(), Moved::To(&"start"));
        assert_eq!(t.undo(), Moved::Nowhere, "the root is the oldest state");
    }

    /// The reason snapshots are the state *after* an op rather than
    /// before, which is what `docs/undo.md` originally specified:
    /// with before-states there is nothing for redo to restore.
    #[test]
    fn redo_walks_back_down_the_branch() {
        let (mut t, t0) = tree();
        rec(&mut t, "a", at(t0, 10));
        rec(&mut t, "b", at(t0, 20));
        t.undo();
        t.undo();
        assert_eq!(t.redo(), Moved::To(&"a"));
        assert_eq!(t.redo(), Moved::To(&"b"));
        assert_eq!(t.redo(), Moved::Nowhere);
    }

    /// The whole point of a tree: doing something new after undoing
    /// adds a sibling and leaves the old branch reachable.
    #[test]
    fn a_new_op_after_undo_branches_instead_of_truncating() {
        let (mut t, t0) = tree();
        rec(&mut t, "a", at(t0, 10));
        rec(&mut t, "b", at(t0, 20));
        t.undo(); // back to "a"
        rec(&mut t, "c", at(t0, 30));
        assert_eq!(t.len(), 4, "b survived");

        // and "b" is still walkable: back to the fork, then across.
        assert_eq!(t.undo(), Moved::To(&"a"));
        let (kin, _) = {
            t.redo();
            t.siblings()
        };
        assert_eq!(kin.len(), 2, "the fork has both branches");
    }

    /// Redo follows the branch actually walked, not the oldest one.
    /// This is the "smart and dynamic" part: after wandering, redo
    /// retraces the path taken.
    #[test]
    fn redo_follows_the_last_visited_child_not_the_first() {
        let (mut t, t0) = tree();
        rec(&mut t, "a", at(t0, 10));
        t.undo();
        rec(&mut t, "b", at(t0, 20));
        t.undo();
        // two children of root: "a" then "b". the last walked into is
        // "b", so redo must go there.
        assert_eq!(t.redo(), Moved::To(&"b"));
    }

    #[test]
    fn switching_branch_moves_across_and_redo_then_follows_it() {
        let (mut t, t0) = tree();
        rec(&mut t, "a", at(t0, 10));
        t.undo();
        rec(&mut t, "b", at(t0, 20));
        // on "b"; across to "a"
        assert_eq!(t.switch_branch(1), Moved::To(&"a"));
        // and wrapping back
        assert_eq!(t.switch_branch(1), Moved::To(&"b"));
        assert_eq!(t.switch_branch(-1), Moved::To(&"a"));
        t.undo();
        assert_eq!(t.redo(), Moved::To(&"a"), "redo retraces the switch");
    }

    #[test]
    fn a_lone_node_has_nowhere_to_switch_to() {
        let (mut t, t0) = tree();
        rec(&mut t, "a", at(t0, 10));
        assert_eq!(t.switch_branch(1), Moved::Nowhere);
    }

    /// Undo stops at an irreversible op and says why, rather than
    /// restoring the rest of the state around it and letting the user
    /// believe it worked.
    #[test]
    fn undo_refuses_at_a_barrier_with_its_reason() {
        let (mut t, t0) = tree();
        rec(&mut t, "a", at(t0, 10));
        t.record(
            "dl",
            "download".into(),
            None,
            Some("a download wrote a file"),
            at(t0, 20),
        );
        assert_eq!(t.undo(), Moved::Refused("a download wrote a file"));
        assert_eq!(t.current(), 2, "and does not move past it");
    }

    #[test]
    fn a_run_of_the_same_adjustment_is_one_node() {
        let (mut t, t0) = tree();
        for (i, s) in ["v1", "v2", "v3"].into_iter().enumerate() {
            t.record(
                s,
                s.to_string(),
                Some("volume"),
                None,
                at(t0, 100 * i as u64),
            );
        }
        assert_eq!(t.len(), 2, "root plus one volume node");
        // coalescing keeps where the run *ended*, so redo lands after
        // it; the state before the run is the parent's, untouched.
        assert_eq!(t.undo(), Moved::To(&"start"));
        assert_eq!(t.redo(), Moved::To(&"v3"));
    }

    #[test]
    fn the_same_adjustment_after_the_window_is_its_own_node() {
        let (mut t, t0) = tree();
        t.record("v1", "v1".into(), Some("volume"), None, t0);
        t.record("v2", "v2".into(), Some("volume"), None, at(t0, 5_000));
        assert_eq!(t.len(), 3);
    }

    #[test]
    fn different_kinds_never_coalesce_however_fast() {
        let (mut t, t0) = tree();
        t.record("v", "v".into(), Some("volume"), None, t0);
        t.record("s", "s".into(), Some("seek"), None, at(t0, 1));
        assert_eq!(t.len(), 3);
    }

    /// Coalescing into a node that has already been walked past would
    /// rewrite a state redo can reach, which is a silent lie about
    /// history.
    #[test]
    fn an_adjustment_does_not_fold_into_a_node_with_children() {
        let (mut t, t0) = tree();
        t.record("v1", "v1".into(), Some("volume"), None, t0);
        rec(&mut t, "other", at(t0, 10));
        t.undo(); // back onto the volume node, which now has a child
        t.record("v2", "v2".into(), Some("volume"), None, at(t0, 20));
        assert_eq!(t.len(), 4, "a sibling, not a rewrite");
    }

    /// A barrier is always its own row: undo has to be able to name
    /// the effect it stopped at.
    #[test]
    fn barriers_never_coalesce() {
        let (mut t, t0) = tree();
        t.record(
            "d1",
            "download".into(),
            Some("volume"),
            Some("wrote a file"),
            t0,
        );
        t.record(
            "d2",
            "download".into(),
            Some("volume"),
            Some("wrote a file"),
            at(t0, 1),
        );
        assert_eq!(t.len(), 3);
    }

    #[test]
    fn rows_carry_depth_the_current_marker_and_forks() {
        let (mut t, t0) = tree();
        rec(&mut t, "a", at(t0, 10));
        t.undo();
        rec(&mut t, "b", at(t0, 20));
        let rows = t.rows(at(t0, 30));
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0].depth, 0);
        assert_eq!(rows[0].branches, 2, "the root forked");
        assert!(rows.iter().filter(|r| r.current).count() == 1);
        let current = rows.iter().find(|r| r.current).unwrap();
        assert_eq!(current.label, "b");
        assert_eq!(current.age, Duration::from_millis(10));
    }

    // ---------------------------------------------------------- ops

    #[test]
    fn the_three_classes_are_what_the_design_says() {
        assert_eq!(classify(&Op::TogglePause), Reversibility::Restorable);
        assert_eq!(classify(&Op::ClearQueue), Reversibility::Restorable);
        assert_eq!(classify(&Op::SelectTop), Reversibility::Ignored);
        assert_eq!(
            classify(&Op::View(crate::shellsong::ViewTarget::Queue)),
            Reversibility::Ignored
        );
        assert!(matches!(
            classify(&Op::Download(crate::shellsong::Target::Selection)),
            Reversibility::Barrier(_)
        ));
        assert!(matches!(classify(&Op::Rescan), Reversibility::Barrier(_)));
    }

    /// `ClearQueue` is the single most valuable thing to be able to
    /// undo, and the op most likely to be misfiled as "too big".
    #[test]
    fn clearing_the_queue_is_undoable() {
        assert_eq!(classify(&Op::ClearQueue), Reversibility::Restorable);
    }

    /// A barrier's reason is shown to the user, so it has to read as a
    /// sentence fragment rather than an identifier.
    #[test]
    fn every_barrier_explains_itself_in_words() {
        for op in [
            Op::Download(crate::shellsong::Target::Selection),
            Op::SaveQueue("x".into()),
            Op::Rescan,
            Op::OpenPage(crate::shellsong::Target::Selection),
        ] {
            let Reversibility::Barrier(why) = classify(&op) else {
                panic!("{op:?} should be a barrier");
            };
            assert!(why.len() > 8 && why.contains(' '), "{why:?}");
            assert!(
                why.chars().next().is_some_and(|c| c.is_lowercase()),
                "reasons read mid-sentence: {why:?}"
            );
        }
    }

    /// Load-bearing: recording an undo would make undoing *it* a new
    /// branch instead of a redo, and the tree would fork every time
    /// someone changed their mind.
    #[test]
    fn walking_the_tree_is_not_an_edit_to_it() {
        assert_eq!(classify(&Op::Undo), Reversibility::Ignored);
        assert_eq!(classify(&Op::Redo), Reversibility::Ignored);
    }

    #[test]
    fn only_the_adjusting_ops_coalesce() {
        assert_eq!(
            coalesce_kind(&Op::SetVolume(crate::shellsong::Level::Absolute(50.0))),
            Some("volume")
        );
        assert_eq!(coalesce_kind(&Op::SpeedUp), Some("speed"));
        assert_eq!(coalesce_kind(&Op::SeekRelative(5.0)), Some("seek"));
        // two plays are two decisions however fast they came.
        assert_eq!(coalesce_kind(&Op::TogglePause), None);
        assert_eq!(coalesce_kind(&Op::ClearQueue), None);
    }
}

/// The host's handle on the tree.
///
/// Rooted lazily: `Tree::new` wants the state the session started in,
/// and the host has no `CoreCtx` to capture one from until it is
/// already running. So the first recordable op plants the root with
/// the state *before* it, which is the same thing and costs one extra
/// capture, once.
#[derive(Default)]
pub struct Recorder {
    tree: Option<Tree<Snapshot>>,
    /// Set by `apply_op` for the length of one op.
    ///
    /// `dispatch_action` is reached two ways: straight from a keypress,
    /// which has to be recorded, and from inside twelve of `run_op`'s
    /// arms, which `apply_op` is already recording. One owner sets
    /// this and one reader honours it, which is a smaller thing to
    /// keep straight than the two independent hooks this replaced.
    suspended: bool,
}

/// What a `Recorder::undo`/`redo` produced. Owns its snapshot, because
/// restoring needs `&mut CoreCtx` and a borrow out of the tree would
/// hold `core` immutably across it.
pub enum Outcome {
    // boxed because a `Snapshot` carries the queue and dwarfs the
    // other two variants, which clippy rightly objects to for an enum
    // returned by value.
    Restored(Box<Snapshot>),
    Refused(&'static str),
    Nowhere,
}

impl Recorder {
    pub fn is_unrooted(&self) -> bool {
        self.tree.is_none()
    }

    /// Stops `dispatch_action` recording while an op it was called
    /// from is already being recorded. Returns the previous value, so
    /// the caller restores rather than assumes.
    pub fn suspend(&mut self, on: bool) -> bool {
        std::mem::replace(&mut self.suspended, on)
    }

    pub fn is_suspended(&self) -> bool {
        self.suspended
    }

    /// Seeds the tree with the state before the first recorded op.
    pub fn plant(&mut self, initial: Snapshot, now: Duration) {
        self.tree.get_or_insert_with(|| Tree::new(initial, now));
    }

    pub fn record(
        &mut self,
        snapshot: Snapshot,
        label: String,
        kind: Option<&'static str>,
        barrier: Option<&'static str>,
        now: Duration,
    ) {
        if let Some(tree) = &mut self.tree {
            tree.record(snapshot, label, kind, barrier, now);
        }
    }

    pub fn undo(&mut self) -> Outcome {
        let Some(tree) = &mut self.tree else {
            return Outcome::Nowhere;
        };
        match tree.undo() {
            Moved::To(s) => Outcome::Restored(Box::new(s.clone())),
            Moved::Refused(why) => Outcome::Refused(why),
            Moved::Nowhere => Outcome::Nowhere,
        }
    }

    pub fn redo(&mut self) -> Outcome {
        let Some(tree) = &mut self.tree else {
            return Outcome::Nowhere;
        };
        match tree.redo() {
            Moved::To(s) => Outcome::Restored(Box::new(s.clone())),
            Moved::Refused(why) => Outcome::Refused(why),
            Moved::Nowhere => Outcome::Nowhere,
        }
    }

    /// Moves to another branch off the same parent, returning what to
    /// restore. `None` when there is no fork here to cross.
    pub fn switch_branch(&mut self, delta: isize) -> Option<Snapshot> {
        let tree = self.tree.as_mut()?;
        match tree.switch_branch(delta) {
            Moved::To(s) => Some(s.clone()),
            _ => None,
        }
    }

    /// Rows for the tree view, empty until something has been
    /// recorded.
    pub fn rows(&self, now: Duration) -> Vec<Row<'_>> {
        self.tree.as_ref().map(|t| t.rows(now)).unwrap_or_default()
    }

    pub fn len(&self) -> usize {
        self.tree.as_ref().map_or(0, |t| t.len())
    }

    pub fn is_empty(&self) -> bool {
        self.tree.is_none()
    }
}

/// `Snapshot::capture`/`restore` against a real mpv.
///
/// `#[ignore]`d for the reason core.rs's seam tests are: a `CoreCtx`
/// holds a live mpv handle, so each of these spawns one (~400ms). Run
/// with `cargo test --lib restore -- --ignored`. The two invariants
/// that have a cheap source-level guard get one in `contract` below,
/// which does run in ci.
#[cfg(test)]
mod restore {
    use super::*;

    use std::collections::HashSet;
    use std::sync::mpsc;
    use std::sync::{Arc, Mutex};
    use std::thread;
    use std::time::Instant;

    use crate::cache::{self, CacheIndex};
    use crate::core::{
        BetweenTracks, Intent, RemoteSearchQuery, ScrobbleState, StarFailed, SubtitleReady,
        TrackResolver,
    };
    use crate::download::DownloadEvent;
    use crate::notify::Notifications;
    use crate::playqueue::PlayQueue;
    use crate::search::Corpus;
    use crate::shellsong::Console;
    use crate::shellsong_host::SearchSettle;
    use crate::{app, browse, config, eq_menu_keys, favorites, library, lyrics, mpv, subsonic};

    /// Everything a `CoreCtx` needs that is not the mpv handle, owned.
    ///
    /// A near-copy of core.rs's own test harness, which is private to
    /// that module's tests. Sharing it is a core.rs change; see
    /// docs/undo.md.
    struct Harness {
        cfg: config::Config,
        app: app::App,
        notifications: Notifications,
        queue: PlayQueue,
        previous_track: Option<SearchItem>,
        cache: Arc<Mutex<CacheIndex>>,
        download_tx: mpsc::Sender<DownloadEvent>,
        _download_rx: mpsc::Receiver<DownloadEvent>,
        downloading: HashSet<String>,
        scrobble: ScrobbleState,
        subtitle_tx: mpsc::Sender<SubtitleReady>,
        _subtitle_rx: mpsc::Receiver<SubtitleReady>,
        console: Console,
        now_playing: Option<SearchItem>,
        favorites: favorites::Favorites,
        remote_starred: HashSet<String>,
        lyrics_client: lyrics::Client,
        lyrics_loader: lyrics::Loader,
        track_resolver: TrackResolver,
        star_tx: mpsc::Sender<StarFailed>,
        _star_rx: mpsc::Receiver<StarFailed>,
        base_volume: f64,
        crossfade_enabled: bool,
        overridden: shellsong_host::Overridden,
        undo: Recorder,
        between_tracks: BetweenTracks,
        fade_in_started_at: Option<Instant>,
        fade: Option<shellsong_host::Ramp>,
        sleep_due: Option<Duration>,
        resume_due: Option<Duration>,
        scan_rx: mpsc::Receiver<library::ScanBatch>,
        _scan_tx: mpsc::Sender<library::ScanBatch>,
        remote_search_tx: mpsc::Sender<RemoteSearchQuery>,
        remote_search_rx: mpsc::Receiver<RemoteSearchQuery>,
        settle: SearchSettle,
        saved_queues: Vec<String>,
        vars: shellsong::Scope,
        bindings: shellsong_host::Bindings,
    }

    /// The reading every `CoreCtx` this harness builds carries, so a
    /// test can ask `settle` about the same instant the restore used.
    const NOW: Duration = Duration::from_secs(1);

    impl Harness {
        fn new() -> Harness {
            let (download_tx, _download_rx) = mpsc::channel();
            let (subtitle_tx, _subtitle_rx) = mpsc::channel();
            let (_scan_tx, scan_rx) = mpsc::channel();
            let (remote_search_tx, remote_search_rx) = mpsc::channel();
            let (star_tx, _star_rx) = mpsc::channel();
            Harness {
                cfg: config::Config::default(),
                app: app::App::new(Corpus::new()),
                notifications: Notifications::default(),
                queue: PlayQueue::default(),
                previous_track: None,
                cache: Arc::new(Mutex::new(CacheIndex::default())),
                download_tx,
                _download_rx,
                downloading: HashSet::new(),
                scrobble: ScrobbleState::default(),
                subtitle_tx,
                _subtitle_rx,
                console: Console::empty(),
                now_playing: None,
                favorites: favorites::Favorites::default(),
                remote_starred: HashSet::new(),
                lyrics_client: lyrics::Client::new(),
                lyrics_loader: lyrics::Loader::new(),
                track_resolver: TrackResolver::new(),
                star_tx,
                _star_rx,
                base_volume: 100.0,
                crossfade_enabled: false,
                overridden: shellsong_host::Overridden::default(),
                undo: Recorder::default(),
                between_tracks: BetweenTracks::new(0.0),
                fade_in_started_at: None,
                fade: None,
                sleep_due: None,
                resume_due: None,
                scan_rx,
                _scan_tx,
                remote_search_tx,
                remote_search_rx,
                settle: SearchSettle::default(),
                saved_queues: Vec::new(),
                vars: shellsong::Scope::default(),
                bindings: shellsong_host::Bindings::default(),
            }
        }

        fn ctx<'a>(&'a mut self, player: &'a mpv::Mpv) -> CoreCtx<'a> {
            CoreCtx {
                cfg: &self.cfg,
                player,
                app: &mut self.app,
                notifications: &mut self.notifications,
                queue: &mut self.queue,
                previous_track: &mut self.previous_track,
                cache: &self.cache,
                download_tx: &self.download_tx,
                downloading: &mut self.downloading,
                scrobble: &mut self.scrobble,
                subtitle_tx: &self.subtitle_tx,
                console: &mut self.console,
                now: NOW,
                now_playing: &self.now_playing,
                favorites: &mut self.favorites,
                remote_starred: &mut self.remote_starred,
                lyrics_client: &self.lyrics_client,
                lyrics_loader: &mut self.lyrics_loader,
                star_tx: &self.star_tx,
                track_resolver: &mut self.track_resolver,
                base_volume: &mut self.base_volume,
                crossfade_enabled: &mut self.crossfade_enabled,
                overridden: &mut self.overridden,
                undo: &mut self.undo,
                between_tracks: &mut self.between_tracks,
                fade_in_started_at: &mut self.fade_in_started_at,
                fade: &mut self.fade,
                sleep_due: &mut self.sleep_due,
                resume_due: &mut self.resume_due,
                scan_rx: &mut self.scan_rx,
                remote_search_tx: &self.remote_search_tx,
                settle: &mut self.settle,
                saved_queues: &mut self.saved_queues,
                vars: &mut self.vars,
                bindings: &mut self.bindings,
            }
        }

        fn capture(&mut self, player: &mpv::Mpv) -> Snapshot {
            Snapshot::capture(&self.ctx(player))
        }

        /// What `restore` itself goes through for a queue, so a test
        /// sets one up the same way rather than writing `queue`
        /// behind mpv's back.
        fn play(&mut self, player: &mpv::Mpv, items: Vec<SearchItem>) {
            let core = self.ctx(player);
            let (status, played) = load_playlist(
                core.player,
                core.cache,
                core.queue,
                items,
                SessionStart::New,
            );
            assert!(played, "the fixture queue has to load: {status}");
        }

        fn restore(&mut self, player: &mpv::Mpv, snapshot: &Snapshot) {
            let mut core = self.ctx(player);
            snapshot.restore(&mut core).expect("the restore");
        }
    }

    fn mpv() -> mpv::Mpv {
        let player = mpv::Mpv::spawn().expect("mpv on PATH");
        // these run on a desktop that may have a real wim playing;
        // never put the fixture on the speakers.
        let _ = player.set_property("ao", serde_json::json!("null"));
        player
    }

    fn id(item: &SearchItem) -> String {
        item.identity().1.into_owned()
    }

    /// An album, which is the one `SearchItem` kind whose resolution
    /// fans out off-thread; its client answers nothing, so the
    /// fan-out fails fast and the test only cares that a batch lands
    /// at all.
    fn album() -> SearchItem {
        SearchItem::Album {
            id: "al1".to_string(),
            name: "a discography".to_string(),
            artist: "a prolific artist".to_string(),
            client: browse::fixtures::test_client(),
        }
    }

    /// Blocks until the resolver hands back a batch, or `within`
    /// elapses with nothing.
    fn landed(resolver: &mut TrackResolver, within: Duration) -> bool {
        let deadline = Instant::now() + within;
        loop {
            if resolver.poll().is_some() {
                return true;
            }
            if Instant::now() >= deadline {
                return false;
            }
            thread::sleep(Duration::from_millis(5));
        }
    }

    // ------------------------------------------------- the queue half

    /// The restore branch that restarts a track used to be the one of
    /// the three that did not disown in-flight resolutions, so a
    /// prolific artist queued before the undo landed seconds after it
    /// and appended its whole discography over the restored queue.
    ///
    /// The control request is the load-bearing half of this test: it
    /// proves a batch really does land in this environment, so the
    /// `false` below means disowned rather than "nothing was ever in
    /// flight".
    #[test]
    #[ignore]
    fn a_restore_that_restarts_a_track_disowns_an_in_flight_fan_out() {
        let player = mpv();
        let mut h = Harness::new();
        let songs = browse::fixtures::test_songs(2);

        h.play(&player, vec![songs[0].clone()]);
        let snapshot = h.capture(&player);
        // something else is playing now, which is the branch that
        // reloads mpv's playlist.
        h.play(&player, vec![songs[1].clone()]);

        h.track_resolver.request(&album(), Intent::Back);
        assert!(
            landed(&mut h.track_resolver, Duration::from_secs(20)),
            "the control has to land or the assertion below proves nothing"
        );

        h.track_resolver.request(&album(), Intent::Back);
        h.restore(&player, &snapshot);
        assert_eq!(
            h.queue.current().map(id),
            Some(id(&songs[0])),
            "the restore has to have taken the restart branch"
        );
        assert!(
            !landed(&mut h.track_resolver, Duration::from_secs(2)),
            "the fan-out was resolved against the queue this restore \
             replaced; letting it land appends a whole discography on \
             top of a finished undo"
        );
    }

    /// Skip-back's target is state the ops move, so a restore that
    /// puts the track back has to put it back too; otherwise back
    /// reloads the track that is already playing.
    #[test]
    #[ignore]
    fn a_restore_puts_the_skip_back_target_back() {
        let player = mpv();
        let mut h = Harness::new();
        let songs = browse::fixtures::test_songs(3);

        h.play(&player, vec![songs[0].clone()]);
        h.previous_track = Some(songs[2].clone());
        let snapshot = h.capture(&player);

        // `next`: songs[1] plays, and the track it interrupted
        // becomes what back goes to.
        h.play(&player, vec![songs[1].clone()]);
        h.previous_track = Some(songs[0].clone());

        h.restore(&player, &snapshot);
        assert_eq!(h.queue.current().map(id), Some(id(&songs[0])));
        assert_eq!(
            h.previous_track.as_ref().map(id),
            Some(id(&songs[2])),
            "back now reloads the track that is already playing"
        );
    }

    // ------------------------------------------------ the state half

    /// `App::set_query` only re-filters what is already indexed; the
    /// contract it states is that the caller kicks the debounced
    /// remote search. Without that a restored query comes back
    /// missing every remote title while `settle` still reports
    /// settled, so the `play(selection)` after it resolves against an
    /// incomplete list.
    #[test]
    #[ignore]
    fn a_restored_query_kicks_the_debounced_remote_search() {
        let player = mpv();
        let mut h = Harness::new();
        // `notify_query_changed` sends nothing without an indexed
        // server, so there has to be one for this to mean anything.
        h.app.corpus.reconcile_server(
            cache::ServerCatalog {
                artists: vec![subsonic::Artist {
                    id: "a1".to_string(),
                    name: "bladee".to_string(),
                }],
                ..Default::default()
            },
            &browse::fixtures::test_client(),
        );

        h.app.set_query("bladee");
        let snapshot = h.capture(&player);
        h.app.set_query("drain");
        h.settle.answered();

        h.restore(&player, &snapshot);
        assert_eq!(h.app.query, "bladee", "the text comes back either way");
        assert_eq!(
            h.remote_search_rx.try_recv().map(|q| q.query).ok(),
            Some("bladee".to_string()),
            "nothing was fired, so the restored list is missing every \
             remote title"
        );
        assert!(
            !h.settle.settled(NOW),
            "a query with a request outstanding must not report settled"
        );
    }

    /// `mute()` is one `set_property` and nothing else, so mpv is the
    /// only place the state lives; a snapshot that skips it reports a
    /// successful undo over music that is still silent.
    #[test]
    #[ignore]
    fn undoing_a_mute_unmutes() {
        let player = mpv();
        let mut h = Harness::new();
        let snapshot = h.capture(&player);
        assert_eq!(
            snapshot.muted,
            Some(false),
            "mpv has to answer the property for this to be restorable"
        );

        // exactly what `Op::Mute(true)` does.
        let _ = player.set_property("mute", serde_json::json!(true));
        h.restore(&player, &snapshot);

        assert_eq!(
            player.get_property("mute").ok().and_then(|v| v.as_bool()),
            Some(false),
            "undo said it worked and left the music muted"
        );
    }

    /// The name and the modified flag are one piece of state in two
    /// fields: restoring the name alone leaves a clean curve reading
    /// "flat (modified)".
    #[test]
    #[ignore]
    fn a_restored_curve_is_not_still_modified() {
        let player = mpv();
        let mut h = Harness::new();
        let snapshot = h.capture(&player);
        let label = eq_menu_keys::eq_label(&h.app, &h.cfg);

        // a band nudge in the eq menu, which is all `eq_custom` ever
        // means.
        eq_menu_keys::adjust_eq_band(&mut h.app, &player, &mut h.notifications, 3, 1.0);
        assert_ne!(eq_menu_keys::eq_label(&h.app, &h.cfg), label);

        h.restore(&player, &snapshot);
        assert_eq!(
            eq_menu_keys::eq_label(&h.app, &h.cfg),
            label,
            "the gains came back but the curve still reads modified"
        );
        assert_eq!(h.app.eq_gains, snapshot.eq_gains);
    }
}

/// Guards for the two restore invariants whose behavioural tests need
/// a live mpv, and so are `#[ignore]`d out of ci. Reading the source
/// is the weaker check; it is also the only one that runs.
#[cfg(test)]
mod contract {
    fn body_of(name: &str) -> &'static str {
        let src = include_str!("undo.rs");
        let at = src
            .find(name)
            .unwrap_or_else(|| panic!("{name} moved or was renamed"));
        let body = &src[at..];
        let end = body.find("\n    }\n").expect("an unterminated fn");
        &body[..end]
    }

    /// One call, before the branching, because the branch that
    /// forgot its own copy appended a whole discography on top of a
    /// finished undo.
    #[test]
    fn every_restore_branch_disowns_in_flight_resolutions() {
        let body = body_of("fn restore_queue(");
        assert_eq!(
            body.matches("track_resolver.invalidate()").count(),
            1,
            "one invalidate for every branch, not one per branch:\n{body}"
        );
        let at = body
            .find("track_resolver.invalidate()")
            .expect("counted above");
        assert!(
            !body[..at].contains("if playing == wanted"),
            "the invalidate has to come before the branching:\n{body}"
        );
    }

    /// `src/app.rs` states the contract on `set_query`: the caller
    /// kicks the debounced remote search. Every other caller honours
    /// it (`core.rs`'s `Op::Search`, main.rs's two typing paths).
    #[test]
    fn a_restored_query_is_notified_like_any_other() {
        let body = body_of("pub fn restore(");
        let at = body.find("set_query(").expect("restore sets the query");
        assert!(
            body[at..].contains("notify_query_changed("),
            "set_query without the notify leaves the remote half of \
             the restored list missing:\n{body}"
        );
        assert!(
            body[at..].contains("settle.notified("),
            "and settle has to be told, or it reports a query settled \
             that nothing was fired for:\n{body}"
        );
    }
}
