# shellswim

wim with nothing but the prompt. a full-screen shellsong console, the
playing track and its scrubber, and no other chrome at all; everything
is a command.

it exists for two reasons. the obvious one is that the maintainer
wanted it. the less obvious one is that it is the proof the library
seam is in the right place: if the op table can drive a front-end with
no panes in it, the line in `docs/library-seam.md` is drawn correctly.

## the two shapes

`shellswim` with no arguments is the prompt. `shellswim -c '...'` and
`shellswim path.shg` are the same thing from a different source: a
list of lines, each run through exactly the parse a typed line gets.

the one-shot path **never touches raw mode or the alternate screen.**
it is a command, so its answers go to stdout and its failures to
stderr, and it exits non-zero if any line failed. the first version
drew a frame and then exited, which left the alternate screen and
wiped the line it had just written; a command whose output you cannot
read is not a command.

that choice has a second payoff worth stating: **the one-shot path is
testable with no pty at all.** every behavioural claim below was
checked by running it.

```
$ shellswim -c 'dry(play(crest))'
would run: play(crest)
  search 'crest'
  played the best song/album match for 'crest'

$ shellswim -c 'view(eq)'; echo $?
shellswim has no equaliser pane; wim's 'e' has it
1
```

## what it will not do, and how it says so

shellswim has no panes, so five `view(...)` targets have nowhere to
go. it declines them, by name, pointing at the key in wim that opens
them. it does not no-op and it does not pretend: `view(eq)` doing
nothing and `view(eq)` saying where the eq lives are different
answers, and only one of them is true.

| | |
| --- | --- |
| printed in the core, identically in both | `view(queue)`, `view(index)`, `view(history)`, `view(playlists)`, `view(help)` |
| printed by shellswim instead of a pane | `view(now)`, `view(lyrics)` |
| declined, naming wim | `view(eq)`, `view(settings)`, `view(keybinds)`, `view(starred)`, `view(suggestions)` |
| declined, naming a command that works | `up()`/`down()`/`top()`/`bottom()`, `go()` on a browse row, opening an album's page |

the first row is not a shellswim decision: `ViewTarget::is_screen()`
already partitioned the language exactly that way, and the seam just
started honouring it.

`dry(...)` still describes **every** op here, including the ones
shellswim would refuse, because `dry` lowers to notes and never
reaches the op table at all. that is the intended reading: `dry`
answers "what does this mean", not "what would happen here".

`play()` with no term means the head of the current result set, since
there is no highlight to read. that is where wim's highlight starts,
so `search(x)` then `play()` means the same thing in both.

## what makes it feel like a shell and not like python3

- **nothing blocks.** the corpus loads on a thread and the prompt is
  usable before it answers; a command that has to resolve a name
  suspends on `AwaitResults` and resumes on a later frame. the whole
  of `Host::pump` is `try_recv`.
- **the prompt is the last row, always.** an overlay grows downward
  from a centred origin; a shell grows upward from the prompt. output
  arriving must not move the thing you are typing into.
- **scrollback reads as a transcript.** a command echoes with the `:`
  it was typed after, so the history above the prompt is the session,
  not a log.
- **completion is the language's,** untouched: ghost text for the part
  that can be inserted, and a separate row for a value that cannot be
  guessed. `volume(5` never suggests `50%` as if it were insertable.
- **one line per command,** declines included (docs/shellsong.md R9).
- **the bar is live and free.** the scrubber is sampled at the
  display's refresh rate while something plays and at `anim::IDLE_POLL`
  when nothing does.

## the graphics probe is not run, and that is a feature

wim queries the terminal over stdio for graphics support. shellswim
draws no images, so it never asks. that is worth a paragraph because
the probe reads stdin for up to a second on a terminal that will never
answer, racing the event loop for input; not running it makes the
event loop the only reader of stdin.

## measured startup

`WIM_STARTUP_TRACE=1`, warm cache, one local path, isolated XDG dirs,
on a machine under load from three other builds.

interactive, under a pty:

```
    1.621ms  config loaded
    4.014ms  library scan: finished
  405.422ms  mpv spawned
  415.214ms  corpus warmed from cache
  419.153ms  terminal ready
  419.992ms  first frame drawn
  426.717ms  refresh rate probed
```

one-shot (`-c`), which touches no terminal at all: 392.7 / 429.6 /
395.0ms to host-ready over three runs.

wim, same machine, for comparison:

```
    1.545ms  config loaded
  339.978ms  mpv spawned
  361.260ms  terminal ready
  364.435ms  first frame drawn
 1361.831ms  graphics probe answered
```

two things to read off that. first, **`Mpv::spawn()` is ~400ms of
both numbers** and is the only blocking call left in `Host::start`;
everything shellswim itself does costs about 4ms, and the frame after
mpv answers costs another 1ms. the honest statement is that shellswim
starts in single-digit milliseconds and then waits for mpv. making the
prompt usable *before* mpv answers is the obvious next move and is not
done.

second, wim's graphics probe answers at 1.36s, a full second after its
own first frame, and for that whole second it is a second reader of
stdin. shellswim has no such line at all.

## layout

```
+--------------------------------------------------+
| : play(crest)                                    |  scrollback, oldest at the top
| playing: BLADEE - FLATLINE (3 queued behind it)   |
| 41 commands                                      |
| ...                                              |
| > volume(<level>)   0-100, or +N% / -N%           |  the hint row, when there is one
| : volume(5|                                      |  the prompt, always the last row
| BLADEE - FLATLINE                            +3  |  the track, and what is behind it
| =4 > ↻ ⇄ × ▂▅▃ ◢◣  1:04 ●─────────────  3:21     |  the transport strip
+--------------------------------------------------+
```

## what needs a human at a real terminal

the interactive loop cannot be driven headlessly: raw mode needs a tty
this build does not have. everything above the event loop is covered
by the buffer tests in `src/swimconsole.rs` and by the one-shot path,
but these specifically have not been seen working:

- tab/shift+tab cycling, and `right`/`ctrl+f` accepting a ghost
- up/down history recall
- bracketed paste
- the scrubber actually moving, and the idle poll costing nothing
- `?` on an empty line
- ctrl+c and ctrl+d leaving cleanly, and the terminal being restored
