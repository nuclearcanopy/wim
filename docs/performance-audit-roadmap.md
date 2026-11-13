# Wim performance and reliability audit

**Status:** active; Phase 0 implementation reverted pending a complete, isolated
reimplementation. Last updated 2026-11-13.

This is the implementation checklist for making Wim start and transition quickly,
recover reliably from network problems, and run efficiently on the ThinkPad T480.
It deliberately favors measured, bounded work over adding threads or buffering
without evidence. Spotify/Netflix internals are proprietary; this roadmap adopts
general techniques (prefetch, adaptive delivery when the source supports it,
bounded workers, buffering, hardware decode, retries) without claiming their
exact implementations.

## Ground rules

- [ ] Measure before and after on the T480; distinguish Wim, mpv, and system cost.
- [ ] Never put network, whole-file compression/decompression, or unbounded work on
  the UI/audio-critical path.
- [ ] Keep parallel work bounded, cancellable, ordered where required, and lower
  priority than active playback.
- [ ] Preserve queue semantics, original library files, URL/DNS protections, and
  the rule that credentials/signed URLs never enter logs or the manifest.
- [ ] Check a box only after its implementation and stated verification are done;
  mark unavailable hardware/service checks as blocked, not complete.

## Audit already performed

- [x] Read-only source audit of playback, mpv IPC, queue transitions, and buffering.
- [x] Read-only source audit of video, Watch/TorBox, subtitles, relay, and hardware
  decode configuration.
- [x] Read-only source audit of cache, downloads, manifest privacy, and concurrency.
- [x] Read-only source audit of rendering, artwork, scanning, and search workers.
- [x] Record source-derived risks separately from measured T480 bottlenecks.
- [ ] Convert every finding below into a fix, a measured non-issue, or an explicit
  limitation; do not silently drop findings.

## Phase 0 — Finish and harden offline cache work

- [ ] Count managed compressed cache files, playback expansions, and active
  download reservations in one documented budget model. The shared process-wide
  counter is seeded from managed files; new transfer and expansion bytes reserve
  capacity before writes and release it on cleanup.
- [ ] Enforce the budget while transfers grow, not only at completed-download
  handoff; concurrent batches share reservations and a verified old copy stays
  until replacement-index persistence succeeds. (The worker cap remains per
  batch; global concurrency is tracked separately below.)
- [ ] Bound decompression output and account for temporary playback files before
  expansion; partial outputs are removed and their reservations released.
- [ ] Make cache handoff transactional: durable index first, then unlink files;
  retain references when deletion fails and recover orphaned files.
- [ ] Make cache-index writes atomic and private (write/sync a mode-0600 sibling,
  rename, sync parent); the isolated replacement test passes.
- [ ] Define and implement behavior for concurrent Wim processes; atomic rename
  alone can still lose updates between processes.
- [ ] Confine remote IDs and loaded index paths to managed cache roots before file
  creation, playback, migration, or deletion.
- [ ] Create private, exclusive playback temp directories/files; defend against
  symlinks, PID reuse, and another local user's pre-created paths.
- [ ] Scope cache keys, in-flight download IDs, and manifest identity by server
  and account; define migration for legacy ambiguous IDs.
- [ ] Invalidate or version expanded playback materializations when a cached file
  is rebuilt, replaced, or re-downloaded.
- [ ] Bound download concurrency globally across simultaneous batches and cancel
  obsolete work.
- [ ] Report partial rebuild outcomes (started, unavailable profiles, already
  running) without hiding any count.
- [ ] Warn at startup and before every download path when the effective lossy
  profile is very low; make the warning dismissible and truthful about whether
  confirmation is required.
- [ ] Cover automatic downloads through the shared core handoff and settings,
  browse, and rebuild paths. Shellsong startup/explicit-download warning exists;
  noninteractive command presentation still needs review.
- [ ] Test disk-full, persist failure, corrupt frames, interrupted transfers and
  migration, oversize files, replacement success/failure, and eviction failure.
- [ ] Measure zstd CPU, disk writes, and size change for representative MP3, Opus,
  AAC, and FLAC; retain lossless wrapping but avoid unjustified extra work.

## Phase 1 — T480 baseline and regression harness

- [ ] Record exact CPU/GPU, VA-API driver, mpv/FFmpeg versions, display session,
  power profile, and representative codec/profile support.
- [ ] Add reproducible generated-media and controlled HTTP(S) fixtures, including
  throttling, ranges, redirects, stalls, and interruption.
- [ ] Measure release startup, time-to-first-audio/frame, transition gaps, seek,
  pause, queue-change and search latency (p50/p95/worst).
- [ ] Measure process-tree CPU/RSS, disk I/O, network bytes, threads, wakeups,
  terminal output, dropped frames, and power/thermals where available.
