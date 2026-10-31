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

use std::time::{Duration, Instant};

use crate::shellsong::Op;

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
        | Op::SkipNext
        | Op::SkipBack
        | Op::SeekAbsolute(_)
        | Op::SeekRelative(_)
        | Op::SetVolume(_)
        | Op::Mute(_)
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
        | Op::Quit => Ignored,
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
    at: Instant,
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
    pub fn new(initial: S, at: Instant) -> Self {
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
        now: Instant,
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
    fn coalesces(&self, kind: Option<&'static str>, barrier: Option<&str>, now: Instant) -> bool {
        let Some(kind) = kind else { return false };
        if barrier.is_some() || self.current == 0 {
            return false;
        }
        let node = &self.nodes[self.current];
        // only at a leaf: coalescing into a node that has been walked
        // past would silently rewrite history a redo could reach.
        node.children.is_empty()
            && node.kind == Some(kind)
            && now.saturating_duration_since(node.at) <= COALESCE_WINDOW
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
    pub fn rows(&self, now: Instant) -> Vec<Row<'_>> {
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
        now: Instant,
        path: &[usize],
        out: &mut Vec<Row<'a>>,
    ) {
        let node = &self.nodes[id];
        out.push(Row {
            id,
            depth,
            label: &node.label,
            age: now.saturating_duration_since(node.at),
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

    fn at(base: Instant, ms: u64) -> Instant {
        base + Duration::from_millis(ms)
    }

    fn tree() -> (Tree<&'static str>, Instant) {
        let t0 = Instant::now();
        (Tree::new("start", t0), t0)
    }

    fn rec(t: &mut Tree<&'static str>, s: &'static str, now: Instant) {
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
