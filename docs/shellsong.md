# shellsong

a command language for wim. `:` opens a console in the middle of the
screen; you type calls into it.

```
: play(crest)
: queue([hex, 4 star crest, sulfur surfer])
: repeat(3, skip())
: dry(queue_aa([crest]))
: moveto(1m20s)
: view(q, bladee)
: view(queue)
```

implemented in `src/shellsong.rs` (language and completion, pure) and
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
- **r10. one name, one meaning.** no arity overloading, and no second
  name for something a first name already does. every screen is a
  `view` target rather than its own verb.
- **r11. omitting the terms means the current selection.** uniformly,
  for every command that takes them: `star()` stars what is
  highlighted, `star(flatline)` stars what that finds. one meaning
  ("operate on terms, defaulting to the selection"), one code path.
- **r12. completion is driven by the declared parameter type.** a
  parameter either has a candidate list worth inserting or it has none
  and gets a hint. nothing is ever guessed.

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
- `q` is deliberately not an alias for anything at statement level: it
  is ambiguous between `queue` and `quit`, so neither claims it. it
  *is* the keyword that switches `view` into its search form, where
  there is nothing to be ambiguous with.

## parameter types

each command declares its parameters by type, and the parser validates
against that. this is what tells `front` from a search term in
`add_to_queue(front, flatline)`, where the arity and type error
messages come from, and (r12) what completion offers.

| type | accepts | completes from |
| --- | --- | --- |
| `terms` | a term, or `[a, b, c]` (r4, r5) | the local corpus |
| `query` | free text to search for | the local corpus |
| `new_name` | a name you are inventing | nothing; hint only |
| `saved_name` | the name of a saved queue | the saved queues |
| `command_name` | a command name | the command table |
| `call` | a nested call | the command table |
| keyword sets | one of an enumerated list | that list |
| `pos` | an unsigned time literal (absolute) | nothing; hint only |
| `offset` | a signed time literal (relative) | nothing; hint only |
| `level` | a percent, absolute or signed | nothing; hint only |
| `count` | a whole number, 1 to 256 | nothing; hint only |
| `index` | a 1-based queue position | nothing; hint only |
| `indices` | one or more queue positions | nothing; hint only |
| `speed` | a multiplier 0.25-3, or `up` / `down` / `reset` | nothing; hint only |
| `duration` | a time literal, or `off` | nothing; hint only |

a parameter marked `?` in a signature is optional. when you pass
exactly as many arguments as there are parameters they match strictly
in order; when you pass fewer, an optional parameter whose value does
not typecheck is skipped, which is how `star(flatline)` gets its term
into the right slot.

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

## volume

```
volume(50%)    set it to 50
volume(50)     the same; a bare number is a percent
volume(+10%)   10 points louder than it is now
volume(-10%)   10 points quieter
```

an absolute level above 100 is refused at parse time, and a nudge is
applied to mpv's current volume and then clamped to 0-100. the ceiling
is real rather than conservative: above 100 mpv applies digital gain,
which clips. ask for `volume(100%)` if you want the top.

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
| `star`, `unstar`, `download` | song, album |
| `view(q, ...)` | artist, album, playlist, song |

`play` being wider than `queue` is deliberate: playing an album is an
ordinary thing to want, building a *queue* out of albums is not. do not
"fix" the asymmetry.

`view` is wider still and in a different order, because it opens pages
and an artist is the thing that most has one. being a different order
rather than a wider one is also why `kind=` and the suffixes do not
apply to it: the ladder below widens *from songs*, and `view` does not
start there.

an album accepted by `_aa` expands to its tracks, the same way enter on
an album does.

### suffixes and `kind=`

suffixes are sugar (r6) for the `kind=` ladder. each rung only widens
the one before it.

| suffix | means | accepts |
| --- | --- | --- |
| none | `kind=song` | songs (remote and local music) |
| `_aa` | `kind=album` | songs, albums ("allow album") |
| `_any` | `kind=any` | songs, albums, artists, playlists, local video |

```
: dry(queue_aa([crest]))
would run: queue(crest, kind=album)
```

there is deliberately **no artist rung** (no `_ar`, no `kind=artist`).
an artist expands to an entire discography, which is never a queue
anyone asked for. `kind=any` remains as the explicit "i really do mean
anything" escape hatch, and it is spelled that way on purpose.

