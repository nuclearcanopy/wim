# Performance

Performance numbers are only useful with the workload and environment attached.
The automated process harness measures starting Wim with an empty local
library and no servers, then sitting idle without playback. A separate,
operator-assisted suite compares client resource use with a shared Navidrome
library. Neither measures end-to-end search or time to audible playback.

## Run the process benchmark

Requires Linux with `/proc`, Python 3, mpv on `PATH`, and a built Wim binary.
Build the same release profile for each comparison; use the actual binary, not
a shell wrapper. Run on an otherwise quiet machine with a fixed power profile:

```sh
nix develop --command cargo build --release --locked --bin wim
python3 benchmarks/bench.py run --binary target/release/wim --runs 10 --seconds 5 --output /tmp/wim-results.json
python3 benchmarks/bench.py report /tmp/wim-results.json
```

Each run has a fresh temporary home, configuration, empty library, and PTY.
It inherits the machine's environment and system settings; it does not clear
the operating-system page cache. The output includes individual samples, the
binary hash and version, Git state, CPU, kernel, mpv version, and parameters.
Record the build command, power profile and other relevant machine details
alongside any published results. A dirty Git tree means the commit alone does
not identify the build. Never publish results from a different binary as if
they came from that commit.

* **First terminal output** is elapsed time from process spawn to the first
  bytes read from the PTY. It is not time to a complete or interactive frame.
* After a two-second settling period, **idle RSS** is the median of sampled,
  summed resident set sizes for Wim and its live descendants (including mpv).
  RSS can double-count shared pages; it is not private memory or system-wide
  memory. It excludes the terminal and audio server.
* **Idle CPU** is the change in live process-tree CPU ticks over the measurement
  interval, expressed as a percentage of one core. Very short-lived child
  processes may be missed. No audio is playing.

The table reports the nearest-rank median and p95 across independent runs;
the p95 of ten runs is the maximum. These numbers vary with hardware, load,
configuration and cache state. The PTY is drained but does not render text, so
it excludes the cost of a real terminal emulator. Keep raw JSON with any
published table and re-run after relevant changes; do not compare samples
collected under different conditions as if they were controlled experiments.

## Comparing Navidrome clients

`benchmarks/compare.py` measures idle or steady-state playback process-tree
RSS and CPU for Wim, Feishin desktop, and Supersonic. It generates a 180-second
PCM tone and keeps separate, persistent client profiles. It randomizes the run
order and stores individual samples and a Markdown report. Each run requires
the operator to navigate the UI and confirm the requested state; it does not
automate or independently verify playback. These are not startup or latency
measurements, and results should be labeled **operator-verified**, not fully
automated. The client workload excludes the Navidrome server, which must stay
the same throughout the comparison.

Use an isolated machine/session, not your personal server or profiles. This
example starts a fresh Navidrome instance bound only to loopback; use a free
port and a disposable account. Its data and the client profiles may contain
credentials: keep the workspace private and do not publish it.

```sh
python3 benchmarks/compare.py init /tmp/wim-comparison
mkdir -p /tmp/wim-comparison/server
ND_ENABLEEXTERNALSERVICES=false navidrome --address 127.0.0.1 --port 4533 \
  --musicfolder /tmp/wim-comparison/music \
  --datafolder /tmp/wim-comparison/server
```

Create a disposable admin account at `http://127.0.0.1:4533`, wait for the
library scan, and leave the server running. In a separate terminal, start the
comparison. The programs and `xterm` must be on `PATH`; build Wim first. Pass
the actual versions of the clients and server being tested:

```sh
python3 benchmarks/compare.py run /tmp/wim-comparison \
  --wim "$PWD/target/release/wim" --scenario playback \
  --server-version VERSION --feishin-version VERSION --supersonic-version VERSION \
  --runs 5 --seconds 10 --output /tmp/wim-comparison-playback.json
python3 benchmarks/compare.py report /tmp/wim-comparison-playback.json
```

At each prompt, connect the client to the fixture server (profiles persist
between runs), select the generated track from that server and start playback.
For `--scenario idle`, stop playback and leave the library open. Confirm only
after the same state is reached; any nonempty response aborts without writing
results. Use the same audio device, codec, volume, server, power profile and
network conditions; disable optional features consistently where possible and
record settings that cannot be made equivalent. Feishin's mpv backend is the
intended comparison. Do not use the local file in Wim for the playback scenario.

RSS sums live processes and double-counts shared pages. Wim's total includes
the xterm process and mpv; Feishin's includes its Electron and mpv children;
Supersonic uses libmpv in-process. The display server and audio server are not
included. CPU is percent of one core. Record machine, client configuration,
audio output, terminal, server version and library details with any published
results; the version arguments are supplied by the operator and not verified
by the harness. A GUI smoke test is available separately:

```sh
xvfb-run -a env WIM_REAL_GUI_SMOKE=1 WIM_BINARY="$PWD/target/release/wim" \
  python3 -m unittest discover -s benchmarks -p test_compare.py -v
```

That smoke test checks that all three clients start in isolated profiles; it
does not test connectivity or playback. The comparison runner does not yet
measure the built-in Navidrome web client.

## Search and future comparisons

The existing ignored search benchmarks exercise the in-memory search algorithm,
not end-to-end typing or a Navidrome server. See
[search-performance.md](search-performance.md) for the corpus, commands and
correctness guards.

