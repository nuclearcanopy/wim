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
equalizer: on · flat
   31 Hz  +0.0 dB
   ...
presets: flat, bass boost, vocal boost, treble boost, rock, classical
choose a curve with eq(name) · E opens the editor
0
```

## what each view does here

shellswim has no general pane system, so every `view(...)` target prints
its content as lines (scripts and `-c` get exactly that). At the
interactive prompt the targets that have a workspace also open it. A
target that can do neither declines by name rather than no-op, because
"did nothing" and "says why" are different answers.

| | |
| --- | --- |
| printed in the core, identically in both | `view(queue)`, `view(index)`, `view(history)`, `view(playlists)`, `view(help)` |
| dedicated interactive view / printed by one-shot | `view(lyrics)` |
| printed by shellswim / opens its editor interactively | `view(keybinds)` |
| printed settings snapshot, then opens the editor interactively | `view(settings)` |
| printed equalizer snapshot, then opens the editor interactively | `view(eq)` |
| printed undo history, then opens the tree interactively | `view(undo)` |
| printed, personalized recommendations | `view(suggestions)` |
| printed starred items from the loaded library | `view(starred)` |
| printed by shellswim | `view(now)` |
| declined, naming wim | — |
| opens the tracks as a playable list | opening an album, artist or playlist page |
| declined, naming a command that works | `up()`/`down()`/`top()`/`bottom()`, `go()` on a browse row |

the first row is not a shellswim decision: `ViewTarget::is_screen()`
already partitioned the language exactly that way, and the seam just
started honouring it.

In the interactive prompt, `view(lyrics)` opens a live lyrics split view
beside the command console.
Synchronized lyrics follow playback; `j`/`k` or arrows pause following
for three seconds before it resumes. `esc` closes the lyrics side and
returns the console to full width.
First launch offers the same provider choices and optional Musixmatch
key-file path as wim's setup. `shellswim -c 'view(lyrics)'` remains a
plain-text, non-interactive command suitable for scripts.

When a video starts, Shellswim starts the configured OpenSubtitles
lookup in the background; downloaded subtitles are added only if that
same video is still playing. Audio and setups without an OpenSubtitles
key do not trigger a lookup.
Press the subtitle-menu key (default `t`, shown as `T` here because a
lowercase letter on an empty prompt starts a command like `top`) to cycle tracks,
toggle visibility, and adjust the seven subtitle-style fields. Changes
apply immediately and save automatically.
Press `K` for the keybind editor: `j`/`k` select an action, `r` captures
a replacement, `0` resets one action, and `R` resets all bindings.
Reserved or already-used keys are refused rather than saved as dead
bindings.
`view(starred)` and `view(suggestions)` print their rows and, at the
prompt, open them as a list: `j`/`k` move, `enter` plays the row, `a`
queues it, `esc` or `q` closes.
At the prompt `view(undo)` also opens the undo tree: `u`/`k` undo,
`r`/`j` redo, `h`/`l` cross a fork, `esc` closes.
Shellswim routes network streams through the same address-pinning media
relay as wim and warns if it cannot start. Press `W` on an empty prompt
for Watch (needs `WIM_STREMIO_ADDON_URL`, as in wim): `/` search, `enter`
opens/advances, `[`/`]` seasons, `a` adds a torrent after confirmation,
`backspace` goes back, `esc` closes; sources play only through the relay.
The Navidrome profile key (default `n`, shown as `N`) opens the server
manager: `enter` reconnects or adds, `d` twice removes, and new
profiles are verified before they are saved.
On a first launch with no library and no server, the server form opens
right after the lyrics choice.
Press `T` for TorBox (setup, search, account lists for torrents, usenet
and web downloads, file playback with `enter`, queueing with `q`; keys
match wim's workspace).
Press `U` (default `u`, uppercased like the subtitle key) or run
`view(settings)` for the settings editor: `j`/`k` select, `h`/`l`
adjust, `enter` edits the ListenBrainz token or confirms an offline
cache rebuild. Album art and TorBox setup are listed as unavailable
here. `esc` closes it and saves.
Press `E` (or run `view(eq)`) for the equalizer editor: `j`/`k` move,
`enter` applies a preset, `h`/`l` nudge a band, and the same save,
rename and delete keys as wim manage custom curves. `esc` closes it
and persists the curve.

The prompt also has an optional queue workspace: press the default `Q`
key on an empty prompt to open it. `j`/`k` or arrows select tracks,
`enter` jumps playback to the selected track, `d` removes it, and `J`/`K`
reorders upcoming tracks. `esc` or `q` returns to the prompt. The
`view(queue)` command remains a printable listing for scripts and
scrollback.

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
- **submitted commands are never replaced by a running command.** New
  input waits FIFO behind the active program and track-bound settings;
  the host continues driving those settings even when the prompt is idle.
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

The measurements below are historical. Startup now observes mpv and loads
the playback cache in the background, with the prompt accepting input while
both are pending. See `docs/performance.md` for the newer PTY measurements.

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

## what wim has that shellswim does not

intentional, so it stays a decision and not drift:

| wim | shellswim |
| --- | --- |
| panes, banner, album art | one console and the scrubber |
| mouse (click, drag the scrubber, wheel) | keyboard only |
| resize mode, fullscreen keys (`F`, F11) | none; `video()` hands off to mpv |
| help overlay with every key (`?`) | `help()` and `?` list commands |
| onboarding for TorBox, ListenBrainz and library paths | first run offers lyrics and the server form; the rest are settings or config |
| `K` keybinding editor, `u` settings, EQ and undo as panes | `view(keybinds)`, `view(settings)`, `view(eq)`, `view(undo)` open the same editors |
| notifications as toasts, `view(messages)` for the history | notifications are printed into the scrollback; `view(messages)` lists the history |

shellswim's subtitle, navidrome and settings keys (`T`, `N`, `U`) differ
from wim's single-letter keys on purpose: the prompt owns the plain
letters.