- [ ] Compare cold/warm cache, small/large libraries, local/remote sources, and
  Wim against standalone mpv with equivalent settings.
- [ ] Save sanitized baselines and commands in `docs/performance.md` or focused
  performance docs; make benchmark names and ignored status accurate.

## Phase 2 — Fast, seamless music

- [ ] Start the first selected item before resolving the rest of a large playlist.
- [ ] Prepare later playlist entries asynchronously with bounded, cancellable
  work while preserving mpv/PlayQueue order and accepted-item invariants.
- [ ] Move cache migration, compression, expansion, and index writes off the UI
  path; never hold the shared cache lock over whole-file I/O.
- [ ] Measure whether mpv's current playlist/readahead opens the next remote item
  early enough; use supported next-file preparation before inventing a duplicate
  player pipeline.
- [ ] Add transition-capture tests for local/remote/cached combinations, codec and
  sample-rate changes, album boundaries, skips, shuffle, and loop-all.
- [ ] Define and test bounded retry/reconnect behavior for transient stream
  failures, expired links, server errors, and missing tracks.
- [ ] Keep the current sequential fade accurately labeled; evaluate true overlap
  only as a separate opt-in feature after gapless behavior is proven.
- [ ] Verify time-to-first-audio and transition-gap improvement against Phase 1.

## Phase 3 — Video behavior the sources can support

- [ ] Query/report mpv's actual video decoder and hardware-decode state; `vo=gpu`
  alone is not evidence of hardware decoding.
- [ ] Add configurable VA-API/hardware decoding with explicit software fallback,
  based on the installed T480 stack rather than assumed codec support.
- [ ] Test representative H.264, HEVC, VP9 profiles/bit depths and supported
  resolutions; record CPU, GPU, dropped frames, thermals, and picture correctness.
- [ ] Test mpv HLS/DASH manifests, nested segments, ranges, redirects, separate
  audio/subtitles, and keys through actual Wim source/relay paths.
- [ ] Where source variants exist, expose safe quality bounds and verify adaptive
  selection favors smooth playback on constrained networks/devices.
- [ ] Clearly report when a direct file/torrent has no alternate bitrate; Wim
  cannot create Netflix-style ABR without source/server variants.
- [ ] Measure buffering and mpv byte caps for high-bitrate video; tune memory
  bounds from results rather than increasing cache blindly.
- [ ] Test audio↔video queue transitions, seek, fullscreen, subtitles, recovery,
  and hardware-decoder fallback.
- [ ] Benchmark relay connection setup, segmented traffic, stalled peers, and
  thread use; optimize only without weakening DNS/redirect/private-address checks.

## Phase 4 — UI, idle power, and visualizer

- [ ] Measure idle/paused redraw rate, terminal bytes, wakeups, CPU, and input
  latency in normal, player, browse, search, and video screens.
- [ ] Reduce needless redraws while preserving animation and prompt input; provide
  a wakeup path for background results if polling is relaxed.
- [ ] Measure spectrum cost at disabled/default/high bands on T480; gate or lower
  analysis while hidden/low-power if it improves CPU without audible artifacts.
- [ ] Verify output remains transparent and filter transitions do not pop.
- [ ] Bound artwork fetch/decode concurrency; cancel stale artwork requests if
  rapid track changes cause measurable work buildup.
- [ ] Verify cover decode/downscale and cache hit/miss latency under track churn.

## Phase 5 — Library, search, and background scheduling

- [ ] Profile cold/warm library scan while music/video plays and while typing.
- [ ] Bound per-frame ingestion of completed scan batches if UI bursts are measured.
- [ ] Measure tag-scan parallelism and set a playback-friendly worker cap if using
  every available core harms latency, thermals, or battery.
- [ ] Re-measure search thresholds on the T480 across realistic corpus sizes; adopt
  a persistent pool only if end-to-end latency and power improve.
- [ ] Preserve exact search ranking, typo behavior, library completeness, and
  selection identity with equivalence tests.
- [ ] Give playback-critical work priority over scan, artwork, downloads, subtitle
  alignment, and optional analysis.

## Phase 6 — mpv IPC and process reliability

- [ ] Fail all pending IPC requests promptly on disconnect/reader EOF.
- [ ] Remove pending request entries on every write/read/error path.
- [ ] Use one total deadline for command batches rather than one timeout per
  missing response.
- [ ] Bound shutdown, terminate/reap a stuck child, and test mpv crash/restart.
- [ ] Bound or coalesce replaceable IPC telemetry while retaining playlist,
  position, idle, and track-transition correctness.
- [ ] Verify ordered playlist commands and report partial failures rather than
  claiming unsent/unaccepted entries succeeded.
