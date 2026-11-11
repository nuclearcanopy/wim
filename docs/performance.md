# performance: product budgets and measured work

Wim's performance goal is not "a fast tui" in isolation. It is a player
that feels immediate, keeps audio reliable, and spends CPU, memory, and
network only where the feature warrants it. Compare complete process
trees against mpv on identical media and conditions; a fast widget
benchmark is not evidence that playback is efficient.

## measurement rules

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

## first baseline observations

These are useful signals, **not yet a complete end-to-end comparison**.
Measured in this worktree on mpv v0.41.0 with a release build:

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

## workload and metrics to keep

Run the same generated media and isolated XDG state for:

1. cold and warm launch through first usable frame;
2. keypress-to-visible-response while idle, searching, scanning, and
   playing;
3. play-to-first-audio, pause/resume, seek, and queue advance;
4. audio-only local and throttled remote playback;
5. inline/fullscreen local and remote video, including seek and resize;
6. paused/idle CPU, active CPU, process-tree RSS, redraw rate, terminal
   bytes, and network bytes per stream.

Use mpv with equivalent output and cache settings as the playback
baseline. Keep startup, steady-state, and feature incremental costs
separate. After repeated runs establish numeric budgets from the actual
hardware rather than choosing attractive but unverified targets.

## work completed in the first performance pass

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

## next investigations, ordered by risk and impact

1. **UI-thread stalls:** instrument loop gaps and attribute them to
   synchronous IPC, filesystem, config writes, drawing, or event drains;
   move only measured long work off the loop with cancellation and
   bounded queues.
2. **Idle work:** measure wakeups/redraws and make static or paused
   screens event-driven while preserving animation and playback updates.
3. **Streaming duplication:** quantify requests, bytes, CPU, and RSS from
   audible mpv, inline frame mpv, and the relay. Prototype coordinated
   two-process behavior against a libmpv single-engine alternative before
   choosing an architecture.
4. **Library and search scale:** reuse the existing corpus/cache
   benchmarks, then test realistic full startup and interaction under
   large-library background work.
5. **Regression gates:** retain synthetic range, interruption, seek,
   pause/preload, track-change, and teardown scenarios; report real
   service testing separately.

No design rewrite is approved by this document. Each architectural
change should follow a measured bottleneck and preserve audio reliability,
URL privacy, queue correctness, and supported-terminal behavior.