`kind=` is refused on a command that does not search, and on `view`.

### a term that matches nothing

reported, and the rest of the list continues; one bad term never aborts
a queue build. a multi-term call also reports a tally at the end.

```
: queue([hex, xyzzy, crest])
no song match for 'xyzzy'
queued 2 of 3
```

## view

`view` is the one and only screen-opening command. every menu, page and
listing is a target of it rather than its own verb (r10). `show` and
`ls` are aliases of it, not separate commands.

```
view(target)       put a screen or listing up
view(q, terms)     search, and open the best match's page
```

### targets

| target | what it opens | also accepts |
| --- | --- | --- |
| `queue` | the queue, listed | |
| `now` | the full player screen | `player` |
| `suggestions` | the suggestions menu | `suggestion`, `suggest` |
| `settings` | the settings menu | |
| `keybinds` | the keybind menu | `keybind` |
| `lyrics` | lyrics for what is playing | |
| `starred` | the starred songs view | `favorites` |
| `index` | the corpus breakdown, listed | |
| `history` | past shellsong commands, listed | |
| `playlists` | saved queues, listed | |
| `eq` | the eq menu | `equalizer` |
| `help` | the command listing | |

an unknown target is matched against this list and suggests the closest
real one; `view()` with nothing at all lists them.

the screens take over the display and close the console behind them;
the five listings (`queue`, `index`, `history`, `playlists`, `help`)
print into the scrollback and leave it open. `view` means "put this on
screen" either way, and a listing satisfies that; there is deliberately
no second verb for the distinction.

`help` is the one target whose lines the language generates itself,
from the same command table `help()` reads, so `view(help)` and
`help()` can never print different things. it is still an ordinary
target otherwise: every spelling in the table lowers to one
`Op::View(target)` and the host prints it, so no target is reachable
only through the console's scrollback and no host has an arm for an op
that cannot arrive.

### the query form

```
: view(q, bladee)
: view(query, 4 star crest)
```

searches, then opens the best match's page: an artist or album opens
its own view. **a song opens the page it belongs to rather than
playing**, because the verb is `view`, not `play`. that is intentional;
`play(hex)` is how you play something.

