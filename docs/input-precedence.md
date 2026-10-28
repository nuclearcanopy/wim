# input precedence

two input sources, one ordering. the keyboard's cascade lives inline in
`main`'s `Event::Key` block; the mouse's lives in `hit::act`. this is
the agreement between them.

## the tier order

`hit::TIERS` is the order, written down once:

1. onboarding
2. nav manager
3. help overlay
4. shellsong console
5. the sidebar's query box (`app.editing_query`)
6. the browse view's song filter (`bv.filtering`)
7. subtitle menu
8. settings menu
9. eq menu
10. keybinds menu

the keyboard checks these in exactly this order, each tier ending in a
`continue` so nothing below it ever sees the key. the two text boxes
(5, 6) sit where they do deliberately: the long comment above
`if app.editing_query` in main.rs is the record of `?`/`t`/`u` leaking
into a query being typed, twice, because a new handler got inserted
ahead of them.

the mouse does not re-derive that order. `main` builds one
`hit::Tiers` per frame, from the same locals the cascade tests, right
before the draw; `hit::front` picks the frontmost by the list above and
stores it on the frame's `HitMap`. adding a tier means adding a
variant, which fails to compile until `TIERS`, `Tiers`,
`Modal::covers_screen` and the per-frame literal all name it.

## why a click can reach a text box's pane and a key cannot

`Modal::covers_screen` splits the ten into two kinds.

the eight that are painted over the panes behind them swallow a click
outright. a click under one would be a click on something the user
cannot see, so it resolves to `Act::Swallowed` and stops. this includes
the three menus (settings, keybinds, eq) that only cover `top` and
leave the bottom bar drawn underneath: the keyboard gives them every
key while they are up, so the mouse gives them every cell.

the two text boxes do not swallow. a keystroke has no location, so a
text box has to claim every key or lose some of them to whatever else
is listening; a click names its target and cannot leak that way.
clicking another pane while typing therefore reaches that pane, and
takes the keyboard out of the box on the way, which is what clicking
away from a field does anywhere else. clicking *into* a box
(`Act::Edit`) puts the keyboard in it, which is the other half of the
same convention and the reason the boxes have their own hit rects at
all: without them a click on a visible search box would be a click on
the pane around it, i.e. the exact opposite of what it looks like.

## the hit map

every rect in this app is computed inside `guard.terminal.draw`, so the
input handler cannot locate anything. rather than recompute the layout
outside the closure, the draw fills in a `hit::HitMap` as it goes and
the next mouse event resolves against that one. a frame's map therefore
describes the frame the user was looking at when they clicked, which is
the only frame the click was aimed at.

the widgets hand over their own geometry rather than being guessed at:

- `controlsbar::ControlsBar::layout` and `ui::player::PlayerView::transport`
  return a rect per control plus the scrubber's track cells, from the
  same placement pass the render paints from. the fit rule
  (`segments_that_fit`) drops whole trailing controls on a narrow bar
  and the player centres its row on its own total width, so neither set
  of columns is derivable from the `Field` list.
- `results_list::row_items`, `browse::rows`, `queue::row_items` and
  `suggestions::row_items` return the item index per visible row, from
  the same layout call and the same `ui::scroll::offset_for` the render
  used. headers, gaps and rows past the end come back as `None`, so a
  click cannot move a selection onto a row that is not one.

`hit::act` is pure given a map, which is the point: raw-mode input
cannot be driven headlessly, so every decision lives below the event
plumbing and is tested without a terminal.

## what the mouse does

- a click in a pane focuses it. a click on a row focuses the pane and
  puts the selection on that row. a click in a text box focuses the box.
- a click on a transport control runs `activate_transport`, the same
  function enter on it runs, after moving that surface's cursor onto
  it. there is no second dispatch.
- a click on the scrubber's track opens a drag at that fraction of the
  duration and seeks there; dragging scrubs, releasing commits. all of
  it through the existing `controlsbar::Scrub` and the 80ms seek
  coalescer, so a drag costs the same ipc as a held arrow key.
- the wheel scrolls the list under the pointer, not the focused one,
  and does not take focus.

## the wheel moves the selection, not a viewport

a notch is `hit::WHEEL_ROWS` rows of selection movement, which makes
one notch the same operation as one `j`. the alternative, an
independent viewport offset, is more mouse-like but means the selection
can leave the screen, and it is a second piece of scroll state for the
keyboard to disagree with: these lists have none today, they derive the
offset from the selection every frame via `ui::scroll::offset_for`,
which is also what keeps a wheeled selection on screen for free.

the lyrics pane is the exception, and not an inconsistent one: it has
no selection at all, `j`/`k` there move a content offset
(`lyrics_scroll`), so the wheel moves the same offset. the rule is
"the wheel does what `j`/`k` do on this surface", and that is what it
resolves to in both cases.

## mouse capture costs the terminal's text selection

with `mouse = true` (the default) the terminal hands click-drag to the
app, so selecting and copying text out of it the normal way stops
working. most terminals let you hold shift to bypass capture;
`mouse = false` gives selection back unconditionally. capture is also
restored on panic, not only on `Drop`: left on after a crash, the shell
prints escape gibberish on every pointer move until it is reset by
hand.

`TerminalGuard` additionally turns any-event tracking (`?1003`) back
off after crossterm's `EnableMouseCapture` turns it on. it reports
every pointer motion across the terminal, which would cost a full
redraw per event whether or not the app was doing anything. `?1002`
stays on, which reports motion only while a button is held, i.e. the
drags the scrubber needs.
