# wim roadmap

terminal omniplayer. full spec lives in README.md; this file tracks what's
done and what's next, for both the maintainer and the overnight build loop.

**the standing bar for every feature, not just the ones listed below:**
is this an incredibly loved feature in top-tier apps? is it modern,
efficient, and does it make the app better for everyone? if yes, build
*that* version, not the minimal literal-request version, even unasked.
see `docs/features.md` for the running rationale behind each feature
and a list of "loved app" additions layered on top of the original
spec (eq presets, gapless/crossfade, smart shuffle, a queue, a now-
playing audio level visualizer, resuming playback state). read
`docs/architecture.md` before touching the areas it covers.

**UX self-check, every single feature, no exceptions:** before calling
anything done, ask "is this the best UX, intuitive and beautiful?" The
honest answer is almost always no the first time; that's expected,
not a failure, but it means keep iterating rather than stopping at
"it compiles and has a test." A feature that works but looks/feels
barebones is not actually done. This was learned the hard way: a
whole session's worth of backend work shipped before anyone actually
looked at the rendered screen, and the first real look was a blank
void with no onboarding, no indexed library, and a thin unimpressive
title card. Visual/UX work specifically needs a human to actually look
at it; don't mark it done on build+test passing alone.

**rules for anyone (human or loop) working from this file:**
- small progressive commits, one coherent change each, lowercase subject,
  no ai co-author trailer (this is the maintainer's own repo)
- every commit must leave `nix develop --command cargo build` passing
  (and `cargo clippy` clean where reasonable) before you commit
- strictly monochrome ui: every color is `Color::Rgb(v,v,v)` via
  `src/theme.rs` helpers, never a hue, anywhere, including album art in
  `blocks` mode (real sixel/kitty/iterm2 art can stay true color)
- no stock ratatui `Block`/`Paragraph`/`List` widgets; custom-painted
  widgets only (see `src/ui/banner.rs` for the pattern)
- can't test raw-mode TUI rendering headlessly (no real tty in CI/loop
  context); rely on `cargo build`/`cargo check`/unit tests for logic,
  note in the commit body what still needs a human to eyeball
- genuinely blocked on a decision only the maintainer can make? don't
  stop; write it to QUESTIONS.md and move to the next unblocked item

## done

- [x] **onboarding looked like a debug dump, and taught wrong keys.**
      two problems in one file. visually: five screens each painting
      raw left-aligned lines at `area.y`, no frame, no vertical
      centring, no sense of progress, and a plain-text "welcome to wim"
      where the ansi-shadow wordmark should be; exactly the "thin
      unimpressive title card" the ux self-check above was written
      about. all five now share one `Screen` chrome (frame, content
      block centred as a unit so short and long rows do not stagger, a
      progress rail pinned to the interior's bottom row) and welcome
      draws `banner::logo_lines(0)`.
      correctness: the controls list baked in key literals ("l",
      "[ / ]", "a"), so onboarding confidently taught the defaults to
      anyone with a custom keymap. `help.rs` had this exact bug and
      fixed it with its `Key` enum; that enum is now `pub(crate)` with
      a `label(&Keymap)` method and onboarding resolves through it, so
      a rebound key is shown as what it is actually bound to. the
      guard test was verified to FAIL when the old behaviour is
      simulated, rather than just passing.
      the rail degrades rather than clipping: under ~23 interior cells
      the dots are dropped and the words "step n of 6" alone remain.

- [x] scaffold: flake.nix devshell, AGPL-3.0, README
- [x] cargo init, full dependency set pinned
- [x] config: toml file at `$XDG_CONFIG_HOME/wim/config.toml`, templated
      on first run, `password_file` support for agenix-style secrets
- [x] mpv: json ipc control layer (spawn, socket, request/reply +
      event demuxing), play/pause/seek/volume/speed/loop-file wired
      but unused
- [x] ui: custom-painted `Banner` widget proving the no-stock-widgets,
      monochrome-gradient, animated approach (later replaced with real
      figlet isometric1 3d lettering; see below)
- [x] onboarding (`src/onboarding.rs`, `src/ui/onboarding.rs`): real
      guided first-run sequence (welcome/controls/navidrome/library
      paths/done), not a disconnected login popup. config.rs no longer
      bails with a cli error on first run. text-overflow handling
      (`src/ui/textfit.rs`) so typed text doesn't silently run off a
      narrow terminal. contrast hierarchy fixed (dividers much dimmer
      than text, not the same brightness).
- [x] real navidrome catalog indexing + multi-profile management:
      login now fetches the actual artist/album catalog
      (getArtists/getAlbumList2, checked against Feishin's real
      implementation first) instead of only playlists. config schema
      is `navidrome_servers: Vec<..>`; multiple servers, all merged
      into one corpus, no "active server" concept. 'n' opens a profile
      manager (list/add/remove), replacing a single always-blank login
      form that looked destructive even though it wasn't. banner swap
      to larry3d (non-isometric, no diagonal letter drift), bracketed
      paste support, url ghost-prefix input all landed alongside this.
- [x] navidrome reliability pass: corpus mutation now resyncs the
      displayed results immediately (no more stale list until you
      happen to type a character), startup reconnect failures are
      reported instead of silently looking like nothing's configured,
      remote items dedup by (kind, id) so reconnecting never
      duplicates the catalog, and the profile manager pings every
      server up front showing ✓/✗ per row.
- [x] blinking cursor block in the search box while editing
      (`src/ui/searchbox.rs`), classic terminal-style ~530ms blink.
- [x] popup notification stack (`src/notify.rs`, `src/ui/notify.rs`):
      bordered boxes stacked top-right, newest on top, each with its
      own countdown baked into the bottom border (lifetime scales 3-10s
      with message length); i/!/x ascii severity codes, brightness-
      coded (not color). wired into every connectivity check, and
      playback/lyrics failures; routine success feedback stays on the
      ambient status line.
- [x] background navidrome resync (`spawn_background_sync` in
      `main.rs`): a dedicated thread re-fetches every configured
      server every 45s so the catalog stays live without a manual
      refresh key; tracks last-known reachability per server and only
      notifies on an actual offline/online transition, not every retry
      while one stays down.
- [x] three-state loop (`app::LoopMode`, off/one/all) instead of a
      binary toggle; "one" maps to mpv's own loop-file, "all" has no
      mpv equivalent (it only sees its own playlist) so it's tracked as
      our own session snapshot (`playqueue::PlayQueue::session`) and
      replayed from the top once mpv's playlist drains with that mode
      active.
- [x] starring (`src/favorites.rs`, `subsonic::Client::star`/`unstar`/
      `get_starred_songs`): `*` stars/unstars whatever's hovered,
      remote songs call navidrome's own star api (stays in sync with
      any other subsonic client), local files fall back to a
      persisted `favorites.toml`. `S` opens a "starred songs" view that
      behaves like any other track listing, merging every server's
      starred list with local favorites.
