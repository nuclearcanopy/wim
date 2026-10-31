# text measurement

how wide a string is, and what the unit is. read this before touching
`ui/textfit.rs` or `ui/draw.rs`.

## the unit is a grapheme cluster, not a char

a terminal puts one *user-perceived character* in a cell. that is a
grapheme cluster (uax #29), and it is not a codepoint:

| string | bytes | chars | clusters | cells |
| --- | --- | --- | --- | --- |
| `Beyoncé` nfd (`e` + u+0301) | 9 | 8 | 7 | 7 |
| `❤️` (u+2764 + u+fe0f) | 6 | 2 | 1 | 2 |
| `1️⃣` (`1` + u+fe0f + u+20e3) | 7 | 3 | 1 | 2 |
| `👩‍👩‍👧` (3 faces, 2 zwj) | 18 | 5 | 1 | 2 |
| `각` decomposed (jamo l+v+t) | 9 | 3 | 1 | 2 |
| `日` | 3 | 1 | 1 | 2 |

none of this is exotic content. nfd is how macos/apfs stores filenames,
so any library imported from a mac arrives decomposed, and emoji turn up
in playlist names constantly.

## the bug this replaced

`textfit` had two width functions implementing two different models:

- `width` called `UnicodeWidthStr::width`, which is string-level and
  applies the emoji-presentation and jamo rules: `❤️` is 2, a zwj
  family is 2.
- `char_cells` called `UnicodeWidthChar::width`, which cannot see those
  rules: `❤️` is 1 (the variation selector is 0), a zwj family is 6
  (three faces at 2 each, the joiners at 0).

every truncation and every drawing loop mixed them: measure with one,
step the cursor with the other. on top of that the drawing loops did
`cursor += cells.max(1)`, which hands a **zero-width codepoint a cell of
its own**. measured in a real `Buffer`, with a `│` at column 8 and a
budget of 8 cells:

- `❤️ love song` returned x=9 and overwrote the border
- `1️⃣ favourites` returned x=10
- `👩‍👩‍👧 mix` returned x=10, the three faces in separate cells
- nfd `Beyoncé` returned x=8 for a 7-cell string
- decomposed `각 song` returned x=9, painted as three loose jamo

a zero-width cell is not skipped by `Buffer::diff`, so the terminal
re-joins the cluster anyway and everything after it lands one column
left of where the buffer thinks it is. the documented contract "the
result is never wider than asked for" was simply false:
`truncate_with_ellipsis("❤️ love song", 8)` really painted 9 cells, and
`truncate_to_width("👩‍👩‍👧 mix", 8)` really painted 4 while measuring
8, because the zwj components were double-counted against the budget.

worst case: a pasted run of combining marks with no base is one
zero-width cluster per paste and hundreds of codepoints. at a cell each
that drove `col: u16` past 65535 and wrapped, since `[profile.release]`
sets no `overflow-checks`.

## the model now

`textfit::clusters` + `textfit::cluster_cells` are the only measuring
pair. `width` is the sum of the clusters' widths, so what gets measured
is exactly what `draw::at` advances by. every fit drops zero-width
clusters: a cluster with no cell cannot be given one.

`draw::at_styled` writes a whole cluster with one `set_symbol` and
advances by `cluster_cells`, with no `.max(1)`. a wide cluster still
blanks the cell after it (`Buffer::diff` skips a blanked trailing cell,
which is why cjk never shifted even before this).

`char_cells` stays, because `ui/controlsbar.rs` and
`ui/lyrics_view.rs` legitimately measure text they built themselves out
of ascii and known-single-char glyphs. it is the wrong tool for anything
that came from a tag.

## what else was downstream of it

three writers hand-rolled the same per-char loop against `width`-derived
budgets and now go through `draw::at`:
`swimconsole::draw_str`, `ui::shellsong::text`, and
`ui::draw::at_styled` itself. `ui/controlsbar.rs` has a fourth copy
(`controlsbar.rs` ~715) that is still per-char; it only ever draws
glyphs it owns, but it is the next one to route.

## the ghost guard, both halves of it

`ui::shellsong` drew the ghost completion at `cursor_col + 1`
unconditionally, over cells the real text had just been written into:
typing `queue(fl, hexed` and pressing Left seven times rendered
`: queue(fl█atline)` with `, hexed` gone from the screen while still in
the buffer.

`swimconsole` looked like the in-repo example of the right thing, with
`cursor_col != width(&window)`. it is not: the language auto-closes `(`,
so `queue(fl` is the 9-cell window `queue(fl)` with the caret at 8, the
guard never held, and **shellswim never drew a ghost at all** for any
parenthesised call, which is nearly all of them. measured, not inferred;
`textfit::eyeball::ghost_rows` prints both rows.

the rule both use now is the one that is actually load-bearing: draw the
ghost only where it cannot land on a cell the real text occupies, i.e.
`cursor_col + 1 >= width(&shown)`. a caret sitting before an auto-closed
`)` still qualifies, because the ghost starts at the end of the row
either way.

## cost, and the two fast paths

a grapheme walk costs ~180ns a cluster in this profile (the crate
builds at `opt-level = 0` while its dependencies build at 2), and the
layout decisions call `width` many times a frame. segmenting
everything regressed the per-frame text path about 5x and made it the
dominant cost in the app, so there are two gates in front of the walk:

- `plain_ascii`: printable ascii. one byte, one char, one cluster, one
  cell, so `width` is `len()` and a fit is a byte slice. `is_ascii` is
  a word-at-a-time library check and most strings stop there.
- `one_cell_chars`: every char is exactly one cell. then a char is a
  cell, so `width` is a char count and `draw::at` can put a char in a
  cell. almost every joiner is ruled out by the same check (a
  combining mark, zwj, variation selector or jamo medial is zero cells;
  a jamo initial, cjk glyph or skin-tone modifier is two). the two that
  slip through, a flag's regional indicators and an indic matra, join
  into a cluster of the same total width, so the measurement still
  matches what gets drawn.

the fits cannot slice on that second gate, because a char boundary
there can land *inside* a cluster and cut a flag in half (a test caught
exactly that). they ask the question of the two chars at the cut
instead, on a stack buffer, and fall back to the walk when it says they
join.

alternated A/B of `ui::player::bench::frame_phases` at 200x60, HEAD's
`textfit`/`draw` against these, minima of three rounds on a loaded
machine:

| phase | HEAD | now |
| --- | --- | --- |
| text (frame minus art) | 70.18us | 68.18us |
| draw_row | 11.57us | 11.67us |
| controls | 3.47us | 4.22us |
| hint | 3.24us | 2.76us |
| scrubber | 2.93us | 2.90us |
| context | 1.12us | 1.09us |
| transport | 17.49us | 19.53us |
| whole frame | 491.2us | 491.2us |

what still costs more than HEAD is text HEAD measured *wrongly*: an nfd
title is ~9x, because it has to be segmented and HEAD's answer for it
was the bug. `textfit::bench` compares the three models in one process
(`cargo test textfit::bench -- --ignored --nocapture`); alternating
batches rather than running them back to back is not optional here,
sequential runs on this box came out 1.6x apart.

## still per-char

`shellsong::Console`'s cursor is a byte offset moved by *chars*, so Left
from the end of nfd `é` parks between the base and the mark. the render
is correct either way (the cluster is still drawn in one cell), but the
caret column is ambiguous for one keypress. fixing it means moving the
editor to cluster boundaries, which is editing behaviour, not
measurement.

## the test that holds the contract

`textfit::eyeball` renders every one of these strings into a real
`Buffer` and asserts the painted width never exceeds the budget, for
`truncate_to_width`, `truncate_with_ellipsis` and `tail_fit` at every
budget from 0 to 14. it also prints the grids:
`cargo test eyeball -- --nocapture`. measuring `width()` against itself
is not the claim; the claim is about cells.
