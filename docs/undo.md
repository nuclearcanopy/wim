# undo and redo as a tree

Design for the maintainer's ask: "allow UNDOING and REDOING with nice
decision trees that should be super smart and dynamic". Not built yet;
this is the spec, written before the code because the hard parts here
are classification and the hook point, not the data structure.

A tree rather than a stack means undoing three steps and then doing
something new does not destroy the branch you left: it becomes a
sibling you can walk back to, the way vim's undo tree works. For an app
called wim that is the right shape.

## the hook point, which is not where it looked

The appeal of undo in this codebase is that mutations look centralised.
They are less centralised than they appear, and getting this wrong is
the difference between complete undo and undo that silently misses
things.

Measured against `src/main.rs` as of writing:

- `run_shellsong_op` has **40 op arms**.
- **12 of them delegate to `dispatch_action`**: `CycleLoop`, `Enqueue`,
  `Go`, `OpenPage`, `Play`, `Quit`, `SetLoop`, `SetShuffle`,
  `SetSpeed`, `SpeedDown`, `SpeedUp`, `ToggleShuffle`.
- **28 act directly** on `ShellsongCtx`, including the ones that matter
  most for undo: `ClearQueue`, `RemoveFromQueue`, `LoadQueue`,
  `SaveQueue`, `SetVolume`, `SetEqPreset`, `Star`, `Download`.

So:

- hooking **only** `dispatch_action` misses those 28, which includes
  `ClearQueue`, the single most valuable thing to be able to undo;
- hooking **only** the shellsong layer misses every keyboard action
  that does not arrive as an op;
- hooking **both** double-records the 12 that delegate.

**Re-priced when it came to wiring, and the answer changed.** Neither
of those is what shipped, because the premise was wrong: `run_op` is
**private**, and `apply_op` is the only public way in. `run_op`
recurses into itself three times (a bar field, a browse-row `go`, a
settings call) and never back out through `apply_op`, so recording in
`apply_op` is **one hook, no re-entrance guard, and complete for every
op** including the ones that delegate. Measured at the time: 44 `Op`
variants, 47 arms, 12 `dispatch_action` call sites inside `run_op`,
and only 3 in `main.rs`.

That is what `src/core.rs::apply_op` does now. The 28-arm refactor was
never needed for the shellsong path. What it *would* buy is the
keyboard path, which is the remaining gap: see "what is not recorded
yet" below.

The two options originally weighed, kept because the reasoning is
still the right shape for the keyboard half:

1. Record at the shellsong layer and in `dispatch_action`, with a
   re-entrance guard so a delegated op is recorded once. Works, and
   leaves two places to keep in sync forever.
2. **Give the 28 direct arms `Action` variants so every state mutation
   goes through `dispatch_action`.** Then undo has one choke point by
   construction rather than by vigilance, and the guarantee is
   structural. This is more work up front and it is the right answer.

Option 2 is cheapest to do **during the library split**, since that is
already moving the mutation surface into a core that does not know
about panes. If the split lands before undo, do it then.

## what is not recorded yet

Recording in `apply_op` covers **every op**, which is everything the
shellsong console and shellswim can do, and shellswim is entirely
commands. It does not cover mutations that never become an `Op`:

- the three `dispatch_action` call sites in `main.rs` (enter on a
  result, a suggestion row, a browse row),
- `activate_transport`, which acts on `Field::Loop`/`Shuffle`/`Speed`/
  `Crossfade`/`Pause` directly for a keypress or a bar click.

Those want either an `Action::describe()` plus a record at the three
call sites, or the long-planned `Action` variants for the direct arms.
The second is still the structurally better answer and is now the
*only* thing it would buy, which is a much weaker case than when it
was also going to fix the shellsong path.

## what a node holds

**Snapshot, do not invert.** The undoable state is small, so storing it
per node is cheaper to write and impossible to get subtly wrong, unlike
maintaining a correct inverse for every op.

A snapshot is:

- the play queue: entries in order, plus `pos` (order matters on its
  own, because `ToggleShuffle` reshuffles rather than just setting a
  flag),
