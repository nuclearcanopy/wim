# shellsong

a command language for wim. `:` opens a console in the middle of the
screen; you type calls into it.

```
: play(crest)
: queue([hex, 4 star crest, sulfur surfer])
: repeat(3, skip())
: dry(queue_aa([crest]))
: moveto(1m20s)
: show(queue)
```

implemented in `src/shellsong.rs` (language, pure) and
`src/ui/shellsong.rs` (the overlay, pure render).

## consistency rules

these are load-bearing. everything below follows from them, and nothing
should be added that breaks one.

- **r1. everything is a call.** no prefixes, no suffixes that carry
  behaviour, no bare words. repetition is `repeat(n, call)`; a dry run
  is `dry(call)`.
- **r2. a bare number means exactly one thing per parameter.**
  `speed(1.5)` is a multiplier, `volume(50%)` a level, `moveto(10s)` a
  position. `skip(2)` is refused: it is ambiguous between "skip twice"
  and "skip to track 2", so repetition is `repeat(2, skip())`.
- **r3. canonical names are snake_case full words.** aliases are
  additive and never canonical; `help()` lists canonical names with
  aliases in a trailing column.
- **r4. `[]` is always a list of plain unquoted strings** split on
  commas. items may contain spaces, so `[4 star crest]` is one item.
- **r5. a single value is accepted anywhere a list is.**
  `favorite(flatline)` is `favorite([flatline])`. this is a property of
  the parameter type, not per-command special casing.
- **r6. a name suffix is sugar for a keyword argument.**
  `queue_aa([x])` *is* `queue([x], kind=album)`, and `dry()` prints the
  desugared form so you can check.
- **r7. kinds are a preference order, not a filter.** see below.
- **r8. signed time is relative, unsigned is absolute**, enforced in
  both directions. `seek(30s)` and `moveto(+30s)` are both errors that
  tell you which one you meant.
- **r9. nothing is silent.** every call reports one line into the
  scrollback, including no-ops.
- **r10. one name, one meaning.** no arity overloading; introspection
  lives in `show()` rather than in a bare call to something else.

## grammar

```
program    := statement (separator statement)*
separator  := ';' | newline
statement  := call
call       := name '(' args? ')'
args       := arg (',' arg)*
arg        := call | list | keyword '=' value | value
list       := '[' item (',' item)* ','? ']'
item       := unquoted-text | '"' text '"'
comment    := '#' ... end of line
```

- whitespace around anything is ignored, except inside a list item.
- a call may span lines; splitting is nesting-aware.
- a trailing comma is allowed in a list and in an argument list.
- quotes are only needed for an item containing a literal comma.
- `q` is deliberately not an alias for anything: it is ambiguous
  between `queue` and `quit`, so neither claims it.

## parameter types

each command declares its parameters by type, and the parser validates
against that. this is what tells `front` from a search term in
`add_to_queue(front, flatline)`, and it is where the arity and type
error messages come from.

| type | accepts |
| --- | --- |
| `terms` | a term, or `[a, b, c]` (r4, r5) |
| `name` | free text: a query, a playlist name, a command name |
| `pos` | an unsigned time literal (absolute position) |
| `offset` | a signed time literal (relative) |
| `level` | a percent, absolute (`50%`) or signed (`+10%`) |
| `count` | a whole number, 1 to 256 |
| `index` | a 1-based queue position |
| `indices` | one or more queue positions |
| `speed` | a multiplier 0.25-3, or `up` / `down` / `reset` |
| `duration` | a time literal, or `off` |
| `call` | a nested call |
| keyword sets | one of an enumerated list (`front|back`, `one|all|off`, ...) |

a parameter marked `?` in a signature is optional. when you pass
exactly as many arguments as there are parameters they match strictly
in order; when you pass fewer, an optional parameter whose value does
not typecheck is skipped, which is how `add_to_queue(flatline)`
defaults its position to `back`.

## time literals

```
10%        percent of the track, 0-100, fractional allowed (12.5%)
10s        seconds
1m20s      80 seconds
2m  1h  1h2m3s
90         a bare number is seconds
1:20       80 seconds
1:02:03    3723 seconds
```

units must run `h`, `m`, `s` in that order and may not repeat, so
`1m2h` is an error rather than silently reordered. `10x` is an error.
a percent outside 0-100 is an error.

a percent stays a percent: the track duration is runtime state, so the
host resolves it when the op is applied.

sign carries relative-vs-absolute (r8):

```
moveto(30s)    seek to 0:30
seek(+30s)     30 seconds forward
seek(-10s)     10 seconds back
seek(30s)      error: write seek(+30s) or moveto(30s)
moveto(+30s)   error: moveto is absolute
seek(+50%)     error: relative seek takes a time, not a percent
```

## kinds and ambiguity

searching "crest" can match a song *and* an album. a kind preference
order decides which wins: a term resolves to the best-scoring result of
the most-preferred kind that is actually present. this is ambiguity
resolution, not filtering (r7).

| command | preference |
| --- | --- |
| `play` | song, album |
| `queue` | song |
| `queue_aa` | song, album |
| `queue_ar` | song, album, artist |
| `star`, `unstar`, `download` | song, album |

`play` being wider than `queue` is deliberate and intentional: playing
an album is an ordinary thing to want, building a *queue* out of albums
is not. do not "fix" the asymmetry.

artists never expand by default. a `queue_ar` queueing an artist hands
the whole artist to the host's existing expansion, which is almost
never what you meant; it exists for completeness, not for daily use.

an album accepted by `_aa` expands to its tracks, the same way enter on
an album does.

### suffixes and `kind=`

suffixes are sugar (r6) for the `kind=` ladder. each rung only widens
the one before it.