- [x] real player screen (`src/ui/player.rs`, `src/ui/miniplayer.rs`):
      title, elapsed/duration progress bar, loop/shuffle/speed/eq, and
      previous/next in queue, polled fresh from mpv each frame.
      maximized (default for a standalone play) takes over
      `main_content`; minimized (default when playing from an open
      album/playlist, or by hand via `m`) drops to a thin ipod-style
      strip above the main bottom bar, spanning only `main_content`'s
      width so it never slides under the sidebar. transport uses real
      unicode media-control glyphs (⏮ ⏯ ⏭), not ascii stand-ins.
- [x] album art (`src/ui/albumart.rs`, `ratatui-image`): fetched via
      `getCoverArt` when an album opens. `i` toggles ascii (default;
      grayscale block rasterization resampled fresh from the source
      image at the panel's current size every render, so resizing the
      terminal re-rasterizes at the new resolution for free, strictly
      monochrome) vs. real (`ratatui-image`'s actual sixel/kitty/iterm2
      protocols, auto-detected once at startup, falling back to
      colored halfblocks if none is supported; that fallback isn't
      monochrome, the explicit tradeoff of asking for real images).
- [x] result row icons switched twice this round: unicode glyphs (◉/○
      etc.) risked emoji-presentation fallback or ambiguous terminal
      width depending on font, so both the row icons and the
      notification severity icons settled on plain ascii (`[sk]`
      bracket codes, `i`/`!`/`x`) instead.

## layout overhaul + artist/album browsing (explicit spec, do these in order; each depends on the one before)

the current single-pane results list is a placeholder, not the real
layout. the actual design:

1. [x] **sidebar + bottom bar layout.** boxed search box + results on
   the left (`src/ui/searchbox.rs`), multi-line playback bar on the
   bottom (`src/ui/controlsbar.rs`), main content area for the player/
   album view/lyrics (`src/ui/player.rs`, `src/ui/browse.rs`,
   `src/ui/lyrics_view.rs`). sidebar only horizontally resizable,
   bottom bar only vertically (plus the mini player strip, vertically
   fixed); nav manager/login stay full-screen modals (setup flows, not
   core browsing).
2. [x] **resize mode** (`src/layout.rs`, tested): `r` toggles it,
   `a`/`w` and `s`/`d` select sidebar/bottom bar, arrow keys stretch
   along that panel's one valid axis only (the other axis is a genuine
   no-op per panel).
3. [x] **result row icons**: `[sk]` plain-ascii bracket codes (source
   letter + kind letter), not unicode glyphs; see the ascii-switch note
   further up for why.
4. [x] **artist profile view**: enter on an artist opens the same
   `BrowseView`/`BrowseListView` as albums/playlists, with a bio
   (navidrome's own `getArtistInfo2`, proxied from last.fm, best-
   effort) and every song actually credited to them across every
   album, chronologically. true genius-sourced bios would need a
   separate integration; not built, not blocking.
5. [x] **album/playlist view**: enter on an album or playlist opens a
   scrollable track listing in `main_content` (`src/browse.rs`,
   `src/ui/browse.rs`); title/artist/year for albums, title only for
   playlists (no single artist). enter on a track plays from there to
   the end of the list; esc returns `main_content` to whatever it'd
   otherwise show (now-playing/lyrics/the idle banner). the quick-queue
   key (`a`) still expands+queues the whole thing directly, no
   browsing step. remote playback itself (stream urls, multi-server
   client attribution per `SearchItem`) landed alongside this.
6. [x] **the "Global/Various Artists" compilation problem**: fixed via
   `artist_tracks` in `main.rs`, filtering each album's songs by their
   own track-level `artist` field rather than the album's credited
   artist. works for any compilation, not a hardcoded "Global" case.

## next (roughly in order, pick whichever is actually unblocked)

- [x] subsonic client (`src/subsonic.rs`): token auth, ping, search3,
      playlists, stream url. token math unit-tested against the
      official api docs example.
- [x] local library scanner (`src/library.rs`): walks `config.library_paths`
      (defaulting to `~/Music`/`~/Videos` via `default_library_paths()`
      when unset; most people shouldn't have to configure paths just
      to get started), classifies by extension, reads audio tags via
      `lofty`.
- [x] unified search (`src/search.rs`): one corpus over local tracks +
      local video + navidrome artists/albums/songs/playlists, merged
      and ranked together. includes typo tolerance (nucleo subsequence
      fuzzy + damerau-levenshtein edit-distance fallback); see
      `docs/features.md` for why that's not optional.
- [x] app key-handling state machine (`src/app.rs`): vim-like nav
      (j/k, gg/G, `/` to edit the query live, enter, q/esc), fully
      unit-tested, decoupled from rendering.
- [x] app event loop in `main.rs`: real render loop driving `App` +
      `mpv::Mpv` + a custom results list + status bar. local
      music/video items load into mpv on enter; remote items report
      "not playable yet" since live navidrome search isn't merged into
      the loop yet (needs a real server to test against).
- [x] play queue: `a` enqueues into mpv's own playlist, which is the
      only queue there is; the current track is resolved from mpv's
      `playlist-pos` (`src/playqueue.rs`). reorder/view-the-queue ui
      still pending (queue currently has no visible list of its own,
      just a count in the status bar).
- [x] playback controls: loop-file (`l`), speed (`[`/`]`), shuffle (`s`,
      reshuffles the queue in place + randomizes future inserts).
      "avoid immediate repeats" smart-shuffle is more meaningful once
      there's a "shuffle whole library into queue" feature; not
      applicable to today's small explicit queue the same way.
- [x] gapless playback: queue (`a`) now appends to mpv's own native
      playlist instead of us reactively reloading on `end-file`, so mpv
      auto-advances internally with no ipc round-trip gap.