A meaningful Navidrome web-client comparison needs a pinned server and client,
the same generated media and catalog, controlled network and cache conditions,
automated equivalent actions, and a defined observable endpoint for each task.
Browser and terminal processes, mpv, and audio services must be accounted for
consistently. Measure search latency and playback-to-audible-output separately;
standalone mpv can provide a playback baseline. Include absolute values and
unfavorable results, not just a best-case ratio. Those latency comparisons are
not implemented yet.

The intended comparison set is Navidrome's bundled web client in a dedicated
browser profile, [Feishin desktop](https://github.com/jeffvli/feishin) with
its mpv backend, and [Supersonic](https://github.com/supersonic-app/supersonic),
which uses libmpv. These represent a browser, an Electron desktop client and
a native desktop client respectively; they have different features and are
not interchangeable. Pin exact versions and browser/backend settings. Report
server cost separately, use a dedicated browser instance, and include each
client's complete process tree. Standalone mpv is a decoder baseline, not a
full Navidrome-client comparison.

## Earlier performance record

The following is the previous performance design record. Its observations are
historical and scoped to the operations described, not the new process benchmark.

### Product budgets and measured work

Wim's performance goal is not "a fast tui" in isolation. It is a player
that feels immediate, keeps audio reliable, and spends CPU, memory, and
network only where the feature warrants it. Compare complete process
trees against mpv on identical media and conditions; a fast widget
benchmark is not evidence that playback is efficient.

#### Measurement rules

- Use a release build and record machine, mpv version, terminal, media,
  configuration, and cold/warm state.
- Measure p50/p95 and worst-observed interaction latency; report CPU and
  RSS for Wim and child processes separately and together.
- Record network requests/bytes for streaming, including the proxy and
  every decoder. Never use real credentials or expose signed URLs in
  logs, argv, or benchmark output.
- Use generated local media and a controlled range-capable server before
  any real service. Real TorBox playback is a separate manual result.
- For microbenchmarks, alternate both implementations in one process,
  repeat, and compare the best of each; wall timings across unrelated
  runs on this host vary substantially (`docs/testing.md`).

#### First baseline observations

These are useful signals, **not yet a complete end-to-end comparison**.
Measured in an earlier worktree on mpv v0.41.0 with a release build:

| Operation | Observation |
| --- | ---: |
| Blocking `get_property` | 76 µs mean, 741 µs worst in the existing 300-read probe |
| Folded observed playback level read | 0.045 µs |
| Seven sequential subtitle property updates | 285 µs per group, best of 5 alternating rounds |
| Seven pipelined subtitle property updates | 188 µs per group, best of 5 alternating rounds |

The property observation path demonstrates why render code should consume
pushed state rather than poll mpv. Batching is also a measurable win,
though these values alone do not establish that Wim is faster than mpv.
An earlier non-alternating probe read 1,166/370 µs; the alternating
benchmark produced lower values and is the one retained as the baseline.

Prior measured work is recorded in `docs/startup-cache.md`,
`docs/search-performance.md`, `docs/video.md`, and
`docs/architecture.md`. Those measurements are scoped to their stated
operations; they should not be mistaken for aggregate CPU, input latency,
or a complete user-experience comparison.

#### Workload and metrics to keep

Run the same generated media and isolated XDG state for:

1. cold and warm launch through first usable frame;
2. keypress-to-visible-response while idle, searching, scanning, and
   playing;
3. play-to-first-audio, pause/resume, seek, and queue advance;
4. audio-only local and throttled remote playback;
5. local and remote video, including seek and resize;
6. paused/idle CPU, active CPU, process-tree RSS, redraw rate, terminal
   bytes, and network bytes per stream.

Use mpv with equivalent output and cache settings as the playback
baseline. Keep startup, steady-state, and feature incremental costs
separate. After repeated runs establish numeric budgets from the actual
hardware rather than choosing attractive but unverified targets.

#### Work completed in the first performance pass

Startup queue restoration used one synchronous mpv IPC command per
saved item after the first screen had already appeared. Unshuffled
restoration now resolves paths locally, sends ordered `append-play`
commands as one IPC batch, and updates the mirrored queue only for
successful replies. Shuffle remains on the existing per-item path
because each insertion may need a corresponding `playlist-move` to keep
the two playlists aligned. This is a latency reduction for large saved
queues, not a claim that all startup work is now asynchronous.

The batch method retains a result per path so partial failure cannot
silently desynchronize mpv's playlist and Wim's queue. Further work must
measure the end-to-end startup and input benefit, then cover the async
and shuffle paths without weakening that invariant.

#### Next investigations, ordered by risk and impact

1. **UI-thread stalls:** instrument loop gaps and attribute them to
   synchronous IPC, filesystem, config writes, drawing, or event drains;
   move only measured long work off the loop with cancellation and
   bounded queues.
2. **Idle work:** measure wakeups/redraws and make static or paused
   screens event-driven while preserving animation and playback updates.
3. **Streaming duplication:** quantify requests, bytes, CPU, and RSS from
   playback and the relay. Compare architectures before choosing one.
4. **Library and search scale:** reuse the existing corpus/cache
   benchmarks, then test realistic full startup and interaction under
   large-library background work.
5. **Regression gates:** retain synthetic range, interruption, seek,
   pause/preload, track-change, and teardown scenarios; report real
   service testing separately.

No design rewrite is approved by this document. Each architectural
change should follow a measured bottleneck and preserve audio reliability,
URL privacy, queue correctness, and supported-terminal behavior.