| suffix | means |
| --- | --- |
| `_aa` | `kind=album` ("allow album") |
| `_ar` | `kind=artist` |
| `_any` | `kind=any` (also playlists and local video) |

```
: dry(queue_aa([crest]))
would run: queue(crest, kind=album)
```

`kind=` is refused on a command that does not search.

### a term that matches nothing

reported, and the rest of the list continues; one bad term never aborts
a queue build. a multi-term call also reports a tally at the end.

```
: queue([hex, xyzzy, crest])
no song match for 'xyzzy'
queued 2 of 3
```

## commands

`help()` prints this table from the same data the parser reads, so the
two cannot drift. aliases in brackets.

### transport

```
pause()                 play / pause                    [toggle, play_pause]
skip()                  next track                      [next, forward, fwd]
back()                  previous track                  [prev, previous]
moveto(pos)             seek to an absolute position    [move]
seek(offset)            seek by a signed offset
volume(level)           set or nudge the volume, clamped to 100%   [vol]
mute()                  mute
unmute()                unmute
```

volume above 100 is digital gain in mpv and clips, so it is clamped;
ask for `volume(100%)` if you want the ceiling.

### modes

```
loop(mode?)             cycle loop mode, or set one/all/off   [repeat_mode]
shuffle(state?)         toggle shuffle mode, or set on/off    [random, randomize, shuf]
shuffle_queue()         reorder the queue now, without touching shuffle mode
speed(speed)            1.5, or up / down / reset             [tempo]
eq(preset?)             open the eq menu, or apply a preset   [equalizer]
crossfade()             toggle crossfade                      [xfade]
```

`shuffle()` and `shuffle_queue()` are deliberately separate names: one
is the mode, one is an immediate reorder, and conflating them is
exactly the confusion two names avoid.

### search and playback

```
search(query)           run a search and leave the results up  [find, grep]
play(terms)             play the first term, queue the rest
queue(terms)            append each term to the queue          [enqueue]
add_to_queue(position?, terms)   front inserts next, back appends (default)
clear_queue()           empty the queue                        [clear, flush]
remove(indices)         drop queue entries by position
goto(index)             jump to a queue position
save_queue(name)        save the queue as a local playlist
load_queue(name)        replace the queue with a saved playlist
star(terms?)            star the selection, or what each term finds  [favorite, fav, like]
unstar(terms?)          the inverse                            [unfavorite, unfav, unlike]
download(terms?)        download for offline playback          [dl, offline, cache]
```

the zero-argument forms of `star`, `unstar` and `download` act on
whatever is currently highlighted, matching the `*` and `o` keybinds.

### selection

```
go()                    act on the current selection, same as enter  [enter, open, select]
up()                    move the selection up
down()                  move the selection down
top()                   select the first result
bottom()                select the last result
```

`go()` is no longer a composition primitive (`play()` does the whole
search-and-play in one call), but it is still how you drive the ui from
the console.

### introspection and overlays

```
show(target)            queue / now / history / index / playlists   [ls]
lyrics()                open lyrics for what is playing
starred()               open the starred songs view
settings()              open the settings menu
keybinds()              open the keybind menu
rescan()                re-walk the local library
sleep(after)            fade out and pause after a delay; sleep(off) cancels
fade(direction, over?)  fade the volume in or out
help(name?)             list every command, or one command's signature
quit()                  quit wim                                    [exit]
```

`sleep` reuses the crossfade volume ramp rather than cutting the audio
dead; remaining time shows in `show(now)`.

### higher-order

```
repeat(count, call)     run a call n times
dry(call)               report what a call would do, running nothing  [explain]
```

`dry` runs nothing at all, including the searches: it prints the plan
from the real lowered program, which is also how you check a suffix's
desugaring. they nest: `dry(repeat(2, seek(+10s)))`.

## the console editor

- `:` opens it, `esc` closes it, `enter` on an empty line closes it.
- `enter` submits. a parse error keeps the line so you can fix it, and
  prints the column and the offending token.
- an unknown name suggests the closest known one.
- `up` / `down` recall history, persisted to
  `$XDG_STATE_HOME/wim/shellsong_history`. the half-typed line comes
  back when you walk forward past the newest entry.
- `tab` completes: command names at statement level, and the enclosing
  parameter's keyword set inside a call (`eq(ro<tab>` becomes
  `eq(rock)`). a unique command name gets its parens too.
- `home` / `end` / `left` / `right` / `delete` do the obvious things.

### auto-pairing

- typing `(` or `[` inserts the pair and parks the cursor inside.
- typing `)` or `]` when that character is already under the cursor
  steps over it instead of doubling it.
- backspace on an empty pair deletes both characters.
- none of this applies inside a quoted string, where brackets are
  literal text.
- a bracketed paste goes in literally, with no pairing.

## non-goals

deliberately not built, so they do not creep in later:

- variables, conditionals, loops, user-defined functions, pipelines.

this is a command language for a music player, not a general-purpose
shell. statement sequencing plus `repeat` and `dry` covers the real
use cases; anything more is scope that will rot.

## how it runs

search is asynchronous: local corpus hits are immediate but remote
song results arrive later over a channel, behind a debounce. a command
that searches therefore cannot be evaluated in one pass, so `Program`
is a resumable state machine: it hands the host one `Op` at a time and
blocks on `AwaitResults` until the host says the results for that
query have settled. a search that never settles times out with an
error rather than hanging.

every `Op` is applied by the host through the same call its keybind
already uses, so a shellsong command and a keypress can never drift
apart. `Env` is the only thing the executor knows about the world,
which is what makes the whole language testable with no terminal, no
mpv and no network.