- [x] crossfade (toggle `c`, duration via `config.crossfade_seconds`,
      off by default); separate from gapless, which stays on
      regardless. not a true overlapping crossfade (one mpv instance,
      one file loaded at a time, can't play two simultaneously); a
      fade-out/fade-in volume ramp at the track boundary instead.
- [x] 10-band eq (`src/eq.rs`) with presets (flat/bass boost/vocal
      boost/treble boost/rock/classical), `e` cycles. applied via
      chained lavfi `equalizer` filters on mpv's `af` property. preset
      gain values are heuristic starting points, not acoustically
      tuned.
- [x] now-playing audio level meter (`src/ui/visualizer.rs`): a
      labeled `astats` lavfi tap on the af chain exposes RMS level via
      mpv's `af-metadata` property, rendered as a scrolling unicode-
      block history. honest caveat: one overall level, not a true
      per-band spectrum (a real `cava`-style meter needs a parallel
      bandpass/astats/amix filter graph); falls back to a calm idle
      sine pulse if the af-metadata property doesn't resolve the way
      expected (unverified against real mpv audio decode in this
      sandbox, no audio device available to test against).
- [x] playback state persistence (`src/persistence.rs`): resumes
      queue/now-playing/position/volume across restarts via plain
      paths under `$XDG_STATE_HOME/wim/state.toml`, best-effort both
      directions (missing file, gone path, or a failed save are all
      silently fine, not errors).
- [x] replaygain: mpv reads embedded tags itself once `replaygain=track`
      is set; enabled by default at startup, no extraction needed on
      our end for local files. subsonic's replayGain fields for remote
      tracks deferred along with the rest of live navidrome wiring.
- [x] transcoding: server-side by default (navidrome's own configured
      profile); `max_bitrate_kbps`/`transcode_format` per navidrome
      server add an optional client-side override on top.
- [x] lyrics (`src/lyrics.rs`): lrclib.net (free, no api key), `L` to
      view/close, j/k to scroll. known gap: blocking fetch on the
      render thread (visible hitch during lookup), and SearchItem
      doesn't carry duration so local lookups always use the fuzzy
      /search endpoint rather than the exact /get one; both noted
      inline, not blocking, fine to revisit.
- [x] album art (`ratatui-image`): see the album art entry further up;
      `config.album_art` now sets the starting mode (closed the gap
      noted here previously).
- [x] video playback + opensubtitles (`src/opensubtitles.rs`):
      auto-fetch behind `config.opensubtitles_api_key`/`_file` (same
      plaintext-vs-file split as navidrome's password). the actual
      rest endpoint shapes are an unverified best-effort guess, see
      QUESTIONS.md; needs a real key from the maintainer to live-test.
- [x] subtitle styling: delay, size, position, outline, background
      opacity, blur, color (grayscale level only, per the monochrome
      rule) all via mpv `sub-*` properties, set from `config.subtitles`
      at startup. sync (`,`/`.`) is the one knob adjustable live, the
      rest are config-only. language selection is the preferred
      opensubtitles search language, not yet a multi-track switcher
      (no embedded-track scenario tested).
- [x] nix packaging: `packages.<system>.wim` (and `.default`) via
      `rustPlatform.buildRustPackage`. mpv is a runtime subprocess dep
      (not linked), so the binary is wrapped with `makeWrapper` to put
      it on `PATH` rather than requiring it installed system-wide.
      verified with `nix build` + `nix flake check` and manually ran
      the wrapped binary with an emptied inherited `PATH` to confirm
      mpv really comes from the wrapper, not the ambient environment.
- [x] home-manager module (`nix/home-manager.nix` + flake
      `homeManagerModules.default`): declarative options mirroring
      config.toml 1:1, including every field added this session
      (crossfade, per-server transcoding overrides, subtitles,
      opensubtitles). verified via a standalone `lib.evalModules`
      instantiation against fake home-manager options, not the full
      home-manager flake (much faster, same actual module code path).

## maintainer review pass (first real human look at the rendered app)

the UX self-check above was vindicated: the maintainer ran the app and
found a lot that build+test had happily called done. recorded here
because the root causes are the useful part, not the fix list.

- [x] **enter did nothing.** the loudest complaint, and two separate
      bugs wearing one costume. `app::handle_key` returned an `Action`
      that the `editing_query` call site dropped on the floor before
      `continue`ing past the only code that acts on it, so the first
      enter after typing silently closed the box and cleared the queue
      and a second was needed. separately, enter on an album inside the
      artist profile set `open_album` and then `continue`d past the
      block that reads it. the dispatch is now one `dispatch_action`
      function both key paths route through, and `Action` is
      `#[must_use]` so a dropped action is a compile error.
- [x] **nothing showed which pane had focus.** replaced the three
      absolute focus keybinds with explicit move/interact modes (`tab`
      enters move mode; wasd/arrows move focus and nothing else
      responds), the focused pane named outright in the bottom bar, and
      each search box labelled in its own border (`┤ search ├` vs
      `┤ filter songs ├`) since the two rendered identically and a
      blinking cursor was the only focus signal, absent half of every
      blink cycle.
- [x] **borders that did not meet.** junction glyphs were hardcoded in
      main.rs; two were simply wrong and one was dead code, because
      `VerticalRule` resolves junctions with `.find()` and an earlier
      hardcoded row shadowed the later push. now derived from which
      arms actually carry a line (`src/ui/junctions.rs`), with buffer
      snapshot tests per app state.
- [x] **the bottom bar narrated at you.** the status line is deleted
      outright, not filtered: ~21 sites that wrote transient sentences
      ("now playing: x", "error loading file") now either raise a
      notification or say nothing. a mode indicator took its place.
- [x] **destructive menus.** the eq menu applied a preset on every j/k
      and wrapped, so scrolling past the last band silently overwrote
      all ten hand-tuned gains and esc saved it; presets now need
      enter and the list clamps. `d` in the profile manager deleted and
      rewrote config.toml on one keypress; now two.
- [x] **help was unreachable.** ~50 rows in one column, so on a 24-row
      terminal the last sections (including how to quit) fell off the
      bottom silently. now column-packed, first-fit, never splitting a
      section, with tests asserting every heading is on screen at 80x24.
- [x] **every key hint lied after a remap.** `src/ui/help.rs` was the
      only surface reading the live keymap; the dashboard, player,
      miniplayer and menus hardcoded defaults. all resolve through
      `key_label(keymap.get(..))` now, with the genuinely-fixed keys
      (j/k, enter, esc, tab) kept literal and `reserved_key_purpose`
      refusing a binding onto them rather than silently shadowing it.
- [x] **perf.** artist pages took ~5s: not the N+1 fan-out (already
      threaded) but `ureq` pooling exactly one idle connection per
      host, so 15 parallel requests each paid a fresh TLS handshake
      (1.94s to 375ms). search was 8-27x slowed by per-keystroke
      haystack allocation and a fresh nucleo matcher per call, with
      ranking verified byte-identical across 30 query/corpus combos.
      startup no longer blocks on the catalog fetch, the library scan,
      the restored queue's ipc appends, or the terminal graphics probe.
- [x] **security.** config.toml was mode 0644 with the navidrome
      password in cleartext; now 0600, zeroized on drop, and the app
      warns when it is about to persist a plaintext password. subsonic
      auth needs the cleartext for every request (`md5(password+salt)`)
      so it cannot be hashed away; `password_file` plus agenix remains
      the real answer and is already supported.

### the "horrible bass" investigation (resolved, not wim's fault)

worth writing down because three plausible theories were wrong and the
measurements are what settled it.

- the eq was innocent: flat chain measured -89.8 dB RMS residual vs dry
  (ffmpeg), -95.9 dB through mpv's json ipc, and flat within 0.02 dB
  per octave band through the real binary. no resample or downmix stage
  inserted; `astats reset=1` is bit-transparent. see
  `docs/audio-quality.md`.
- `format=raw` was a dead end: the source file is already opus 246k and
  both the old (no `format` param) and new paths return byte-identical
  `audio/ogg`, so navidrome was never transcoding for wim. the change
  is kept as a correctness guarantee, not a fix.
- replaygain was a dead end: the server reports `replayGain: {}`.
- actual cause, found by reading the live pipewire graph: wireplumber
  had a persisted per-application volume for mpv of
  `channelVolumes: [3.548, 3.548]`, i.e. **+11.0 dB**, restored into
  every mpv stream. the browser was unaffected because that state is
  keyed per application. +11 dB on modern masters hard-clips and bass
  carries the energy, hence "clipping from how much bass there is".
  reset to unity, and wireplumber persisted the correction.
- real defect found while in there: ten 1-octave peaking bands spaced
  an octave apart sum, so "bass boost" declared +6/+5/+3/+1 dB and
  measured +7.05/+6.70/+4.31/+1.77. `compensated_gains` now inverts the
  measured spill matrix; a flat eq emits no biquads at all and null
  tests bit-identical.

## video in the workspace (maintainer spec, 2026-10-28)

real video is the DEFAULT and stays the default. the ascii path is a
thing the user turns on deliberately, never something they land in by
accident.

- [ ] **BLOCKER: there is no working way to get frames out of mpv.**
      found by `perf/mpv` and it gates every item below. `screenshot-raw`
      **cannot work over json ipc at all**: mpv's own manual says it is
      client-api/script only, and over ipc it replies with a `data`
      field containing nothing after it (the value is an
      `MPV_FORMAT_BYTE_ARRAY` and mpv's json writer has no encoding for
      one), so **the reply line is not valid json**. the old reader
      skipped the unparseable line, the pending request was never
      answered, and the main loop sat in a 10 second `recv_timeout`;
      **the whole ui froze in 10 second blocks for as long as a local
      video played.** verified on a real h264 file and a lavfi source,
      with and without `--vo=null`, at every `flags` value. the pump
      now recognises the reply as `Unsupported`, stops asking and
      notifies once, so the freeze is gone, but no frames arrive.
      measured alternatives, none free: `screenshot-to-file` plus
      read-back is 202ms at 480p and 530ms at 1080p per frame on mpv's
      side (2-5fps, and 1080p jpg failed outright); an mpv lua script
      using `mp.command_native`; or `--vo=image`. each is a new
      subsystem, so none were built. `FramePump`'s public surface does
      not change under any replacement source, so this is a swap of the
      source only. see QUESTIONS.md.
- [ ] **video plays as real video in the workspace.** confirm the
      maximized-song path actually shows a real video surface for a
      video file, not the album-art fallback and not ascii.
- [ ] **ascii video renderer, opt-in.** same technique as album art,
      via the shared rasterizer being extracted on `perf/art`:
      half-block glyphs so one cell carries two vertical samples.
      toggled by the user (config key plus a shellsong command plus a
      key binding); default off.
- [ ] **usable at absurd zoom-out.** ascii video is only watchable
      once the font is small enough to give a few hundred cells across,
      so the layout has to survive that: sidebar and bottom bar must
      stay visible and operable at very small cell sizes, rather than
      the video eating the frame or the chrome collapsing to unreadable
      slivers. this is a layout constraint, not a rendering one; the
      chrome should scale its own footprint, not vanish.
- [ ] **frame budget.** a few hundred cells across times a few hundred
      down, at video frame rate, is the real cost. the work is CPU
      (decode, downscale, per-cell sampling, diffing the buffer), not
      gpu: the terminal emulator rasterizes glyphs on the gpu and that
      part is cheap even for a full-screen repaint. the things that will
      actually cook a core, in order: decoding every frame at full
      resolution rather than downscaling once to the cell grid,
      per-pixel `get_pixel` instead of a contiguous walk, reallocating
      the output buffer per frame, and repainting cells that did not
      change. decouple the ascii frame rate from the display refresh
      rate and make it tunable; dropping to a lower rate must degrade
      smoothness, never block the input path.

## in flight

**all five perf branches are MERGED into master as of 2026-10-28**
(`perf/search`, `perf/startup`, `perf/mpv`, `perf/art`, `perf/shell`),
each verified after its own merge: clippy `-D warnings` clean, full
test suite green, fmt clean. 741 tests now pass. their measurements and
findings are recorded in the sections below and in `docs/`
(`search-performance.md`, `startup-cache.md`, `raster.md`,
`architecture.md`).

one real collision, worth knowing about if agents are ever fanned out
this way again: **`perf/search` and `perf/shell` independently
optimized `Corpus::search`** and restructured the same function, so six
conflicts landed in `search.rs`. resolved toward `perf/search`'s
machinery, because it carries an oracle test that keeps the
pre-optimization scorer verbatim and compares item-by-item over 38
query shapes; shell's one non-overlapping piece (`search_names`, which
completion calls per keystroke to avoid cloning a `SearchItem` per
match) was re-implemented on top, with `rank` now being
`rank_borrowed` plus the clone so the two entry points cannot rank
differently.

also reconciled at merge, as `perf/mpv` predicted: `sample_luma` is
gone entirely and both art paths go through `perf/art`'s O(output)
rasterizer (`with_memo` for static covers, `with_scratch` for anything
changing per frame), so mpv's warning about ~2.07M `get_pixel` calls
per frame on the render thread is moot and `VIDEO_FRAME_MAX_EDGE`
staying `None` costs nothing in sampling.

- `feat/mouse` + `wim-wt-mouse` (12 commits, NOT merged, held
  deliberately). all four gates pass on its tip and it adds click to
  focus, click to press, wheel to move the selection and drag to
  scrub. **held because no human has ever clicked it**: under a bare
  pty the ratatui-image graphics probe goes unanswered and then eats
  stdin, so raw-mode input cannot be exercised headlessly here. the
  hit resolution is well covered as pure functions, which is not the
  same as working. merge after one manual pass. note it rewrote the
  `main.rs` regions the merged perf branches patched, so expect
  conflicts there now and resolve toward mouse's structure with the
  perf call sites re-applied.

### album art, real vs ascii (maintainer report, 2026-10-30)

- [x] **`album_art = "auto"` never showed real art.** the default
      config's own mode. `wants_real_art` excluded `Auto` and
      `art_mode_for_picker` only returned `Real` for a *named* protocol,
      so "auto" effectively meant "start in ascii and press `i`", which
      is not what the word means and left the default config on the
      rasterizer even where the terminal fully supported sixel/kitty.
      `auto` is a detect now: real when the probe answered with a
      protocol, ascii when it did not. `blocks` is unchanged and is the
      way to ask for the monochrome rasterizer deliberately.
- [x] **`i` into real mode on a terminal with no graphics protocol was
      a silent blank.** the widgets draw ascii only in `ArtMode::Ascii`
      and main.rs overlays the image in `Real`, so with no `Picker`
      real mode is an *empty pane*, not a fallback. the existing
      "no graphics protocol" alert only fired when the **config** named
      a protocol, so cycling with `i` got no explanation at all. `i`
      now skips a real mode the terminal cannot render and says why
      once.
- [ ] **alacritty cannot do real art at all.** not a wim bug, recorded
      so it stops being rediscovered: alacritty (0.17 here) implements
      no inline-graphics protocol, so the probe correctly reports none
      and ascii is the only honest output. `foot` is installed on this
      machine and does support sixel, which is the way to actually see
      the real-art path. worth a line in the readme next to the
      `album_art` setting.

### found by the agents, not yet fixed (all in files they own)

- [ ] **nobody has clicked the mouse support.** see `feat/mouse` above;
      the hit resolution is well covered as pure functions but no real
      click has been driven end to end. a human pass is the only way.
- [ ] **the paste handler's tier chain diverges from the key cascade.**
      `Event::Paste` checks `settings_token_editing` before
      `console_open` (keys do the reverse) and has no `editing_query`
      guard at all, so a paste reaches `app.query` whether or not the
      box holds the keyboard. harmless only because those states cannot
      currently coexist, which is the kind of thing that stops being
      true silently.
- [ ] **`InputMode`'s doc claims `focus` is only ever changed from
      `InputMode::Move`, never as a side effect.** already untrue
      (`dispatch_action` and several pane tiers change it) and clicks
      make it more so. the comment is load-bearing elsewhere, so it
      wants a careful reword rather than a delete. also `main.rs:2870`
      has the `focus_in_direction` doc block attached to `enum
      InputMode` by mistake.
- [ ] **`suggestions::render` has a `flat.get(idx)` -> `continue`
      fallback that cannot fire**, and `row_items` does not mirror it;
      that is the one place the hit map could formally name a row the
      render skipped.
- [x] **`fetch_navidrome_catalog` pings before fetching, serially.**
      ping removed; a failing fetch says everything it said, and it was
      a serial round trip per server per 45s pass (~80 wasted requests
      an hour). `Client::new` still runs, being local and free.
      **the ping was hiding something worse, which is the real fix
      here.** each of the three fetches was `unwrap_or_default()`, so
      any request failure arrived as an empty list inside an `Ok`. once
      `perf/startup` added `Corpus::reconcile_server`, which drops what
      a server no longer lists, an empty list stopped being harmless
      and became a *claim that the server has nothing*: one failed
      `getArtists` during a sync would delete that server's whole
      library from the corpus, and `save_catalog` would then persist
      the deletion. the ping only narrowed the window (ping fine, fetch
      times out moments later) rather than closing it. every fetch
      error and thread panic now propagates, via `fetched()`, with
      three tests pinning that a failure is not an empty catalog while
      a genuinely empty server still succeeds. `reconcile_server`'s doc
      now states the caller contract both ways.
- [x] **unknown runtime names gave bare errors.** `eq::resolve_failure`
      (added with the `eq()` fix) and `load_queue`'s
      "no saved queue called X" were the same problem solved once and
      not at all: names the user invented are in no static table, so
      shellsong cannot validate them while parsing and the error has to
      be built at apply time. `src/namehint.rs` is the shared one;
      `eq::resolve_failure` delegates to it and `load_queue` uses it
      now. three behaviours worth keeping: a near miss suggests
      ("did you mean 'rock'?"), nothing near lists what exists, and an
      **empty** list says "none saved yet" rather than rendering a bare
      "have: " (the likeliest case for `load_queue`, since a user with
      no saved queues is exactly who mistypes one). long lists are cut
      at 40 and counted so a notification stays readable.
- [x] **fresh `Client` per call in `open_starred_view` and
      `test_navidrome_connectivity`.** both reuse a live client now via
      `client_for`, falling back to building one only when the corpus
      holds none for that server (cold start, or a profile just added).
      the zeroize worry in the original note turned out to be moot
      rather than solved: the reused clients are the **corpus's own**,
      which already live for the session and already hold the password,
      so nothing extends its lifetime and it is still zeroized when the
      corpus drops. a `static` would have been the problem, since it
      would never drop and so never zeroize; this is not one.
      **`Client::matches` includes the stream options, not just
      username and url.** a client carries `max_bitrate_kbps` and
      `transcode_format` into every stream url it builds, and
      `open_starred_view`'s items get *played*, so matching on identity
      alone would have let a client built before a bitrate change go on
      streaming at the old setting. that is a bug this fix would have
      introduced; a test pins it.
      pinging the live client is also the better connectivity test:
      same pool, same resolved credentials, rather than a throwaway's.
- [x] **the 45s sync pass still builds its own `Client`.** done, and
      the lifetime question answered differently than this note
      guessed: the sync thread does **not** reach for the corpus's
      clients (it cannot; different thread) but keeps its own across
      passes. `Client::new` re-reads `password_file` off disk and
      starts with a cold pool, and this loop runs every 45 seconds for
      the life of the app, so rebuilding per pass was continuous waste.
      `retain_live_clients` runs at the top of every pass and drops any
      kept client no current profile still describes, which is what
      stops an edited or removed profile from syncing through the old
      credentials; dropping it is also what zeroizes that password.
      extracted as a named function rather than left inline so the
      three tests exercise the real predicate instead of a copy that
      could drift. `Client::matches` counts the stream options, so a
      bitrate change is an edit here too.
      `index_navidrome_server` deliberately passes `&[]`: it runs when
      a profile is added or reconnected after an edit, where building
      from the credentials as they are now is the whole point.
      also fixed on the way through: routing the catalog fetch via
      `client_for` had reduced "connection failed: {e}" to a bare
      "connection failed", losing an unreadable `password_file` or a
      malformed url as the cause, so `client_for` returns `Result`.
- [ ] **`Corpus::remote_clients` is an O(corpus) scan.** it walks every
      item and dedupes by url, so it is a full 40k-item pass to answer
      "which servers are live". fine where it is called now (a keypress:
      starred view, the profile manager, `load_queue`), but it must not
      drift onto a per-frame path, and a cached per-server list keyed on
      the corpus's own generation would retire the worry.
- [ ] **the first open of an album or artist still blocks the main
      thread.** `open_browse_view`/`open_artist_view` are synchronous;
      the response cache makes repeats free and the pool makes a cold
      open 5x faster, but truly non-blocking needs the currently unused
      `stream_artist_tracks` wired into the ui.
- [x] **the browse view's own cover fetch bypasses the new disk
      store.** it goes through it now. the two keyspaces are one:
      `Key::album(artist, album)` is the single definition and `Key::of`
      delegates to it, so an album opened in browse and the same
      album's tracks played from the player derive the same key and so
      the same file on disk. reopening an album is now free, and a
      cover either side fetched is the other's hit. `fetch_stored` is
      the public store-first/store-after fetch for callers outside
      `Cache`, reusing the private `fetch` rather than growing a second
      copy of the sequence.
      it did not need the identity work after all: the browse site
      already destructures `SearchItem::Album { id, name, artist, .. }`,
      so the names were in hand. **what identity would still buy** is
      pinned as a test rather than left as a surprise
      (`a_compilation_track_does_not_share_the_albums_key`): a
      compilation's tracks each carry their own artist, so they key
      separately from the album and that cover is still fetched more
      than once. an album-artist or album id on `SearchItem` is what
      closes that, and nothing else here depends on it.
      untagged albums fall back to the id's own slot, so they keep
      working, just without sharing.
- [ ] **catalog parse beyond ~100k rows** would start outgrowing mpv's
      spawn and resurface on the first frame; parallelize the row parse
      if that ever shows up.

## shellsong and suggestions (engines merged, hosts not wired)

- [x] **shellsong** (`src/shellsong.rs`, `src/ui/shellsong.rs`,
      `docs/shellsong.md`): a call-only command language on `:`.
      `play(crest)`, `queue([hex, 4 star crest])`, `queue_aa(...)`,
      `favorite(...)`, `add_to_queue(front, ...)`, `moveto(10%)`,
      `seek(+30s)`, and one `view(target)` that every screen-opening
      verb collapsed into (`view(queue)`, `view(suggestions)`,
      `view(q, bladee)` to search and open). resumable executor, since
      search is asynchronous and `play` must await results rather than
      act on stale ones. zsh-style completion: inline ghost text, tab
      cycling that only ever *extends* what was typed (fuzzy finds the
      candidates, then they are filtered to prefix extensions, so no
      completion can replace typed characters), history-ranked, and a
      type-driven split where enumerable parameters get insertable
      candidates while value parameters get a non-insertable hint
      instead (`volume(` offers `0-100, or ±N%`, never a guess).
- [x] **suggestion engine** (`src/suggest.rs`, `docs/suggestions.md`):
      candidate generation, then scoring, then diversification, over
      the play history already in `cache.rs`. nine generators covering
      the maintainer's own examples: daily rotation, album gap, track
      gap, album neighbour, genre under-exploration, rediscovery,
      starred-but-unplayed, similar artists, popular-unplayed. affinity
      is `distinct_days + ln(1+plays) + starred`, decayed on a 30-day
      half-life, normalised per generator before merging so raw
      magnitude cannot decide the result. diversification caps 3 per
      artist and 2 per album across the whole menu. every row carries a
      reason built from the real numbers ("4 of 5 tracks played from
      Eversince, never this one").
- [x] **suggestions menu** (`src/ui/suggestions.rs`, `R`): grouped
      sections with each row's reason printed under it. the reason is
      rendered, not just computed: a recommendation with no stated
      cause is indistinguishable from a random row, and the engine
      already knows exactly why it fired. so a suggestion is two rows,
      the selection plate spans both so they read as one unit, and the
      reason brightens only on the row you are on so every explanation
      does not compete with the titles. enter reuses
      `Action::Play`, so a suggested song plays and a suggested album
      opens its page with no second code path. the engine runs once on
      open rather than per frame (it walks the whole corpus and the
      play history). `cache::now_secs` became `pub` instead of adding
      a second clock read.
- [x] **suggestions: the three background fetches.** all nine
      generators can fire now. `spawn_background_enrichment` follows
      the `spawn_background_sync`/`spawn_background_search` shape: one
      job in flight at a time so a keypress costs one job, 8 artists
      and 12 albums per job, 4-way striped inside. the real bound is
      `album_targets`, which only offers albums with a played track
      and no known tracklist; everything else is `AlbumGap`'s job and
      needs no fetch. warming only runs while the menu is open, so the
      first open never blocks. artist answers persist to
      `~/.cache/wim/enrichment.toml` including the negatives, so a
      server with no last.fm metadata is asked once ever; tracklists
      are not persisted (bulky, and rebuilding a `SearchItem::Song`
      needs the `Arc<Client>`) and are instead kept in one long-lived
      `Catalog` across menu opens.
      `PopularUnplayed` needed a step nobody had flagged: song titles
      are never eagerly indexed, so `getTopSongs` ids resolved against
      an empty catalog and the generator would have stayed silent even
      with the fetch wired; the handler now ingests the response too.
- [x] **narrowed `suggest.rs`'s `allow(dead_code)`** from one
      module-level blanket to four per-item allows, each with a reason.
- [x] **`ingest_album` takes the album's credited artist.** it derived
      it from track one, so a compilation's tracklist landed on a
      different catalog slot than the corpus row it was fetched for:
      that row kept `track_total: None`, was re-offered every cycle,
      and the fetched data sat where nothing read it. `AlbumTarget`
      carries the artist through so the reply lands where it was asked
      from. an explicit `asked` flag on the album replaces "no track
      total means never asked", which also closes the failure case:
      the fetch now always reports, with empty songs on failure, and
      `mark_album_asked` records the attempt. the host's
      session-scoped `asked_albums` workaround is deleted, and its
      test now drives the real flag instead of the workaround.
- [x] **one model build per enrichment batch.** `enrichment_plan`
      builds the affinity model once and answers both target
      questions; asking `enrichment_targets` and `album_targets`
      separately walked every track twice. `suggest` is deliberately
      left out of it: the menu asks for suggestions and the scheduler
      asks for targets, never at the same moment, so folding it in
      would compute a result nobody reads. the two single-answer
      wrappers are gone rather than left as dead public API.
- [x] **the suggestions menu and queue view pulse at the display
      rate.** `selection_pulsing` only counted the browse view for
      `Focus::Workspace`, so both of those idled at the 100ms poll and
      animated their selection plate at a tenth of the display rate.
- [x] **the navidrome step has no progress rail.** done, and it was
      more than the rail: step 3 is `ui::login::LoginView`, which drew
      top-aligned behind a `draw_header` with no frame, so the flow
      visibly changed shape in the middle. the chrome is extracted out
      of `Screen::render` into `ui::onboarding::chrome(buf, area, step)`
      and login borrows it, which also removes the duplicate rail math.
      `LoginView::step` is `Option<usize>`: `Some` only from the
      onboarding render site (via `NAVIDROME_STEP`), `None` from the
      profile manager's add form and the standalone reopen, where
      "step 3 of 6" would be a lie. both are pinned by tests, the
      standalone one asserting the absence of a frame and rail so the
      shared path cannot be changed by accident.

- [x] **shellsong's `eq(<preset>)` cannot reach saved curves.** fixed,
      though not the way this entry proposed: `Op::SetEqPreset` carries
      the **name**, not a `Source`. a `Source` cannot be built at
      lowering time, because `Program::parse` has no host context and
      the saved curves only exist at runtime; carrying the name defers
      resolution to apply time, where `eq::resolve` reads built-ins
      then saved (the precedence the eq menu shows), and a curve saved
      after the line was typed still resolves.
      the parameter went from `Ty::Words(Words::EqPreset)` to a new
      `Ty::EqName`, since a `Words` set is static by definition.
      completion merges the built-ins with `Candidates::eq_presets()`,
      the same host-provides-runtime-names pattern `SavedQueues`
      already uses for `load_queue`. `eq::index_of` is deleted; it was
      the built-ins-only resolver this bug was made of.
      **the thing worth not losing:** parse-time validation used to
      give `eq(rok)` -> "did you mean 'rock'" and `eq(disco)` -> the
      list of real names, for free. moving resolution to apply time
      would have degraded that to a bare failure, so the message moved
      with it, as `eq::resolve_failure` (same jaro-winkler floor
      shellsong's own `did_you_mean` uses, and it suggests saved curves
      too).
      a missing argument is still a parse error; that needs no host
      context to decide.
- [x] **`!foo` does not exclude anything.** fixed by handling it
      ourselves instead of handing it to nucleo, which parses `!` and
      then ignores it in `Pattern::score`. `split_exclusions` peels
      `!term` tokens off the query, the positive remainder is what
      nucleo sees, and `Needle::excludes` drops a match before it is
      scored (an excluded item cannot be rescued by any score, so that
      is the cheapest rejection point).
      the pinned guard table proved the old bug outright: `!blade`'s
      pinned row was byte-identical to `'blade`'s, hash and all.
      exactly one row of 38 changed, which is the check that says
      nothing else moved.
      decisions: a bare `!` stays literal text (no term to exclude);
      an exclusion-only query ranks nothing rather than the whole
      corpus minus the term, matching how an empty query behaves;
      matching is substring, not word-boundary, since a boundary rule
      would keep "(Live)" only by luck of the punctuation.
      the oracle test now applies the same exclusion to its reference
      scorer, since `!term` is a filter over the corpus rather than
      part of scoring; that the exclusion itself is right is what the
      seven new tests cover.
      also: the app had quietly supported `'foo`, `^foo` and `foo$`
      all along and said so nowhere, so the help overlay has a "search
      syntax" block now instead of leaving it folklore.
- [ ] **the remaining search cost is the result clone, and it needs an
      api change to fix.** for a broad query at 40k items, 3-5ms of a
      ~7ms search is `rank` cloning ~15k `SearchItem`s. cutting it
      means not materializing rows nobody scrolled to, which cannot be
      made invisible while `App::results` is a `Vec<SearchItem>` that
      `main.rs` reads directly for drawing, `G`, the result counter,
      shellsong's `Target::Index` and `resync_results`'s identity scan.
      doing it properly is `App` owning the ranked index list and
      materializing a growing prefix with those five call sites behind
      accessors; that is the `main.rs` decomposition item's lane. no
      cap was added, deliberately, since any cap is visible at all five
      sites.
- [ ] **album/artist identity is name-based.** `SearchItem` carries no
      `album_id`/`artist_id`, so the suggestion catalog joins on
      trimmed-lowercased names. two same-named albums by one artist
      collapse, and a split-credit artist ("Bladee & Ecco2k") rolls up
      under neither. widening `SearchItem` touches every construction
      site, so it was left alone deliberately; revisit if the menu's
      suggestions look wrong in practice.

- [x] **maximized player: cover art and an arrow-selectable transport.**
      `ui::player::layout` is a `browse::layout`-shaped free function
      returning the art rect plus a row per element, so main.rs can
      overlay a real image on exactly the rect the widget used. art is
      square-first (height always re-derived from the surviving width),
      centred above the title, dropped entirely below 6 rows or 14
      cols, and the whole stack degrades through seven row tiers down
      to title-only. the layout also stops above the level meter's two
      rows, which the old view could draw underneath.
      selection reuses `controlsbar::Field`/`Scrub`/the glyph
      alphabet rather than copying them, and the pure key-to-cursor
      state machine (`nav`, `NavKeys`) lives in `controlsbar.rs`
      because both surfaces call it; a copy in `player.rs` would have
      been the second selection model. one deliberate divergence: the
      bar owns h/l and swallows up/down, the player is arrows-only so
      `l` still reaches cycle-loop. labels come in three width tiers
      (glyph + name + value, glyph + name, bare glyph) because an
      80-column terminal minus the sidebar is ~50-60 cells and
      all-or-nothing words would have lost the names in the common
      case.
- [x] **now-playing cover art for tracks outside an open browse view**
      (`src/coverart.rs`). fetched on its own thread and handed back
      over a channel, so nothing on the render or input path blocks on
      the network. keyed per *album* rather than per track, since every
      track on one album shares a cover; ten tracks were otherwise ten
      round trips for one image. `getCoverArt` takes a song id, so this
      needed no album id and sidesteps the name-based-identity item
      below entirely for the fetch. negative answers are cached, so a
      cover the server does not have is not re-requested every frame
      for as long as the track plays, and the cache is bounded at 24
      decoded images (a few MB each) evicting oldest-first. the open
      view's own art still wins when the playing track belongs to it:
      no fetch needed and it is already decoded at the right size.
      eleven tests over the key derivation, in-flight dedup, negative
      caching and the bound.
- [x] **cover art in `ArtMode::Real`.** the player has its own
      `StatefulProtocol` now, keyed by `coverart::art_key` so it is
      rebuilt only when the cover actually changes (a protocol holds
      resize state; rebuilding per frame would throw it away). this
      was not merely missing: the player was reaching for
      `real_art_protocol`, which is built *only* from a browse view's
      image, so with a graphics protocol configured a search-played
      track drew nothing, and if a view had been open earlier it drew
      **that** album's cover instead. the resolution of which image to
      draw also moved out of the draw closure, so the ascii widget and
      the real overlay are guaranteed to be handed the same one.
- [x] **the chrome widgets go through `ui::draw`**, and the overlays
      own their rect. eight widgets migrated (eq_menu, keybinds_menu,
      settings_menu, nav_manager, login, onboarding, subtitle_menu,
      banner's menu row; the figlet logo grid stays per-char by
      construction). five latent bugs fell out: a **missing top-edge
      bounds guard** in eq_menu/keybinds_menu/settings_menu/
      nav_manager/banner (only the bottom was checked), char-count
      centring in onboarding and banner, char-count-derived x offsets
      in login's three-colour url row, banner's key column padded by
      `len()` (bytes) inside a char-width format spec, and
      subtitle_menu having **no bounds guard at all**, running to the
      buffer edge over whatever the popup sat on, plus a char-count
      right-alignment that underflowed for an over-long value.
      Separately, the maintainer's "the settings menu has no border
      and its fucky" turned out to be systemic: five of six overlays
      never cleared their rect, so every cell they did not write still
      held the sidebar or browse list underneath. `draw::clear`,
      `frame`, `framed_inner` and `overlay` fix that in one place;
      each overlay gets a bleed-through test that lays down a sentinel
      char *and* background and proves neither survives.
      `help.rs`'s `MIN_COLUMN_WIDTH` went 40 to 38: the frame costs
      four columns, which dropped a 120-cell terminal to two help
      columns and reintroduced the sections-falling-off-the-bottom
      failure. 38 is the widest line that table actually produces, and
      the regression test still passes at 80x24.
- [x] **`notify.rs` goes through `ui::draw`**, the last per-char text
      writer in the app. Three width bugs, and the interesting one was
      not the draw loop: the box sized its interior with
      `chars().count()`, so a cjk track label in a message got a box
      too narrow for it, and because the text writer budgets correctly
      the symptom was not an overrun but a **silently truncated
      message** with plenty of terminal left. The footer (the
      countdown baked into the bottom border) was also interleaved per
      cell with the border dashes, which a multi-cell glyph would
      desync; it is drawn as a full rule and overwritten now. The box
      also clears its own three rows, since it is painted over the
      sidebar and main content and an interior cell left unwritten
      showed through (the box can be wider than its text, see the
      `max(4)`).
- [ ] **decompose main.rs** (~3.9k lines). a reviewed, ordered plan
      exists: extract focus/InputMode, then the playback free functions,
      then the sync/channel plumbing, then a `struct Ui` for the ~35
      loose locals, then the draw closure, and only then the input
      tiers (their ordering is load-bearing and heavily commented).
- [x] **the render path no longer polls mpv.** `time-pos`, `duration`
      and `pause` are observed properties now, folded into
      `mpv::Playback` as change events arrive on the channel the loop
      already drains. that removes the last three blocking ipc round
      trips per frame; `playback_info` is deleted rather than kept
      around. a null value clears the field instead of leaving a stale
      reading, which is what happens when the playlist drains (a stale
      position would keep the scrubber crawling after the music
      stopped). the one remaining per-frame call is `audio_level`,
      which is an inherently per-frame sample of the astats tap.
      `cell_aspect`'s ioctl also went from five calls per frame to one,
      since the answer cannot change inside a frame. one deliberate
      exception: shellsong's percent seek still asks for `duration`
      directly, because it runs once per command and must resolve
      against the duration as of now rather than the last pushed
      change.
      **correction (perf/mpv):** the claim above that `audio_level` is
      "inherently a per-frame sample" was wrong. `af-metadata` IS
      observable (measured at 20 pushes/s with real data), so it is an
      observed property now like the rest. worse, the old per-frame
      round trip bought nothing at all: it asked for a property path
      that does not resolve, and mpv hands af-metadata through as
      strings so `as_f64()` would have read every reading as absent
      anyway. the level meter had never worked.