- `paused`, volume, mute,
- loop mode, shuffle flag,
- speed,
- eq `Source` plus the live gains (both: a nudged curve is "rock,
  modified" and the name alone would lose the nudge),
- crossfade on/off,
- the search query,
- any live sleep timer or fade.

Plus, per node: the label from `Op::describe()`, which every op already
has, so the tree gets human-readable rows for free; a timestamp; the
parent; and the children in visit order.

The snapshot is the state **after** the op applied, and the root holds
the state the session started in.

**Corrected while building `src/undo.rs`.** This said *before*, which
works for undo alone: restore the state the op replaced. But then redo
has nothing to restore, because the state after an op is recorded
nowhere, and a tree whose redo cannot move is a stack with extra steps.
After-states make the two symmetric: undo restores the parent's
snapshot, redo the child's. `redo_walks_back_down_the_branch` is the
test that pins it.

## the three classes of op

This is the part that makes undo honest rather than a lie that happens
to work most of the time.

### 1. snapshot-restorable

Undo restores the snapshot and that is the whole story.

`TogglePause`, `SkipNext`, `SkipBack`, `SeekAbsolute`, `SeekRelative`,
`SetVolume`, `Mute`, `CycleLoop`, `SetLoop`, `ToggleShuffle`,
`SetShuffle`, `SetSpeed`, `SpeedUp`, `SpeedDown`, `SetEqPreset`,
`ToggleCrossfade`, `Search`, `Play`, `Enqueue`, `ClearQueue`,
`RemoveFromQueue`, `GotoQueue`, `ShuffleQueue`, `LoadQueue`, `Sleep`,
`Fade`.

### 2. irreversible side effects: undo must refuse, with a reason

These left the process. Restoring a snapshot around them restores the
*rest* of the state and leaves the effect in place, which is worse than
refusing because the user believes undo worked.

- `Star` on a remote item is a **write to the server**. An inverse call
  exists (unstar) but it is a network round trip that can fail on its
  own, so it is not a restore. Offer it as an explicit action, or
  refuse; do not fold it into a snapshot.
- `Download` wrote a file. Undo would mean deleting it, which is
  destructive in the other direction.
- `SaveQueue` wrote a file. Same.
- `OpenPage` handed a url to a browser. There is nothing to undo.
- `Rescan` rebuilt the corpus from disk and network. It is not state the
  user edited.

The rule: a node carrying one of these is a **barrier**. Undo may walk
up to it and stop, saying what it cannot reverse and why. It may not
step over it and pretend.

### 3. not recorded at all

Recording these buries the real changes under noise, which is how undo
trees become useless.

`MoveSelection`, `SelectTop`, `SelectBottom`, `View`, `ClearConsole`,
`Quit`.

Selection and view changes are navigation, not edits. (`Go` is a queue
jump and belongs in class 1, despite reading like navigation.)

### ops added since this table, now classified

`SetGap` and `Silence` are class 1: a session setting and a queue
mutation.

`Define`, `Unset`, `Remember` and `Forget` are **class 1, not
barriers**, which is a judgement worth stating. They do write a file,
which is the rule that makes `SaveQueue` a barrier. The difference is
whose artifact it is: a saved queue and a downloaded track are things
the user now has, and undo would have to delete them. Variables and
per-identity bindings are wim's own state, so restoring the scope and
writing it back leaves nothing behind. The price is that **the snapshot
has to carry vars and bindings**, and that is worth paying to be able
to undo an accidental `unset` of a variable built up over a session.

`classify` in `src/undo.rs` is an exhaustive match, so a new op cannot
be added without being filed here.

## coalescing

Without this the tree fills with noise and the feature dies.

- A run of the same adjusting op within a short window is **one node**:
  holding `+` through twelve `SpeedUp`s, dragging the scrubber, or
  walking the volume up. Coalesce on (same op kind, within N ms).

  **Keep the *latest* snapshot of the run, not the first.** This said
  first, because "that is the state the user wants back" -- which is
  right, and the conclusion was inverted by the before/after change
  above. With after-states the state before a run is the *parent's*
  snapshot, which coalescing never touches, so undo already lands
  there. The run's own node should hold where the run ended, or redo
  drops you back into the middle of it.

  Two further rules, both found by testing rather than designed:
  coalescing must not fold into a node that already has **children**
  (that rewrites a state redo can reach, which is a silent lie about
  history), and a **barrier never coalesces**, because undo has to be
  able to name the one irreversible effect it stopped at.
- Typing in the search box must not record per keystroke. One node per
  settled query, which `shellsong_host`'s `search_settled` already has
  a notion of.

## tree semantics, and the "smart and dynamic" part

- **undo** moves to the parent and restores its snapshot.
- **redo** moves to the **last-visited child**, not the first. That is
  what makes a tree feel intelligent rather than annoying: after
  undoing along a branch, redo retraces the path you actually walked.
- doing something new while not at a leaf **adds a child**; it never
  truncates. The branch you left stays reachable.
- branch navigation picks a sibling, which is the only operation that
  needs the tree view to be visible to be usable.

**Restoring a queue while music plays** needs a rule, or undo becomes a
skip: restore entries and position, and **do not restart the current
track if it survives the restore**. Only reload mpv's playlist when the
restored queue's current entry differs from what is playing.

## the tree view

This is the part most likely to ship barebones, so it gets the ux
self-check in ROADMAP.md explicitly.

Minimum to be worth opening:

- one row per node, labelled with `Op::describe()` and a relative time
  ("2m ago"), the current node marked unambiguously,
- branch structure drawn with the box-drawing glyphs the rest of the ui
  already uses, not ascii pipes,
- barrier nodes (class 2) marked as such, so "why can I not go further"
  is answerable from the screen rather than from a notification,
- the monochrome rule applies: brightness carries the hierarchy
  (current node brightest, ancestors mid, other branches dim), since
  there is no colour to spend.

## the payoff that ties this to the rest

A branch of this tree is a list of ops in order, which is to say **a
`.shg` script**. Once scripts exist, "export this branch" is close to
free, and an undo tree becomes a way to *capture* a session you stumbled
into and replay or share it. That is the feature the maintainer and I
both want out of this, and it is worth keeping the node data in a shape
that can serialise to a script line rather than only to a snapshot.