the query form searches, so it goes through the same await/resume path
`play` and `queue` do. `q` requires terms (there is no "view the
selection" reading of "search for this"), which is the one documented
exception to r11.

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
volume(level)           set or nudge the volume         [vol]
mute()                  mute
unmute()                unmute
```

### modes

```
loop(mode?)             cycle loop mode, or set one/all/off   [repeat_mode]
shuffle(state?)         toggle shuffle mode, or set on/off    [random, randomize, shuf]
shuffle_queue()         reorder the queue now, without touching shuffle mode
speed(speed)            1.5, or up / down / reset             [tempo]
eq(preset)              apply a preset; view(eq) opens the menu  [equalizer]
crossfade()             toggle crossfade                      [xfade]
```

`shuffle()` and `shuffle_queue()` are deliberately separate names: one
is the mode, one is an immediate reorder, and conflating them is
exactly the confusion two names avoid.

### search and playback

every command here takes its terms optionally; without them it acts on
whatever is highlighted (r11).

```
search(query)           run a search and leave the results up  [find, grep]
play(terms?)            play the first term, queue the rest
queue(terms?)           append each term to the queue          [enqueue]
add_to_queue(position, terms?)   front inserts next, back appends
clear_queue()           drop everything upcoming, keep playing  [clear, flush]
remove(indices)         drop queue entries by position
goto(index)             jump to a queue position
save_queue(name)        save the queue as a local playlist
load_queue(name)        replace the queue with a saved playlist, and play it
star(terms?)            star what each term finds              [favorite, fav, like]
unstar(terms?)          the inverse                            [unfavorite, unfav, unlike]
download(terms?)        download for offline playback          [dl, offline, cache]
```

`add_to_queue`'s position is **required**. with it optional,
`add_to_queue(back, x)` was just another spelling of `queue(x)`, which
r10 does not allow; `front` is the capability `queue` genuinely lacks,
so that is all the command is for. `add_to_queue(flatline)` is an error
pointing at `queue(flatline)` or at a position.

### selection

```
go()                    act on the current selection, same as enter  [enter, open, select]
up()                    move the selection up
down()                  move the selection down
top()                   select the first result
bottom()                select the last result
```

`go()` means "what enter does", and what enter does depends on which
pane owns the keyboard, so it resolves against the focus: a focused
browse view claims it, then the bottom bar's focused field, otherwise
the selected search result. that mirrors the app's own enter precedence
exactly, which is the point.

### everything else

```
view(target)            see above                              [show, ls]
rescan()                re-walk the local library
sleep(after)            fade out and pause after a delay; sleep(off) cancels
fade(direction, over?)  fade the volume in or out
help(name?)             list every command, or one command's signature
quit()                  quit wim                               [exit]
```

`sleep` reuses the crossfade volume ramp rather than cutting the audio
dead: the last `crossfade_seconds` of the wait fade the volume down,
then it pauses and puts the level back so unpausing later is not
silent. A toast says so when it fires; there is no countdown on the
player screen yet.

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
- `home` / `end` / `left` / `right` / `delete` do the obvious things,
  except that `right` at the end of the line accepts a suggestion.

### auto-pairing

- typing `(` or `[` inserts the pair and parks the cursor inside.
- typing `)` or `]` when that character is already under the cursor
  steps over it instead of doubling it.
- backspace on an empty pair deletes both characters.
- none of this applies inside a quoted string, where brackets are
  literal text.
- a bracketed paste goes in literally, with no pairing.

## completion

### the one rule

what a parameter completes to is decided by its declared type (r12),
nothing else. types fall into two classes and are treated completely
differently:

- **enumerable** types have a candidate list: command names, corpus
  names, a keyword set, the saved queues. these produce an insertable
  suggestion.
- **value** types have no sane candidate list, so they produce a
  **hint** instead, which is never insertable and which no key
  accepts. completing `volume(5` to `50%` is absurd and so it does not
  happen.

the hints:

```
volume(     0-100, or +N% / -N%
seek(       +30s / -10s (sign required)
moveto(     10% / 90 / 1m20s
speed(      0.25-3.0, or up / down / reset
sleep(      30m / off
repeat(     how many times, 1-256
goto(       a queue position, 1 or higher
remove(     1, or [1, 3, 5]
save_queue( a name to save it under
```

alongside the hint, the signature of the call you are inside shows with
the active parameter picked out: `volume(<level>)`. a new parameter type
has to declare candidates or a hint; declaring neither fails a test
rather than silently completing nothing.

the `up` / `down` / `reset` and `off` keywords live in those hint
strings rather than being insertable candidates. that is the price of
one simple rule, and it is a small one: the parameter is fundamentally
a value, and the keywords are short enough to type.

### never overwriting what you typed

the corpus search is fuzzy, so subsequence and typo matches surface
things a prefix scan would miss. candidates are then filtered to the
ones your typed token is a case-insensitive prefix of, because
inserting anything else would have to replace characters you typed.

a fuzzy hit that is not a prefix extension is excluded from both the
ghost and the tab cycle. accepting appends only the candidate's
remaining characters, so your prefix survives byte for byte including
its casing: `fl` plus `atline`, never `Flatline`. search is
case-insensitive, so the result still resolves.

the token being completed is the argument slice back to the last `(`,
`[`, `,` or `;`, not the last word, so an item with spaces still
completes: `queue([4 star cre` completes `4 star crest`, and
`queue([a, fl` completes `fl` rather than the whole list.

### two sources, and which wins

1. **a whole past command line**, the way zsh-autosuggestions does it:
   if everything you have typed is a prefix of a line you have run, the
   rest of that line is ghosted. only with the cursor at the end of the
   input, where the rest of the line is genuinely what comes next.
2. **the token under the cursor**, from the type-driven candidates
   above.

the ghost shows the history suggestion when there is one, otherwise the
best token candidate. **tab always cycles token candidates**, never the
history line; the two are reached by different keys on purpose.

### keys

| key | does |
| --- | --- |
| `tab` | open the candidate menu, then step forward through it |
| `shift+tab` | step backward |
| `right` | accept, but only at the end of the line |
| `ctrl+f` | accept, wherever the cursor is |
| typing, `esc` | cancel an open menu |

with exactly one candidate, `tab` just takes it. while cycling, the
candidates are listed under the input with the current one in brackets
and a `2/3` position indicator, so cycling is not blind. accepting a
command name adds its parens and lands the cursor inside them, so `qu`
plus `right` gives you `queue()` ready for arguments.

### learning from use

running a line and accepting a completion are both recorded in a small
frequency/recency store next to the history file
(`$XDG_STATE_HOME/wim/shellsong_ranks`; its own file, since history is
a plain line list and this is a scored map). candidates are ordered by
frequency, with recency breaking frequency ties.

anything unranked keeps the order it arrived in, which matters: that is
relevance order for corpus hits and alphabetical for names, both better
than a reshuffle. the sort is stable, so the whole thing is
deterministic.

### performance

completion runs on every keystroke, so it uses the **local corpus
only** and never triggers a remote search; typing would otherwise spam
the server. candidates are capped at 50 and the existing optimised
`Corpus::search` does the scoring rather than a second scorer. the host
supplies the corpus behind a small provider trait, which is also what
lets the completer be tested with no corpus, no network and no
terminal.

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

## the host side

the wiring lives in three places. `main.rs` owns the console state, the
input tier that claims every key and every paste while it is open, the
per-frame `Program::step` loop and `apply_shellsong_op`, which is the
whole `Op` -> host table and sits next to `dispatch_action`.
`shellsong_host.rs` holds the host decisions worth having apart from
the event loop (all pure, all tested): the completion provider, the
`search_settled` state machine, percent-to-seconds resolution, the
volume clamp and the fade/sleep ramps. `queues.rs` is the named-queue
store behind `save_queue`/`load_queue`.

### the api a host calls

the whole surface, copied from the real signatures and pinned by a
test that calls every one of these forms, so the list cannot drift
without failing the build.

```rust
const OPEN_KEY: KeyCode                 // ':', also reserved in keymap

Console::new() -> Console               // loads history + ranks; the only i/o
Console::handle_key(&mut self, key: KeyEvent, cand: &dyn Candidates)
    -> ConsoleAction                    // None | Close | Run(Box<Program>)
Console::paste(&mut self, text: &str, cand: &dyn Candidates)
Console::note(&mut self, text: impl Into<String>)
Console::fail(&mut self, text: impl Into<String>)
Console::push(&mut self, kind: LineKind, text: impl Into<String>)
Console::history(&self) -> &[String]
Console::cycle(&self) -> Option<&Cycle>
// read by the overlay: input, cursor (byte index), lines, error,
// suggestion.

Program::parse(input: &str) -> Result<Program, ParseError>
Program::step(&mut self, env: &Env) -> Step
Program::is_done(&self) -> bool

Op::describe(&self) -> String           // the one scrollback line (r9)
Op::closes_console(&self) -> bool
ViewTarget::is_screen(self) -> bool
Kind::of(item: &SearchItem) -> Kind      // for Env::results
help_lines() -> Vec<String>              // what view(help) prints

trait Candidates {
    fn corpus(&self, token: &str) -> Vec<String>;
    fn saved_queues(&self) -> Vec<String> { Vec::new() }
}
```

both `handle_key` and `paste` take the candidate provider because both
edit the line and therefore have to recompute the suggestion; a paste
that left a stale ghost up would be the same bug as a keystroke that
did.

### the index contract

`Target::Index(i)` indexes `Env::results`, which the host builds 1:1
off `App::results`. that list is not stable: `App::resync_results`
reassigns it wholesale (`corpus.search(&query)`), so rows are added,
dropped and regrouped, and it runs on every remote search outcome, on
every library scan batch and on every catalog merge. `Op::Search` and
`Op::Rescan` rebuild it too, which means a resolved index can be
invalidated by **the very next op of the same program**, not merely by
the next frame.

a resolved index is nevertheless safe here, and worth understanding
before changing either side:

- the executor never stores one. `resolve` computes the index and
  returns it inside the same `Step::Emit`; no `Node` and no `Program`
  field holds a `Target`.
- the host applies each `Emit` immediately, rebuilding `Env` per step
  rather than per frame, and drains no channel inside that loop.
- every term-taking handler reads its item out of the list before
  touching anything, and the only two ops that rebuild the list
  (`Search`, `Rescan`) carry no `Target`.
- it is all one thread; the channels carry data, not mutation.

so a host that **batched** emitted ops and applied them after a drain
would act on the wrong row, intermittently and only under load. one
route is already closed off: `Resolve` refuses to resolve until it sees
its own query in `Env` (`env.query != pending.query || !search_settled`
keeps it on `AwaitResults`), so a host that deferred `Op::Search` fails
loudly on the await timeout instead of resolving against stale results.

nothing enforces this at the type level. making it structural means
giving `Env::results` an identity per row (a hash of
`SearchItem::identity()`, since `SearchItem` cannot be `PartialEq`) and
having `Target::Index` carry that stamp for the host to verify. that
touches `Env`, the env builder and every term-op handler, and the
cheaper half-measure (asserting the list's *length* is unchanged) is
blind to the equal-length regroup that is `resync_results`'s normal
outcome, so the contract is written down here instead.

## pinned behaviours

these were all ambiguous until the host was written, and a reader (or a
future host) would have had to guess. they are decisions, not accidents:

- `clear_queue()` drops everything upcoming and **keeps the current
  track playing**, matching mpv's own `playlist-clear`. it is not a
  stop.
- `load_queue(name)` **starts playing**, consistent with what
  `play(...)` on an album does. it does not merely stage the queue.
- `goto(index)` and `remove(indices)` are 1-based positions in the
  **whole playlist, including already-played entries**, because that is
  what the queue view numbers on screen and therefore what a user reads
  off it. `Env::queue_len` matches.
- `view(now)` can only unminimize the player; the host has no separate
  "player screen" state, so it fails when nothing is playing.
- `view(q, ...)` landing on a song can only open a page for an album
  already indexed, since `SearchItem::Song` carries its album's name
  and not an id. local files have no page.
- `go()` on the bottom bar's progress field refuses: enter there is a
  keyboard scrub drag, and an open console claims every key, so the
  drag could never receive them. use `moveto(...)` or `seek(...)`.

### routing

anything with an `Action` behind it goes through `dispatch_action`, so
it cannot behave differently from the keypress: `play`, `queue`,
`view(q, ...)`, `loop`, `shuffle`, `speed`, `eq`, `quit`. everything
else goes through the same function the matching keybind calls
(`go_to_previous`, `toggle_star`, `trigger_downloads`,
`resync_upcoming`, `apply_eq_preset_row`, `open_starred_view`), or the
same mpv property/command the keybind writes.

the two shared decisions that used to be inline in the input loop are
now functions both the keyboard and shellsong call: `browse_session`
(which tracks enter on a browse row plays) and `App::move_selection` /
`select_top` / `select_bottom`.

### search_settled

a query is unsettled only because a remote `search3` actually went out
for it. `notify_query_changed` reports whether it sent anything, so
with no indexed server there is nothing to wait for and the query
settles the instant it changes. otherwise the first outcome off the
channel settles it, and a 1.2s window settles it regardless, which is
past the 300ms debounce plus a round trip and well inside the
language's own 8s await timeout: a slow server costs `play(crest)` a
beat rather than an error.

first-answer-wins rather than one outcome per server is deliberate.
outcomes carry no query tag and a failed server sends nothing at all,
so there is no honest count to wait for; a second server's late answer
is still merged into the corpus, it just does not hold the program up.

### not supported yet

reported through the console rather than faked, because a truthful
error beats a silent no-op:

- `go()` with the bottom bar's **progress** field selected. enter
  there opens a keyboard scrub drag, and the open console claims every
  key the drag needs. it points at `moveto`/`seek` instead, which do
  the same job without a mode.
- `view(q, ...)` landing on a **song** whose album is not indexed.
  `OpenPage` opens an album/artist/playlist directly, and a song's own
  page is its album's; a `SearchItem::Song` carries its album's *name*
  but not its id, so the page can only be opened for an album already
  in the corpus. a local file has no page at all.
- `sleep(...)` has no remaining-time readout on the player screen.

### where things are stored

```
$XDG_STATE_HOME/wim/shellsong_history   the command history
$XDG_STATE_HOME/wim/shellsong_ranks     the completion frequency store
$XDG_STATE_HOME/wim/queues/<name>.toml  one saved queue each
```

a saved queue stores more than a path per entry: a remote song keeps
its server url plus the server-side song id and is re-bound to a live
client on load, since a stream url carries credentials and expires. an
entry whose file moved or whose server is no longer configured is
skipped, same best-effort stance `persistence.rs` takes.