- [ ] Add stress tests for disconnect, slow/no replies, event floods, rapid queue
  edits, and app shutdown.

## Phase 7 — Global concurrency and recovery policy

- [ ] Audit every thread/spawn/channel for bounds, cancellation, ownership, and
  shutdown; avoid per-item unbounded thread creation.
- [ ] Use reusable bounded pools where measurements beat simple scoped work.
- [ ] Define priorities and queue limits for networking, scan, artwork, download,
  decode preparation, and analysis.
- [ ] Ensure failed/cancelled jobs release reservations, temporary files, and
  in-flight identity; ensure retries do not duplicate work.
- [ ] Add resource-exhaustion tests for large catalogs, artwork churn, multiple
  download batches, stalled relays, and rapid playlist replacement.

## Phase 8 — Final verification and rollout

- [ ] Run `cargo fmt --all -- --check`, all-target tests, Clippy with warnings
  denied, and the current-working-tree Nix build after the final edit.
- [ ] Re-run the T480 baseline and publish before/after results with commands,
  hardware/software versions, and limitations.
- [ ] Exercise real Navidrome and supported video/TorBox services only when
  available; never put credentials or signed URLs into fixtures/logs.
- [ ] Review complete staged and unstaged diffs separately; preserve unrelated
  work and do not claim tests or platform checks that were not run.
- [ ] Mark every checklist item complete, measured non-issue, externally blocked,
  or explicitly deferred with rationale before calling the audit done.

## Work log

- 2026-11-13: Source audit completed across playback/IPC, video/sources, cache and
  downloads, UI/rendering, search, library scanning, and workers. Read-only audit
  agents identified code-level risks; no T480 runtime benchmark was performed.
- 2026-11-13: Cache-accounting and low-quality/rebuild-reporting patches were
  started in the worktree. They are **not yet fully verified**: active transfers
  are counted at handoff rather than incrementally reserved, and playback
  expansion is not rejected/reserved before it grows. See Phase 0.
- 2026-11-13: `CARGO_TARGET_DIR=/tmp/opencode/wim-target cargo test --all-targets`
  passed (1,651 passed; 92 ignored, including two new policy/reporting tests).
  This is a current-worktree verification, not T480 performance evidence.
- 2026-11-13: This host reports the T480-class i5-8350U (4 cores/8 threads),
  Intel UHD 620/Kaby Lake-R, i915, mpv 0.41.0, and Wayland. VA-API capability,
  actual decoder selection, playback latency, CPU/RSS, power, and frame drops have
  not yet been measured. `vainfo` is unavailable in this environment.
- 2026-11-13: mpv now starts with `hwdec=auto-safe`, documented as a best-effort
  preference with software fallback. This does not verify decode on representative
  media or expose a user setting; the corresponding checklist remains open.
- 2026-11-13: Added the same effective-bitrate warning to shellswim startup and
  resolved explicit-download handling, and corrected the wording so it does not
  claim dismissal blocks/continues the download. Focused tests now cover the
  low-quality policy and combined partial-rebuild status. Automatic-download
  warning now lives at the shared core handoff, covering both frontends; its
  call-site test was added. Shellsong startup/explicit warning coverage is in
  place, while shellsong command/noninteractive presentation still needs review.
- 2026-11-13: Cache index persistence now writes a unique, owner-only sibling,
  syncs it, atomically renames it, and syncs the parent on Unix. A replacement
  test checks round-trip contents, private mode, and temp cleanup. Multi-process
  lost-update behavior remains unresolved.
- 2026-11-13: Added shared cache-budget reservations seeded from existing managed
  track/playback files. Download writers reserve compressed bytes per write;
  playback expansion reserves uncompressed chunks and refuses before exceeding
  the cap. Successful eviction, stale sweep, process playback cleanup, and
  failed writes release accounted bytes. Focused tests cover full-disk transfer
  cleanup and refused expansion. Global cross-batch worker limits and broader
  transactional/orphan recovery remain open.
- 2026-11-13: Verification after these changes: `cargo test --all-targets`
  passed (1,655 passed, 92 ignored), `cargo clippy --all-targets -- -D warnings`,
  `cargo fmt --all -- --check`, `git diff --check`, and the latest Nix build
  passed. The Nix build produced
  `/nix/store/aswzqbgf7rslnhmmgqj1sq27f6p8kwfv-wim-0.1.0`.
- 2026-11-13: The Phase 0 implementation commit was reverted after CI exposed
  malformed source hunks and an incomplete dependency/config integration. The
  earlier test/build results describe the uncommitted working snapshot, not the
  current branch. Re-implement Phase 0 as a complete, separately verified change.
