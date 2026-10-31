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
  `src/theme.rs` helpers, never a hue, anywhere. **album art is the one
  exception, authorised by the maintainer directly** ("i want FOUR ART
  OPTIONS: ASCII BW, ASCII COLOR, FULL BW, FULL COLOR"): `ascii-color`
  rasterizes the cover in its own rgb via `raster::Tone::Color`, and
  `full-color` is the graphics protocol's own true color. do not "fix"
  that back out. everything that is not album art stays monochrome, and
  `ascii-bw`/`full-bw` stay grey; tests in `src/ui/raster.rs`,
  `src/ui/player.rs` and `src/browse.rs` pin both halves of that
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
- [x] album art (`src/ui/albumart.rs`, `src/ui/raster.rs`,
      `ratatui-image`): fetched via `getCoverArt` when an album opens.
      four modes plus off, cycled with `i` and set by
      `config.album_art`: `ascii-bw`/`ascii-color` are wim's own
      quadrant-block rasterizer (resampled fresh at the panel's current
      size every render, so a terminal resize re-rasterizes for free,
      and 2x the horizontal detail of a half-block renderer), grey or
      in the cover's own rgb; `full-bw`/`full-color` are a real inline
      graphics protocol (sixel/kitty/iterm2) through `ratatui-image`,
      the bw one greyscaling the image before it is encoded. `auto`
      resolves to `full-color` where a protocol exists and
      `ascii-color` where none does. a `full-*` mode on a terminal with
      no protocol falls back to the matching ascii mode and says so
      once; `ratatui-image`'s own `Halfblocks` fallback is never used
      (it is coloured half-blocks at half the rasterizer's resolution,
      which is exactly the "ascii but with colour" confusion this
      replaced).
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

- [x] **video works: mpv draws it, fullscreen, and `tct` works in any
      terminal including alacritty.** `src/video.rs` plus a
      `video_output = "auto" | "tct" | "kitty" | "off"` config.
      `video::wanted` is the whole decision: `auto` picks kitty only
      when the probe reported `ProtocolType::Kitty`, else `tct`.
      four things found by measuring rather than assuming, each of
      which would have shipped broken:
      **mpv's stdout was `Stdio::null()`**, so a drawing vo had nowhere
      to write at all; it is `inherit()` now, and with `--vo=null` mpv
      writes *zero bytes* across a load, a seek and a volume change, so
      it costs nothing for music.
      **`vo` is settable at runtime**, verified mid-playback with
      `time-pos` still advancing, so no relaunch and no seek-restore is
      needed. but `set_property vo bogusvo` replies `"error":"success"`
      and then silently never draws, so only a `video::Vo` variant is
      ever handed over, never a config string.
      **`--no-terminal` breaks vo auto-sizing**: mpv never queries the
      tty and falls back to 80x25, so wim pushes the geometry itself.
      mpv reads it only at vo init, so a resize has to bounce `vo`
      through `null`.
      **mpv's alt-screen escapes are not balanced around a vo.** `set
      vo tct` emits `?1049h ?2J ?25l ?1003h`, `set vo null` emits only
      `?25h ?1003l`, and the matching `?1049l` arrives at mpv's
      *process exit*. `Mpv`'s drop normally ran after the terminal
      guard's, so that escape landed on the user's shell and restored a
      cursor position the shell never saved; `Mpv` is dropped before
      the guard when a vo was up.
      six guards were mutation-checked individually.
- [ ] **in-workspace (sub-rect) video.** detailed in docs/video.md. the
      real blocker is not mpv: ratatui owns a rectangle of cells and
      diffs all of them with no concept of a hole, so both the render
      path and the diff have to learn to skip a rect. beyond that,
      `kitty` has `--vo-kitty-left/top` and `alt-screen=no` so it can
      draw at an offset, but `tct` has neither and always paints from
      the top-left of the alt screen it just entered; both emit
      `ESC[2J` at init and neither can be told not to; and init-only
      geometry makes every pane resize a vo bounce. realistic shape is
      kitty-only sub-rect video with tct staying fullscreen.
- [x] **BLOCKER (for the pull-frames-ourselves route only): there is no
      working way to get frames out of mpv.** -- **closed as obsolete.**
      it gated a route that no longer exists: `FramePump` is deleted
      (`fe6a687`) and mpv draws video with its own `tct`/`kitty` vo, so
      nothing in wim wants a frame. the measurements below are kept
      because they are the reason, and because anyone tempted by
      `screenshot-raw` over ipc should read them first. the last
      sentence of it is now wrong: `FramePump`'s public surface cannot
      be a swap point for a replacement source, because there is no
      `FramePump`.
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
**the blocker on these is lifted; a frame source exists.** the old
conclusion generalised one true fact into a false one. the true fact,
re-measured on mpv 0.41.0 and still true: `screenshot-raw` cannot come
over json ipc, because mpv emits a literal `"data":}` and no format or
flag changes that (`bgr0`/`bgra` truncate the same way, `rgba` answers
"error running command" under `--vo=null`, `jpg` is "invalid
parameter"; the documented `format` argument is a *pixel* format, not
an encoding). the false generalisation was "therefore there is no
frame source". three working sources are measured in docs/video.md,
and `examples/frame_probe.rs` pulls real pre-scaled rgb24 frames out
of a second headless mpv at 38% of one core for 1080p.
**the decisive thing every earlier measurement left out is
`--vf=scale`**: mpv does the downscale, which is the expensive half,
which is why every per-frame number in the old notes was an order of
magnitude too pessimistic (113-396ms unscaled became 35-41ms, and the
fifo path 144-290ms to *first* frame).
two negatives worth keeping. the tempting single-process answer
(`set_property vo image` at runtime, which mpv does accept mid-
playback exactly like the existing `tct` flip) is disqualified by
**pacing**, not cost: `vo_image` is an untimed vo, so against an
identical realtime-emulating null ao, `vo=null` advances `time-pos`
at 1.00x and `vo=image` at 0.44x. that is the user's audio at half
speed. and libmpv's software render api does work (proven:
`mpv_render_context_create` returns 0 and delivers a frame on the
second render) but it is a rewrite rather than a feature, since the
render context is only available to the process owning the
`mpv_handle` and wim spawns mpv and talks over a socket; it also needs
flake work the flake lacks (`pkg-config` is not in the dev shell and
mpv's dev output is not on the include path).
`video plays as real video` still needs someone at a real terminal,
and `in-workspace (sub-rect) video` is still kitty-only via
`video::Hole`, but **the ascii chain is now ordinary work.**

- [ ] **frame source: a second mpv, rawvideo on a fifo.** spawn a
      headless `--of=rawvideo --ovc=rawvideo --ovcopts=pix_fmt=rgb24`
      instance with `--vf=scale` to the cell grid plus a `pad` so every
      frame is the same size, and read exactly `w * h * 3` bytes per
      frame off a fifo.
      **the fifo is the clock**, which is the frame-budget item's
      requirement for free: mpv blocks in `write(2)` once the pipe
      fills, so wim's read rate *is* the decode rate, and pause is
      "stop reading". measured: after a 1.0s stall only 5 frames were
      waiting, not the 25 a free-running decoder would have made.
      sync is read scheduling, not seeking: frame `n` is
      `start + n / container-fps` (`container-fps` reads fine over the
      existing socket), catching up is read-and-discard, waiting is
      not reading, and there is no drift because the frame instance has
      no clock of its own. put the audible instance on `vid=no` while
      ascii mode is on (the `set_vid_for_item` machinery already
      exists) so there is only ever one video decode.
      costs: one extra process (134MB rss at 1080p, 74MB at 480p);
      38% of a core to decode 1080p h264 and scale to 320x192 at 24fps
      (36% to 160x96, so it is the decode not the scale; 10% for
      480p); and **~180-550ms of black on a seek**, because encode mode
      has no socket to seek over so the instance is restarted with
      `--start=`. no new nix dependency and `src/mpv.rs` does not
      change. prototype in `examples/frame_probe.rs`.
      one gotcha the implementation will hit, documented in the
      prototype: on a fifo with no writer attached yet, `read` returns
      `Ok(0)` (a clean eof), **not** `WouldBlock`. zero has to mean
      "wait"; child liveness, not the byte count, is what says it is
      over.
      two docs still carry the stale claim and are not this slice's to
      fix: `src/video.rs`'s module doc reads broader than it is now,
      and `src/mpv.rs` ~252 is stale the *other* way, calling
      `screenshot-raw` "the embedded video pane's own frame source"
      when it is unreachable over the socket wim uses (the clause
      about screenshots coming from the decoded/filtered frame is
      right, and is exactly why `vf=scale` works).

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

**nothing. all ten branches are merged as of 2026-10-31**: the five
perf ones, `feat/mouse`, `fix/async-open`, `feat/video`,
`feat/art-modes`, and the mouse audit's follow-up commits. 874 tests,
clippy `-D warnings` clean, fmt clean.

`feat/art-modes` was the one hard merge: 12 conflicts, because it was
based three merges back and both `fix/async-open` and `feat/video` had
rewritten the regions it touched. the resolutions, recorded because the
merge commit's own message lost three backticked words to shell
interpretation (write commit bodies through a file, not an inline
double-quoted string):

- art-modes replaced four scattered open-time protocol builds with one
  per-frame keyed rebuild. that wins, because it is the only shape that
  can see the `i` key change the art mode with **no view change at
  all**, which four separate open-time sites could never agree on. the
  open-time builds stay deleted.
- `real_art_protocol` stays on `ActionCtx` even so, because
  `open_browse` still clears it; without that the previous album's
  cover sits under the new title for a frame. the `picker` field does
  leave `ActionCtx`, since its only users were the builds now gone.
- every `invalidate()` from async-open is kept. dropping one would let
  a late album land in a view the loader does not own, which is the
  exact bug its generation counter exists to stop. all three call sites
  (esc/q, and both starred opens) and all four guard tests were
  re-verified after the merge, not assumed.
- the fullscreen ascii video pane art-modes was tinting is gone, since
  fullscreen video now means mpv draws and wim stops drawing entirely.
- `fetch_cover_art` is not restored to main.rs; it moved to browse.rs
  with the loader.

**the lesson from this batch**, since it cost real time twice: a branch
that merges master down regularly lands for free (`feat/mouse` merged
with zero conflicts after twelve commits), and one that does not pays
for every merge that happened meanwhile. merge agent branches as they
report, not in a batch at the end.

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
- [x] **the ui defect batch, and the char-versus-cell class is now at
      eight instances.** merged. the four recorded defects were all
      real; more usefully, the sweep found **four more char-versus-cell
      bugs and one more emoji glyph**, which is why that sweep was
      worth asking for rather than assuming the class was closed:
      - `lyrics_view` wrapped on chars, so a CJK song put up to twice
        the pane width on a row and `wrapped_len` under-counted, which
        put the end of the song outside the scroll clamp. lyrics are
        arbitrary utf-8 from lrclib, so this was live.
      - `header::junction_col` compared a **char index against a cell
        column**, so a wide glyph in a title moved the `T` junction a
        cell left per glyph and the two lines that must meet did not.
        live for any CJK library, since browse heads its pane with an
        album name.
      - `banner`'s logo box was sized by `chars().count()`, mis-centring
        the wordmark and putting the right arrow inside it.
      - a bonus defect the new invariant test turned up: the lyrics
        hard-split only ran when the over-long word was **first** on
        its row, so a long word after a flush overflowed on its own.
      the emoji one: `logos.rs` has a logo drawing circled capitals,
      and U+24C2 is `Emoji=Yes` with no non-emoji equivalent to swap
      in. since that file is generated, the fix was to filter the logo
      table to codepoints the art is actually built from (ascii, box
      drawing, block elements, braille), which drops exactly 2 of 178
      entries, and to make a config naming a dropped one fall back to
      the default rather than to whichever logo took its index.
      **the sharpest thing in the report, worth keeping:** the
      render-versus-map agreement test alone does *not* catch a
      mis-numbering, because the render and the map read the same wrong
      index and agree with each other. consistency and correctness need
      two different tests. that generalises well beyond this one case.
      two intended user-visible changes: 176 logo alternatives instead
      of 178, and the scrubber's track is a cell shorter for the first
      ten minutes of a song longer than ten minutes, which is the cost
      of reserving the readout width to kill the drag-jump class
      outright rather than handling the one crossing.

### shellsong stages 1-3 (merged 2026-10-31)

Variables, `.shg` scripts and per-identity settings are in. 940 tests.

- **variables.** `define(name, terms)` is canonical and `let name = [a,
  b]` is surface sugar parsed straight into it, so typechecking, errors
  and expansion are all the canonical call's and R1 stays literally
  true of every *lowered* program. Terms rather than resolved tracks,
  as briefed: a variable is a vibe that follows the library, and
  `save_queue` is already the frozen case. Expansion lives in exactly
  one place (`coerce`'s `Ty::Terms` arm), which is why every
  terms-taking command gets it for nothing and `dry` prints the
  expanded plan for free.
- **`.shg` scripts.** `run(script)` lowers a file's statements into the
  same `Program`, so there is one stepper, one await path and one
  `dry`. A bad line reports and the rest runs (`mix:2: col 1: unknown
  command ...`); a *runtime* failure still stops. `init.shg` runs at
  startup as literally `run(init)`.
  A test caught a real design gap: a script's `let` on line 1 was
  invisible to line 6, because every line parsed against one scope
  snapshot. Fixing it also made `let a = [x]; play(a)` work typed on
  one line.
- **per-identity settings.** `remember(tier, call)` / `forget(tier,
  setting?)`, where **the setting is a call**: no parallel vocabulary of
  setting names and value formats, the value goes through `eq`/
  `volume`/`speed`'s own parameter type, and the binding applies
  through the identical `Op` the typed command lowers to. A relative
  value is refused at parse time, since `remember(track, volume(+10%))`
  would bind "louder than whatever it happens to be".
  Precedence is **per setting**, track then album then artist, decided
  independently for eq, volume and speed; per-identity precedence would
  mean remembering an eq for one track silently dropped the volume its
  album remembered. Settings **restore** rather than staying applied,
  with the restore point captured only on the *first* override so a run
  of bound tracks cannot ratchet it.

**A real user-facing bug it found on the way:** `MAX_CANDIDATES = 50`
was being applied to the **command-name** list, which the table bounds
rather than the library. With more than 50 written names (there are 45
commands plus aliases), every name alphabetically past the cut was
**unreachable by tab from an empty token, including `view` and
`volume`**. Only corpus and user-grown lists are capped now, and
`every_command_name_is_reachable_by_tab_from_an_empty_token` fails if
any name ever becomes unreachable again.

### the queue timeline: two decisions taken so stage 4 can start

The agent stopped at a stage boundary green rather than half-landing
the timeline, and left a concrete handoff. Two things needed deciding;
both are decided here so the next agent does not stall on them.

**1. the entry is `silence(15m)`, not `pause(15m)`.** `pause()` is
already the play/pause toggle, and R2 explicitly refuses arity
overloading ("`skip(2)` is refused: it is ambiguous"). `silence`
describes what actually plays, cannot be confused with the toggle, and
pairs with `gap(10s)` for the default between tracks, which keeps the
maintainer's two ideas named distinctly.

**2. `silence` is its own command with an optional position, not a
nested call inside `queue`.** `queue`'s parameter is `Ty::Terms`, which
means "names of things in the library"; letting it accept a call form
is a genuine grammar extension and would make `Ty::Terms` mean "names
or calls" everywhere. So: `silence(15m)` appends and `silence(15m,
front)` takes the position parameter `add_to_queue` already uses. The
composition loss versus `queue(silence(15m))` is real but small, and it
is reversible later if the grammar extension turns out to be wanted
anyway.

**The invariant question is already answered, and the answer is the
good one.** Keep mpv's playlist as the source of truth and make a
silence a *real* mpv entry via `av://lavfi:anullsrc=d=N`, which mpv in
this setup already loads (two ignored benches prove it). Then
`entries.len() == mpv playlist len` still holds, every absolute-index
path keeps working untouched, and the desync regression tests at
`playqueue.rs:296-393` keep holding. The invariant's doc comment gets
*stronger*: "`entries` is mpv's playlist, and every entry is something
mpv loaded, a generated silence source included".

- [x] **two latent bugs that must be fixed as part of the timeline**,
      both found during that survey and both currently unreachable only
      by luck:
      - ~~`load_playlist` and `resync_upcoming` skip an item whose
        `resolve_playable` returns `None` when appending to mpv but
        still store the full vec in `PlayQueue`~~ **fixed.** both now
        go through `core::resolve_all`, which returns each kept item
        **paired with the path mpv is handed** plus the ones that did
        not resolve, so storing a list mpv did not get is no longer
        expressible rather than merely avoided. `load_playlist` stores
        only what mpv took, counting index 0 as taken because
        `try_play` loaded it, and an `is_ok()` on each append, so a
        *refused* append cannot desync either (it was `let _ =`).
        the status line gains " (n skipped)" and nothing at all when
        none were, since otherwise every ordinary play would end in
        "(0 skipped)"; pinned by a test, as is order preservation,
        which the length fix could otherwise have traded for a
        different mapping bug.
        **a worse sibling turned up next door**: `resync_upcoming`
        returned early when an append failed, *after* `playlist_clear`
        had already emptied mpv and before touching `PlayQueue`, so a
        mid-shuffle failure left mpv empty and `entries` full. every
        index wrong, not just the ones after a gap. it updates the
        queue on the partial-append path too now, which is the only
        answer that keeps the invariant.
        `playqueue.rs`'s invariant doc now also states the rule in the
        form that catches this: **every entry is something mpv
        accepted**, not merely "same length".
        still wants a human or an `--ignored` mpv test for the full
        path; the pure seam is covered, the ipc pairing is not.
      - ~~`queues.rs`'s `Entry` has no discriminant~~ **fixed.**
        `Entry::kind` is explicit now and written on save, optional on
        load, with `Entry::kind()` falling back to exactly the old rule
        (a path means local, a server plus id means remote) so every
        queue saved before this loads unchanged; that compatibility is
        pinned by a test against toml the old code actually wrote.
        an entry that is neither comes back as
        `Dropped::Unrecognised` rather than vanishing.
        **this turned up a present-day bug while in there, not just a
        latent one:** `load`'s `filter_map` dropped every unresolvable
        entry silently, so a fifty-track queue whose server is no
        longer configured came back as twelve tracks and said nothing
        about the other thirty-eight. `load_report` returns what it
        could not rebuild and why (moved file, server gone,
        unrecognised), `load` delegates to it so nothing changed for
        existing callers, and all four reasons fail their tests if the
        silent drop is restored.
        **done:** `load_queue` calls `load_report` now and says what it
        lost. one loss names itself, since the title is the useful part
        when there is only one; several group by reason and count,
        because forty titles is not a status line and a bare "38
        missing" with no reason is most of the way back to the silent
        shortening. a queue that loads nothing distinguishes "it is
        empty" from "nothing in it loaded: 38 with no configured
        server".

### shellswim, first real use (maintainer, driving it live)

- [x] **the bottom bar was wim's `ControlsBar`, which was never
      asked for.** the spec was the title and the scrubber; reusing
      wim's bar brought pause/loop/shuffle/speed/eq/crossfade glyphs
      and a queue count with it. `swimconsole::SwimBar` replaces it:
      title left of centre, scrubber right, **exactly half each** so
      the scrubber never shifts as titles change length, and a title
      that would reach it is ellipsised with two cells of clearance.
      above it a `HorizontalRule`, then space / bar / space.
      a test asserts none of `▸ ‖ ↻ ⇄` appear. removing them made
      `Screen`'s `app` and `crossfade` fields dead, which is the
      confirmation they only ever fed glyphs.
      **one judgement call**: a running `gap` takes the title's half
      (`gap · 7s`), because the track whose name would be there has
      ended and a full scrubber on a paused player explains nothing.
      the `+N` queue count is gone with the rest; `view(queue)` has it.
      two of the new tests were wrong first: one compared against
      `trim()` where testbuf fills untouched cells with `.`, and one
      used a *byte* offset to find a column, which an ellipsised title
      breaks because `…` is three bytes to one cell -- the same trap
      the test it replaced had a comment about.
- [x] **`search()` answered with a count and nothing else**, which
      with no sidebar is no answer. `Surface::show_results` is the
      hook: wim ignores it (its sidebar *is* the answer), shellswim
      prints the list once the search settles.
      **asked for through the surface rather than inferred by the
      host**, which cannot tell `search(x)` from the query a searching
      `play(x)` sets on its way to a match, and would have echoed a
      list after both.
      `ui::textfit::columns` is the layout: up to three columns,
      column-major like `ls` so reading *down* keeps the ranking, each
      item ellipsised rather than wrapped so one item is always one
      row. capped at 30 results with `+N more`, since the point was
      not flooding the terminal.
      **each column is sized to its own contents**, which is the
      algorithm rather than a detail of it. the first cut divided the
      width evenly, which is wrong in both directions: a list of
      eight-cell names wasted two thirds of every column, and long
      titles got chopped at the same arbitrary place whether or not
      there was room. it now tries three columns, then two, then one,
      and takes the first whose *actual* widths fit -- so six 6-cell
      names pack into a 24-cell terminal (22 used) where an even
      division would have refused, and a 72-cell terminal truncates
      nothing where it used to cut at 22.
      only when nothing fits untruncated does it share the width out,
      and then only while each column stays >= 18 cells, so a narrow
      terminal with long titles drops to one column instead of three
      unreadable slivers. the even-division behaviour had a test
      asserting it; that test now asserts the packing instead, which
      is how the improvement was confirmed rather than assumed.
      deliberately **unnumbered**: no `Target` is addressable by
      result index from the language (`Target::Index` is only ever
      produced internally by `best_match`), so a numbered list would
      advertise a `play(2)` that does not exist. bare `primary_name`s
      rather than the decorated label, because three columns of a
      72-cell terminal leave ~22 cells each, which truncates
      "title · artist (album)" to exactly the useless half, and a bare
      name is what you would type back into `play(...)`.
- [x] **`search()` in shellswim printed nothing for a query whose
      matches land after the settle window.** the plumbing was already
      most of the way there and the note above misread it:
      `settle.answered()` is called when a remote outcome arrives, so
      `settled()` is already true as soon as a server replies. the
      1200ms `SETTLE_WINDOW` only bites when a server is *slower* than
      that, and then the echo had already gone out with the local
      matches alone and the late batch enriched the corpus silently.
      so no tagging of `RemoteSearchOutcome` was needed, which the
      note had guessed at. the host remembers **the query echoed and
      the names printed for it**, and a late answer prints only what
      is new, under "...and from the servers:".
      **by name, not by index**, and that is the whole reason it is a
      set: a late answer is merged into the corpus and the results are
      then *re-ranked*, so the new matches are not a tail that can be
      printed by offset -- a name can move anywhere in the list.
      diffing by name is correct under any reordering and cannot
      reprint what was already shown. the 30-result cap spans both
      batches, so a late arrival cannot push the total past it.
      pinned: a second batch prints only its new names, a third prints
      nothing, and the cap is spent across batches.

### live-use backlog (maintainer, driving shellswim 2026-11-01)

Asked for while testing, in the order they were asked. Everything
above this line in the shellswim section is done.

- [x] **scrollback in shellswim.** a `scroll` offset on
      `shellsong::Console` (0 = the live tail), **PageUp/PageDown** for
      a page and **shift+Up/shift+Down** for a line. plain Up/Down stay
      history recall and a test pins that, since taking them would
      break the one thing a prompt must do. handled in `handle_key`,
      which does see `KeyModifiers` -- the note here guessed it did
      not.
      **the offset clamps against the scrollback viewport, which only
      the renderer knows**: the height left after the prompt and the
      error/cycle/info rows varies per frame. the view reports it back
      through a `Cell<usize>` on `Console`, which is the contained way
      to do it when the widget is handed `&Console`; clamping against a
      guessed height is how a scrollback ends up banking keypresses
      that then have to be undone.
      **arriving output holds the line, not the offset.** a
      scrolled-back console gets `scroll += 1` per pushed line, so
      reading back while a program prints is actually possible; at the
      tail there is nothing to hold and it follows. a submitted line
      always snaps, since you pressed enter to see what it did. buffer
      trimming decrements the offset, or one parked on dropped lines
      would point at unrelated text -- pinned. a page keeps one row of
      overlap so the line being read stays on screen to orient by.
      **printing it caught what the tests did not**: the notice
      ellipsised mid-hint at 46 cells, so it is "N back · pgdn to catch
      up". it exists at all because a scrolled console that said
      nothing reads as a *frozen* one -- output arrives, the screen
      does not move, nothing explains why -- and it claims a row only
      while it is true.
      wim's own console (`ui::shellsong::ShellsongConsole`) has the
      state for free now, since it lives on `Console`; what it still
      needs is the key handling and the same notice row.
- [x] **`queue()` decides between a song and an album dynamically**,
      with `k=song`/`k=album` as the assertions.
      the mechanism changed rather than gaining a rung, as the note
      above predicted. `Pick::Best` takes the acceptable result
      *nearest the front* of the list, and that is exactly "whichever
      kind scored better" because `Corpus::search` sorts by score and
      *then* groups categories, keeping each category's first
      appearance. so the dynamic default is one `position` call.
      `Pick::InOrder` keeps the old walk for **`view`**, which is the
      one command whose preference list is a real order: `view(q,
      bladee)` must open the artist's page even when a song scored
      higher. gated on the same `widenable()` predicate `kind=` is,
      for the same reason -- the rungs widen *from songs* and `view`
      does not start there.
      **`kind=` rungs became filter sets**: `song` is `{Song}`,
      `album` is `{Album}`, and neither falls back, because an
      assertion that quietly accepts the other kind is not one. a
      `k=album` with no album match now reports that.
      `_aa` still desugars to `kind=album` so scripts parse, but its
      name ("allow album") no longer describes it -- albums are
      allowed by default and the suffix asserts. kept rather than
      removed, since dropping a documented suffix breaks scripts
      silently; docs point at `k=a` instead.
      **eight tests pinned the old semantics and were rewritten, one
      found a real bug.** `add_to_queue(back, x)` must lower
      identically to `queue(x)`, and a test comparing the two
      lowerings caught them diverging when only `queue` was changed --
      the kind set had to move on both. the kind-preference test has
      now been inverted twice (song-preferring, then
      album-preferring, now score-deciding); both earlier versions
      were approximations of this, and the comment says so rather than
      pretending the first was wrong in isolation.
      r7 and the `play`/`queue` asymmetry note are rewritten, not
      deleted: the bare command still *resolves* ambiguity, which is
      what r7 was about, and `kind=` filters, which is what it denied.
      `SONG` fell out of the command table entirely -- nothing is
      song-only any more, since that is what `k=song` is for.
- [x] **shellswim never handled `idle-active`** -- the other half of
      the `skip()` report. its event drain watched `PROP_PLAYLIST_POS`
      and nothing else, so when mpv's playlist genuinely ran out the
      queue still thought the last track was playing: the bar kept its
      name and `now_playing` never cleared.
      `core::drained` is the shared answer, and it is shared because it
      is a *decision about the queue* rather than a piece of either
      front-end: "loop all" replaying from the top is wim's own
      bookkeeping (`PlayQueue::session`), mpv having no concept of the
      session that was built. it returns `Stale`/`Cleared`/
      `Restarted(track)`/`Failed`, so each front-end runs the hooks it
      actually has instead of one of them pretending to have the
      other's.
      it takes its four pieces rather than a `CoreCtx`, for two
      reasons that both turned up while writing it: wim's call site
      sits *above* where its `core_ctx!` is defined, and the decision
      is then testable without spawning mpv.
      **the stale-report guard lives inside the shared function**, not
      at the call sites. that is not tidiness: a guard every caller
      has to remember is one a caller will forget, which is precisely
      how shellswim ended up with no end-of-playlist handling at all.
      `drain_decision` is split out from the ipc so all of it is
      testable; only reloading the playlist for a restart needs mpv.
      pinned including `LoopMode::One`, which does *not* replay a queue
      (it is mpv's own `loop-file`), and loop-all with an empty session
      clearing rather than restarting from nothing.
- [x] **track-change hooks are at parity now: `core::track_started`.**
      three paths started a track and fired different hooks. wim's
      advance and its loop-all restart fired all five (subtitle fetch,
      playback note, scrobble tracking, per-identity settings, the
      volume ramp); **`core::start_session` -- which is what `play()`
      and `Action::Play` go through -- fired three and not
      per-identity settings or the ramp**, so a bound eq applied when
      a track arrived by advancing and silently did nothing when you
      asked for it by name. shellswim fired none of them at all.
      all four sites call `track_started` now. it returns the
      per-identity ops rather than applying them, because they *are*
      ops: pushing them through the same `apply_shellsong_op` path is
      what keeps a bound eq indistinguishable from a typed one, and
      the pending-program slot differs between the hosts.
      `Surface::queue_program` carries them, defaulting to dropping
      them for a front-end with no step loop.
      `Overridden` moved from a main.rs local into `CoreCtx`, since
      the hook is shared; shellswim gained one, having never had any.
      `set_vid_for_item` deliberately stayed at the call site: it is
      about what mpv decodes when the file loads, not a consequence of
      playback starting.
      **the test was wrong on its first draft and the mutation check
      caught it.** it called `track_started` directly, so gutting the
      call inside `start_session` left it passing -- it pinned the
      function, not the wiring. it goes through `start_session` with a
      capturing `Surface` now, and fails when that call is removed.
      wim's `core_ctx!`/`wim_surface!` macros moved to just after
      `now_playing` is bound, with `shellsong_now` hoisted to join
      them: a `macro_rules!` is only in scope after its definition,
      and the event drain above them wants them too. this is the
      second time that ordering has forced a workaround, so it is
      fixed rather than worked around again.


### the overnight performance run (2026-11-01)

Eight agents in parallel, maintainer away, brief was "performance
first, then bug fixing, then polish". Measured numbers throughout,
because the standing rule on this is **alternate batches of the two
implementations in one process**: sequential before/after runs on this
box came out 1.6x apart while other builds were running, which is not
a measurement.

- [x] **the album art rasterizer is 2-4x faster, and it was 72% of a
      frame.** `paint` is the half the memo cannot skip, so it was all
      of that. four changes: hoist the tone branch out of the per-cell
      loop and row-slice the grid; replace **8 hardware divides per
      colour cell** with exact reciprocal multiplies, two of them
      feeding values that were then thrown away; pre-encode
      `QUADRANTS` as `&str` for `set_symbol`, the glyph write being a
      *third* of the per-cell cost; and sum per output sample into a
      register instead of read-modify-writing a `Vec` slot per tap.
      21.5 -> 13.5 ns/cell mono, 52 -> 23.5 colour; the whole 200x60
      frame 126 -> 49us.
      **two negative results, reported rather than buried**: reading
      rows as `[u8;3]` chunks to kill per-tap bounds checks measured
      0.87-1.01x, so the bounds check was never the cost; and
      `div_taps` alone is 0.99-1.05x, because llvm already hoists a
      loop-invariant divisor, so it is kept only for landing in the
      dependency chain once the sum moves into the same loop.
      every test compares against a **verbatim copy of the old code**,
      the pattern `legacy_raster_cost` already used: full `Buffer`
      equality (glyph and both colours) over 4 sources x 2 tones x 2
      fits x 7 rects, 200k pseudo-random quads, and both reciprocal
      tables exhaustively. the grid-clip mutation survived the first
      version of that equality test, which is why the oversized-rect
      case is in it.
      what is left: ~9 of the 13.5 ns is `Cell::set_symbol` building
      and dropping a `CompactString`, a ratatui floor. `fill` is ~2
      ns/tap and only runs on resample (track change, every frame of a
      resize drag, every video frame); const-generic specialisation on
      `taps_x` is the diagnosed next win and was not done.
- [x] **text is measured by grapheme cluster now, and the fast paths
      are the whole story.** the repo had **two width models**:
      `textfit::width` used `UnicodeWidthStr` (string-level ligature
      rules) while `char_cells` used `UnicodeWidthChar` (none), so
      every truncation mixed them, and `draw` advanced by
      `cells.max(1)`, giving a zero-width codepoint its own cell.
      measured in a real `Buffer` against a border at column 8: an
      emoji with a variation selector returned x=9 and **overwrote the
      border**, a zwj family painted three separate faces, NFD
      `Beyoncé` measured 8 for a 7-cell string, and decomposed hangul
      painted as loose jamo. NFD is how macOS and APFS store
      filenames, so this is ordinary library content, not a corner
      case. the doc contract "never wider than asked for" was simply
      false.
      **the first grapheme pass cost 5x on the text path** (12.6 ->
      96.2us per frame, i.e. 3x what the art panel costs), caught by
      the rasterizer agent isolating it rather than by anyone noticing
      later. a `plain_ascii` gate and a `one_cell_chars` gate bring it
      to **1.00x whole-frame parity**, each with an equivalence test
      against the grapheme walk. the ascii-only gate was not enough on
      its own: wim's own chrome (`·`, `…`, `─`, `▸`) is non-ascii, so
      it missed the title and every rule.
      the fits cannot slice on the one-cell gate, because a char
      boundary there can cut a flag's two regional indicators in half;
      `the_ascii_fast_path_agrees_with_the_grapheme_walk` caught
      exactly that. an NFD title stays ~9x, which is **text HEAD
      measured wrongly**; its cheap answer was the bug. and a
      hand-rolled byte scan was *slower* than the walk, because the
      crate builds at `opt-level = 0` while dependencies build at 2,
      so every gate is a library call; that is in
      docs/text-measurement.md so nobody simplifies it back.
      **the audit's "in-repo example of the right thing" was itself
      broken.** it named `swimconsole`'s ghost guard,
      `cursor_col != width(&window)`, which is never satisfied when
      the language auto-closes a paren, so **shellswim never drew a
      ghost for any parenthesised call at all**. both consoles now use
      the rule that is actually load-bearing.
      a fifth hand-rolled per-char writer with the same `.max(1)`
      exists at `ui/controlsbar.rs` ~715 and was not in the audit.
- [x] **downloads are capped, pooled and timed out.**
      `download_tracks` spawned **one thread per track with no cap**,
      so the download key on an 800-track artist meant 800 threads and
      800 simultaneous connections, most failing into a storm of
      toasts, while every other batch network op in the tree caps.
      `DOWNLOAD_CONCURRENCY = 4` via the shared-`AtomicUsize` pattern
      rather than a fixed stripe, because track sizes vary by an order
      of magnitude and a stripe idles whoever drew the short ones.
      `timeout_connect` 10s plus `timeout_read` 30s and **deliberately
      no overall timeout**: a long flac on a slow link is legitimately
      minutes, so inactivity is what must be bounded, and ureq's
      overall `timeout()` takes precedence over `timeout_read` and
      would break large files.
      found while in there: `ureq::get` builds a **throwaway `Agent`
      per call**, so every track in a batch paid its own handshake; one
      pooled agent took 20 tracks from 20 connections to 4. the `.part`
      stub is also removed on every error exit now, including a failed
      rename, and a dead receiver aborts the batch instead of filling a
      cache nobody will read.
- [x] **the dead subsonic function is gone, and its test was
      toothless on both paths.** `artist_albums_with_tracks` was
      `#[allow(dead_code)]` and its doc's reason for existing was false
      on both halves: it was not serial (it called `album_details`'
      bounded fan-out) and nothing ever compared it against the live
      path. the connection-reuse assertion was the only one in the file
      and lived **only** on the dead path, so a regression reinstating
      the recorded "~16 handshakes" bug failed nothing.
      moving it to `stream_artist_tracks` revealed it was toothless
      *there* too: with pooling mutated away it still passed, because
      15 albums at zero latency stagger themselves and the recorded bug
      does not reproduce at that size. resized to 120 albums at 10ms,
      where pooled is a flat 9 connections however long the discography
      and unpooled measures 26-31; the mutation now fails 3 of 3.
      the live path also moved from static striping to the shared
      counter, converging it with `album_details`. perf-neutral on
      uniform latency and **-14% with skew** (469-480ms -> 403-416ms
      with every tenth album 8x slower), so a `with_slow_albums` knob
      was added to the mock to measure that rather than assert it.
- [x] **the frame source for ascii video exists** (`framesource.rs`),
      turning the unblocking measurement into a tested module. `Grid`
      owns all geometry, `Schedule` is pure arithmetic so every sync
      decision is testable with no child process, and `FrameSource`
      owns the child, the fifo and the read loop with teardown on
      `Drop`. **`ui::raster` needed no change at all**: frames are read
      straight into a `DynamicImage::ImageRgb8`, so handing one over is
      a borrow, and raster is a 2x2 *quadrant* rasterizer (its own doc
      and docs/raster.md say half-block and are stale) which `Grid` can
      ask mpv to match exactly, `Fit::Stretch` then being a 1:1 copy.
      four findings the measurement pass did not have:
      **the prototype's filter chain letterboxes for square pixels**,
      so 16:9 into 160x48 cells came out pillarboxed and stretched 2x
      vertically (the recorded "94.8% non-black" is only right if
      320x192 is displayed square, which a terminal never is);
      a clock landing exactly on a frame boundary **froze forever**,
      because `7.0/25.0*25.0` is `6.999999999999999` and floors to the
      frame already shown, so nothing was read and `time-pos` never
      moved; `--hr-seek` is a pin rather than a fix, since mpv already
      defaults to exact and lands on `floor(start*fps)`, which
      `Schedule` must snap to or every frame time is permanently off;
      and SIGPIPE bounds an orphaned instance to one pipe buffer
      rather than the length of the film.
      measured, release: mpv 38-39% of a core at 1080p into 320x96 at
      24fps (confirming the recorded figure), wim's reads 0.4%, wim's
      rasterization 8-9%. decode dominates by an order of magnitude and
      is in another process. restart is 156-430ms best but 254ms-1.9s
      worst, which is worse than the recorded 180-550ms.
      **zero allocations per frame, counted.** pointer identity cannot
      prove that: a `clone` of a live buffer still alternates between
      two addresses, because the old chunk is freed and reused next
      frame. it takes a counting allocator, in a separate test binary
      since `#[global_allocator]` is per-binary.
      open question for the maintainer, recorded in QUESTIONS.md
      rather than decided: **colour or monochrome.** `paint` takes a
      `raster::Tone` so both work today and it defaults to `Mono`,
      matching what `ui/albumart.rs` already asserts. the exemption
      video has in docs/video.md is narrower than it looks: it covers
      *mpv's* pixels drawn by `tct`/`kitty`, on the grounds that they
      are not wim's. an ascii renderer paints wim's own cells, so a
      second exception needs the same direct authorisation
      `ascii-color` got.

- [x] **`App` holds the ranking, not a clone of every row.** the last
      step of the search-cost item. `Ranking` (indices plus the
      revision they address) replaces `Vec<SearchItem>`, with the
      materialized rows built on first ask and dropped on every
      re-rank. `results()` still hands out a slice, because `core.rs`
      and shellswim's `echo_lines` need one, but **the keystroke path
      never calls it**: the two whole-list consumers are a shellsong
      env and the console echo. `results_list` grew a `Rows` trait so
      `layout`/`header_rows`/`row_items` take a count and the group
      boundaries, which is all they ever read.
      **measured by interleaving the two trees**, because sequential
      runs moved 1.8x on load: the clone is 3.74ms of a broad 40k query
      and 14.27ms at 150k, which **confirms the roadmap's 3-5ms**. the
      "of a ~7ms search" framing does **not** hold: it is 41% on top of
      ranking at that query and 1-3% for a realistic 5-6 character one.
      the win is exactly where this item pointed (short query, large
      library) and scales with library size, but it is not a halving of
      a keystroke in general.
      an obstacle not in the brief: `resync_results` runs *after* the
      corpus was mutated, which is precisely when a ranking refuses to
      resolve a row, so read-the-selection-then-research cannot work.
      `selected` and an identity anchor are private now, written only
      by `select`. and `replace_corpus` is the only way to swap a
      corpus, pinned by a source scan asserting no other file assigns
      `.corpus`, which closes the onboarding site that swapped it
      without resyncing.
- [x] **keybinds are refused instead of silently dropped**, from both
      sides. a typo'd action, an unrepresentable key and a reserved key
      all fell back to the default with nothing said, and a collision
      was *applied*: `quit = "a"` killed quit outright, because
      `app.rs` tests `Enqueue` before it. `keymap.rs`'s own doc said
      the opposite. `from_overrides_reporting` returns warnings in
      `config::load_reporting`'s shape; collision resolution loops,
      since refusing an override hands its default back and that can
      collide in turn, and a genuine **swap** is honoured with no
      warning, which the naive version would have refused.
      the interactive remap had the same hole from the other side: it
      never checked `reserved_key_purpose`, so a binding was accepted,
      displayed, written to config.toml and discarded at the next
      launch. `Keymap::refusal` refuses exactly what a load would drop,
      including **a third class nobody had noticed**: keys
      `key_to_name` cannot store at all. and `name_to_key` measured
      length in *bytes*, so a key rebound to a wide char wrote a name
      that could not be read back.
      **the console was undiscoverable.** `:` opens a whole command
      language and appeared in neither help, onboarding nor the bottom
      bar, while `K` was listed. walking a new `RESERVED_KEYS` table
      found two more missing (`up`/`down`, `backspace`). the queue's
      `'a' to add` is the real `Enqueue` binding now, and `J/K` is
      honest again because main.rs stops claiming `K` over a focused
      queue view; a test reads main.rs for that guard, crudely, because
      the hint lives in one file and the precedence in the other and
      nothing else tied them together.
      the eyeball grid caught one more: onboarding clipped **both** its
      how-to-leave lines at 50x20, i.e. a screen that never says how to
      leave it, so a pinned tail draws them from the bottom up.
- [x] **the play index is off toml, and lost writes are reported.**
      the biggest startup find was not the catalog parse at all:
      `CacheIndex::load` is toml and is the one startup read still on
      the main thread before the first frame, with `save` back on it on
      **every track change**. parse then render: 2 000 tracks 22.3 ->
      4.2ms and 26.8 -> 1.3ms; 5 000 59.6 -> 13.0 and 73.8 -> 4.5;
      20 000 x 64 days 948 -> 361ms and 1.47s -> 97ms. `index.toml`
      becomes `index.ndjson`, the same shape and the same argument the
      catalog half already used; the old toml is read once when there
      is no ndjson and unlinked after the first good write, so nobody
      loses the play history the auto-download threshold counts.
      the warm scan's resolve did three things per file it did not
      need: a `String` for a `HashSet` of every path, a `PathBuf`
      clone, and a clone of tags out of an index that is then dropped
      unread. `TagIndex::take` moves them, and what is left is by
      construction what `retain_seen` pruned: **73.7 -> 39.6ms at 34k
      files**, floor 210 -> 175ms. the walk is 78% of what remains, at
      exactly one `lstat` per media file (a non-media extension is
      never stat'd and never tagged).
      **tried and reverted**: fanning the 34k stats over a pool, with
      order preserved. serial cpu 138.9ms vs pooled 137.0, i.e.
      identical, so they are cpu-bound on a warm dcache rather than
      latency-bound, and pool-of-one overhead was +78%. same three
      reasons as the recorded catalog finding. the catalog fan-out was
      not re-litigated.
      and the audit's two silent-save findings here: a full disk killed
      the startup cache and the queue resume without a word. both
      report now, and `PlaybackState` gained a round-trip test plus a
      **literal on-disk fixture**, so a field rename fails instead of
      reading as "nothing to resume" on every install.

- [x] **the test suite could write the user's real cache, and did.**
      found by an agent that then checked its own damage: a
      `cargo test --lib -- --ignored` run reaches `cache_dir()` through
      `note_playback_started` -> `CacheIndex::save` from a harness that
      starts at `CacheIndex::default()`, so it **overwrote a real
      install's play index with the fixture's one row**. the already
      downloaded files under `tracks/` survived but stopped being
      referenced, so they would be re-streamed and re-downloaded and
      `sweep_stale` no longer knew about them. a default
      `cargo test --lib` does not do it; an `--ignored` run does, and
      this repo's convention is that mpv-gated behavioural tests are
      `#[ignore]`d, so "run them with `--ignored` rather than trusting
      them" is advice that was actively harmful.
      `cache_dir()` now **refuses a path outside the temp dir under
      `cfg(test)`**. honouring `XDG_CACHE_HOME` was never the problem;
      not noticing when a test had failed to set it was. a test that
      wants a cache has to point it somewhere disposable and is told so
      by name. verified by running all 50 `#[ignore]`d tests with the
      guard in place: still green, because the write now fails where
      the result was already discarded.
      the better fix is still open and belongs to whoever owns
      `core.rs` next: a `persist: &dyn Fn(&CacheIndex)` on
      `DownloadCtx`, the shape `download::Sink` already uses, so the
      save is injected rather than reached for.
      **the data loss itself is the maintainer's to decide about.** the
      two orphaned files are recoverable in principle (the filename
      stem is the song id, so `cached_path` can be relinked) but the
      play-day history is genuinely gone, and reconstructing an index
      with invented counts is not a repair anyone should make on
      someone else's behalf. see QUESTIONS.md.

### the untracked-defect audit (2026-10-31)

Ran once the roadmap was down to 8 unchecked items, 5 of them blocked,
to answer "what is not written down". 55 findings, verified reachable
(the auditor reproduced the three top panics with a standalone probe
rather than reasoning about them). **`[profile.release]` sets no
`overflow-checks`, so release wraps where debug panics**, which is why
the arithmetic findings are not purely theoretical.

Four are already fixed and are checked off in their own sections
above: the cleared-queue-vs-loop-all bug, `Op::RemoveFromQueue`'s
ordering, the header's blind truncation, and `ArtProbe`'s dead-probe
arm. The rest are below, grouped by how bad they are rather than by
file, because several span two files.

**these abort the process from ordinary use.** in flight as of this
writing (two agents, one per file).

- [x] **`enclosing()`: a multibyte char before `(` aborted the
      process on one keystroke.** the call-name range came from
      `rfind(pred).map(|j| j + 1)`, and `rfind` returns the matched
      char's *first* byte, so `rós(` built `name=(2,4)` and the slice
      panicked inside the `ó`. autopair meant one `(` did it, and
      `site()`/`enclosing()` run per keystroke, so it was an instant
      crash while typing in either binary.
      now `char_indices().rev().find(pred)` plus `len_utf8()`, so the
      start lands after the char rather than inside it; the name is
      still the ascii ident tail (`rós(` gives `s`, `é(` gives
      nothing). the same `rfind`-plus-one shape appears nowhere else in
      the file: every other `+ 1` is keyed off a single-byte ascii
      literal found with `find` or a byte scan that only acts on ascii.
      **the test whose name gave false confidence is renamed.**
      `multibyte_input_never_slices_mid_character` only ever covered
      `edit_insert`/`edit_backspace`, so it is now
      `multibyte_editing_never_slices_mid_character` and
      `the_site_never_slices_a_multibyte_call_name` is its sibling,
      sweeping `site()` over every char boundary of each autopaired
      form.
- [x] **`parse_time`: `inf` was a valid duration, and the player had
      no way to refuse it.** `parse_time`'s bare-number branch is
      `rest.parse::<f64>()` with only a `< 0.0` check, so `inf` and
      `1e30` become `Pos::Seconds`. `: sleep(inf)`, `: pause(inf)`,
      `: fade(out, inf)`, `: silence(inf)` all reached
      `Duration::from_secs_f64(inf)`, which panics *inside the draw
      loop*; the hook tore the terminal down and the queue went with
      it, and `secs.max(0.0)` launders NaN but not infinity.
      every seconds-valued exit of `parse_time` (bare, colon and unit
      forms) now goes through one `seconds()` gate that refuses
      non-finite and anything over `MAX_TIME_HOURS`. **24h was chosen**
      as past any real sleep timer, gap, silence, fade or seek position
      while staying ~10^13 clear of `Duration`'s overflow, so the host
      can convert unconditionally; refused with a message rather than
      clamped, matching the file's existing habit for a count above
      256.
      same boundary, same class, all fixed with it: a bare-number
      `volume` is held to the `0..=100` the `%` spelling always was,
      which is what kept `volume(nan)` out of `remember`'s persisted
      settings; and `silence` rounds with `s.round().max(1.0)` instead
      of truncating, so `silence(0.4s)` is a one-second silence rather
      than a zero-second one and `silence(1e20)` can no longer saturate
      the `u64` into "5124095576030431h8m15s".
      **the test aborts exactly where the app did**: it runs every
      emitted op through core.rs's own
      `Duration::from_secs_f64(secs.max(0.0))` rather than asserting on
      the parse alone.
- [x] **one rule for moving a cursor in a list**
      (`main.rs::step_in_list`), and **the audit was wrong about this
      one**, which is worth recording as loudly as the findings that
      held. it called the suggestions branch a process abort
      (`clamp(0, count - 1)` at count 0); it is not. that
      `saturating_sub(1)` runs in **usize** and the `as i64` cast
      follows it, so the max is 0 rather than -1, `clamp(0, 0)` never
      panics, and `: bottom()` on an empty pane writes 0 rather than
      `usize::MAX`. reproduced against the bug-intact body and the
      empty-list test passed.
      **a real bug was underneath it**: the `at as i64`, which reads a
      cursor sitting at `usize::MAX` as -1, so a down-step from a stale
      cursor landed on the top row instead of the last one. the three
      copies of "step, then stay inside the list" (suggestions, queue
      view, the keyboard j/k pane) now call one saturating usize
      helper, and the queue branch's `len() as i64 - 1` (genuinely -1
      when empty, and safe only because of the `is_empty` guard above
      it) went with them. pinned on an empty list, both ends, a page
      jump, and a cursor past the end; mutation-checked by restoring
      the `as i64` body.
      `hit::wheel_select` is a fourth copy of the same rule for the
      mouse wheel. it guards `len == 0` explicitly and is safe, so it
      was left alone, but it is a fourth copy.
- [ ] **`view(suggestions)` on an empty corpus is a dead-end pane.**
      no longer a crash, but `main.rs` ~336 still installs
      `Some(suggestions)` and takes `Focus::Workspace` at zero
      entries, reporting "0 suggestions", where the queue branch
      answers `op_fail("the queue is empty")`. it should refuse the
      same way.
- [x] **a config file could abort the process** (`crossfade_seconds`).
      `Duration::from_secs_f64` panics on a negative, nan or infinite
      value, and that sleep-timer fade was the only unguarded
      `from_secs_f64` in the tree, so `crossfade_seconds = -1` plus a
      sleep timer killed wim. the settings menu's clamp only ever
      bounded its own 0.5 step, never the loaded value, and
      `f64::clamp` passes a nan straight through anyway.
      guarded at both ends: `config::crossfade_seconds` is the single
      range (1 to 15, non-finite falls back to the 3s default), and the
      menu's steps, load-time validation and `main.rs::fade_span` all
      go through it. **`.max(0.0)` would not have been enough**, which
      is the part worth remembering: nan is laundered by it but `inf`
      and `1e300` overflow a `Duration` and panic too. that is what the
      test pins.
- [x] **a typo'd token path no longer disables a feature in silence.**
      `listenbrainz_user_token_file` and `opensubtitles_api_key_file`
      were read with `fs::read_to_string(path).ok()`, so one wrong path
      meant onboarding said scrobbling was set up, the settings menu
      kept showing the row, and nothing ever scrobbled with nothing
      said. both read through `Result` twins shaped like
      `resolve_password` now; the `Option` resolvers, whose callers
      have nowhere to put an error, delegate to those, and load reports
      the io error once by path ("scrobbling is off: reading
      listenbrainz_user_token_file at ..."). pinned with a temp-dir
      fixture, the readable path and the missing one.
- [x] **config.toml validates on load, correcting rather than
      refusing.** `libary_paths` (sic) parsed clean and was read by
      nothing, so the library just looked empty, and
      `scrobble.threshold_percent = 5000` meant nothing ever scrobbled.
      every hand-editable number is pulled into the range its reader
      can act on with each correction named, and an unrecognised key is
      reported with the nearest real name through `namehint::closest`,
      nested tables and `[[navidrome_servers]]` included.
      **a warning and not `deny_unknown_fields`**, deliberately:
      refusing to start over one stray key in a hand-edited file (or
      over a key a newer wim wrote) is the worse failure, and that
      attribute covers only whichever struct carries it, so
      `[subtitles]`, `[scrobble]` and a `pasword` typo in a server
      block would all have slipped through anyway. nothing is written
      back, since `save` regenerates the file from the struct and would
      drop a hand-edited one's comments; the correction holds for the
      session and the next settings-menu save persists it.
      the hand-written key list is pinned by a test that builds an
      **exhaustive** `Config` literal with no `..Default::default()`,
      so a new field fails to compile until it is accounted for, and
      another asserts a config using the `cache_listens_threshold`
      field alias loads with zero warnings (exactly the kind of thing
      a hand-written list forgets).
- [x] **`repeat` expanded as the product of its levels, and
      `MAX_REPEAT` claimed otherwise.** the expansion is static and
      `MAX_REPEAT` bounds one level, so
      `: repeat(256, repeat(256, repeat(256, skip())))` built ~16.7M
      nodes (over a gigabyte) synchronously in `submit` with the ui
      frozen, and one more level was an unrecoverable
      `handle_alloc_error`.
      `MAX_EXPANSION` (4096 nodes) is checked inside the `repeat` loop
      against the program's *total*, with a `refused` flag so one
      `Node::Fail` is reported however deep the nesting went and every
      enclosing loop stops rather than spinning 256^depth no-op
      iterations. `dry` inherits it, since it plans through the same
      lowering. four levels lower to ~4.1k nodes instead of 4.3
      billion; a flat `repeat(256, skip())` is untouched; and
      `MAX_REPEAT`'s doc now says what it actually bounds and points at
      the constant that bounds the total.
- [ ] **`parse_call` recursion is still unbounded**, independently of
      lowering: a balanced deep nest (`dry(dry(dry(...)))`, pasted or
      in a `.shg` line) recurses once per level at *parse* time, before
      `MAX_EXPANSION` can see it. unverified, because a stack overflow
      aborts the test harness; flagged rather than expanded into scope.
      a depth counter in `parse_call` would close it.

**silent desync and stale replies.** none of these crash; all of them
do the wrong thing quietly, which is worse to diagnose.

- [ ] **`main.rs` ~2948 is the only `servers_tx.send` site** while
      `cfg.navidrome_servers` is mutated in three (~2946 onboarding,
      ~3023 nav-manager delete, ~3086 nav-manager add). press
      `n j d d`: the profile leaves config.toml and the list, but
      `spawn_background_sync`'s own `servers` vec never changes, so
      that server keeps being polled every 45s, its catalog keeps
      being re-merged, offline/online toasts keep firing for a profile
      you removed, and its `Arc<Client>` is never dropped so the
      superseded password is never zeroized. add a server instead and
      it indexes once and is never auto-resynced, so new albums do not
      appear until restart.
      **and it is a pinned-the-function-not-the-wiring case**:
      `catalog.rs` 308/325/335 pin `retain_live_clients` dropping an
      edited or removed profile's client, but its single production
      call site (`catalog.rs` ~60) reads a list that only ever grows,
      so the behaviour those three tests assert is unreachable in the
      binary.
- [x] **onboarding's library-paths step did three wrong things at
      once**, and `Op::Rescan` had been doing all three right next
      door.
      it ran `library::scan` **synchronously on the main loop**, so the
      first-run ui froze with no feedback (that path had been threaded
      everywhere else since the recorded "this used to run inline
      before the first frame" fix). it never replaced `scan_rx`, so the
      startup scan's in-flight batches drained into the new corpus
      (`Corpus::push` dedups remote keys only) and **every local track
      was listed twice for the rest of the session**. and it built a
      whole new corpus rather than dropping the local half, so the
      navidrome catalog the *previous* onboarding step had just indexed
      went with it until the next 45s sync.
      the rule that matters is the receiver, so it lives in one place
      now: `library::restart_scan(paths, &mut rx)` makes the fresh
      channel, spawns the thread and swaps the receiver, with the
      reasoning in its doc. the caller keeps the corpus half
      (`drop_local`, not a new corpus) because that is the part the two
      call sites legitimately differ on.
      the test pins the receiver swap rather than the thread, since
      that is the half that was wrong: a batch queued on the old
      channel must reach nobody, and the old sender must come back
      `Err`. mutation-checked by dropping the swap, which is exactly
      the shape someone "simplifying" it back would produce.
      `rebuild_corpus` had that one caller and is gone.
      **this was also the trigger named by the `Ranking` revision
      guard**: stale rows today, wrong rows once `App` held the order,
      which it now does.
- [ ] **`main.rs` ~2297: a late subtitle lands on the wrong video.**
      `SubtitleReady` (`core.rs` ~753) carries only path/release/
      language, no track identity and no epoch, and the drain calls
      `sub_add` unconditionally with mpv's `select` flag. with an
      opensubtitles key set, play video A then immediately play B (or
      let a queue auto-advance): A's three-round-trip fetch lands late
      and becomes B's active subtitle track, with a toast naming A's
      release. which wins is thread-completion order.
      every other async reply path in the tree has a generation or
      identity guard (`browse::Loader`, `coverart::Cache`,
      `download::DownloadEvent`, `graphics::ArtProbe`, mpv ipc); this
      is the one that does not.
- [x] **undo's restart branch did not disown in-flight
      resolutions.** queue a prolific artist, press next, undo: the
      restore rewrote the queue and the artist fan-out landed seconds
      later, appending the whole discography through `append_resolved`
      with a "queued 847 tracks" toast, **visibly un-doing the undo**.
      `invalidate()` was written per branch and the one branch of three
      that reloads mpv's playlist did not have it; hoisted above the
      branching, once, because a guard each branch has to remember is
      a guard one branch forgets.
      the behavioural test goes through the real `Snapshot::restore`
      and **fires a control request first**, so a disowned batch is
      distinguishable from no batch at all; that control is what keeps
      it from passing for the wrong reason. a source-level `contract`
      test pins "one call, before the branching" for ci, which cannot
      run the mpv-gated half.
      **still open on that branch**: it calls `load_playlist`, not
      `start_session`, so none of the five track-start hooks fire (no
      subtitle fetch, no playing-now submission, `listened_at` from the
      previous track, no bound eq/volume/speed). that needs a `Surface`
      threaded into `restore`, and the exact diff plus the two
      behaviour changes riding along are written up in docs/undo.md.
      worth reading before touching it: calling `track_started` without
      a `Surface` would be **worse than the gap**, because
      `identity_settings` mutates `Overridden` as it decides and
      returns the changes as ops, so with nowhere to put them the ops
      are dropped while `Overridden` has already recorded them as
      applied, and the *next* unbound track restores to a value never
      set.
- [x] **skip-back after an undo reloaded the track already playing.**
      `next` makes the track it left the previous one, and `Snapshot`
      carried no `previous_track`, so an undo put the track back and
      left back pointing at it. snapshotted, and restored *after*
      `restore_queue`, because `start_session` owns that slot while it
      rewrites the playlist. dropping the one assignment fails the test
      with the restored track's own id, which states the bug exactly.
- [x] **a restored query fired no remote search.** `search(bladee)`,
      `search(drain)`, `undo()` brought the text back with every remote
      title missing while `SearchSettle` still reported settled, so a
      `play(selection)` right after resolved against the short list.
      `App::set_query` only re-filters the corpus already indexed and
      says so: kicking the debounced `search3` is the caller's job, and
      `Op::Search` plus main.rs's two typing paths all do it while
      `Snapshot::restore` did not. it runs the same three lines now.
      the test **indexes a real server first**, since
      `notify_query_changed` sends nothing without one and the test
      would otherwise prove nothing, and asserts both the request going
      out and settle going unsettled; the two halves have teeth
      independently (dropping the notify failed the first, dropping
      only `settle.notified` failed the second).
- [ ] **`main.rs` ~4326: the pause keybind bypasses
      `core::toggle_pause`,** whose doc at `core.rs` ~54 claims "the
      keybind, the bar click and enter on the player cannot disagree".
      set `gap(5s)`, let a track end, press space during the
      countdown: mpv unpauses at an already-finished eof,
      `between.cancel()` never runs so `holding()` stays true and the
      countdown keeps drawing, and nothing plays until the gap
      expires. clicking the bar's pause glyph in the same moment does
      the right thing.
- [ ] **`main.rs` ~2421: the suggestions cursor is reconciled by
      clamping a flat index** while `suggest_with` rebuilds and
      reorders every section ("best section first"). open suggestions,
      press `j` seven times, let an in-flight enrichment reply land,
      press enter: the highlighted row's content changed under the
      cursor and enter acts on the replacement (for a `Song` that is
      `start_session`, which clears the queue). `browse.rs` ~640
      already anchors its cursor on `song_id` for exactly this.
- [ ] **`lyrics.rs` ~224: `Loader::invalidate` has no production
      caller.** only the test at ~333; `main.rs` ~4042 calls
      `lyrics.close()`, which drops lines only, and no track-change
      hook touches the loader. the
      `#[cfg_attr(not(test), allow(dead_code))]` is why nothing
      flagged it. press the lyrics key, press esc while lrclib is
      still answering, wait: the pane reopens itself and swallows
      j/k/esc again. or press it 3s before a track ends and let it
      auto-advance: the pane opens unprompted over the next track.
      the comment at `main.rs` ~2323 asserts the opposite of what the
      code does.
- [x] **`overlay` was a second copy of the modal list, and it was
      wrong in both directions *and* answering two different
      questions.** `hit::Tiers` already held the list once.
      direction one: `console` and `subtitle_menu` cover the screen and
      were missing from `overlay`, which feeds `video::wanted`, so mpv
      kept drawing a video over them. play a local video, press `v`,
      press the subtitle-menu key: **the one menu that exists for video
      was invisible while claiming every key.**
      direction two: `undo_menu_open` was in `overlay` with no `Tiers`
      entry, so with the undo tree drawn over the workspace a click or
      a wheel notch resolved against the pane behind it.
      **and the conflation, which is the part the audit did not see.**
      `animating` read `!overlay && (.. || console_open || ..)`, so one
      flag was answering "is something painted over the video" *and*
      "is there nothing left worth repainting". those differ on exactly
      the console: it covers the screen and is simultaneously the
      reason to keep animating, because its caret blinks. adding the
      console to `overlay` to fix direction one would have frozen its
      own caret. so `Modal` gained `animates()` beside
      `covers_screen()`, and `Tiers` answers both questions from one
      literal built once per frame: `covers_screen()` for mpv,
      `static_modal()` for the animation gate.
      `Modal::UndoMenu` sits between `SettingsMenu` and `EqMenu`,
      matching main.rs's key cascade, which is the order `TIERS`
      exists to mirror.
      four tests, each mutation-checked: every modal is reachable
      through `Tiers` and is frontmost alone (direction two), a
      screen-covering tier takes the screen back with the console and
      subtitle menu named explicitly (direction one), a text box drawn
      inside a visible pane does *not* (the two the old flag was right
      about by omission), and a tier with a caret covers the screen
      without freezing it (the conflation). making the console static
      fails the last one; making it not cover the screen fails two.
- [x] **a restore point taken mid fade-out muted the next track.**
      `identity_settings` stores what it is handed as the level to
      restore to when a binding stops applying, and `track_started`
      handed it `current_volume(core.player)`. that runs at a track
      boundary, i.e. mid crossfade fade-out, so a bound track captured
      something near zero and **the next unbound track was restored to
      that**: silent until you typed `volume(50%)`. same root cause at
      `Op::SetVolume`, where a relative nudge measured against the live
      reading made `: volume(+10%)` in the last seconds of a track give
      10 instead of 90, and that became the new `base_volume`.
      both sites take `*core.base_volume` now, which *is* the chosen
      level by construction (every `SetVolume` writes it, and the code
      already called it "the ceiling the crossfade and fade ramps scale
      against"). the live reading survives in exactly one place,
      `Op::Fade`'s starting point, where it is correct.
      `identity_settings` had **no tests at all**; it has a module of
      its own now rather than joining an existing one (see the accreted
      -test-module item). the behavioural test is the two-track
      sequence, because that is the only shape the bug appears in: a
      single bound track looks fine, and it is the *next* one that goes
      quiet. it keys the binding through `identity_of` so a change to
      how a track identity is spelled cannot silently unbind it and
      leave the test passing for nothing.
      the call site itself needs a live mpv, so the guard on it reads
      the source. **the first draft of that guard was worthless**: it
      checked only the argument list, and the mutation that moved the
      transient one line up into the local it reads passed happily. it
      checks the whole function body now, and catches it.
- [ ] **`Op::Remember` still never seeds `overridden`** (`core.rs`
      2508/2780 against the single writer at ~3063). at volume 80,
      `: remember(album, volume(25%))` then `: skip()` onto another
      album: `settle` sees `active == None` and restores nothing, so
      you stay at 25 for the session, contradicting the documented
      rule. separate from the fix above, which was about *what* gets
      captured rather than whether anything is.
- [x] **`BetweenTracks::step` never retried a lost keep-open write.**
      it did `let _ = player.set_keep_open(..)` and then set
      `self.keep_open` unconditionally, while the write is gated on the
      field differing. the field *is* "what mpv has been told", so
      recording a failed write means it is never retried: one dropped
      ipc message of `false` left `keep-open` pinned on, mpv never went
      idle, and `idle-active` going quiet took end-of-playlist and
      loop-all with it for the rest of the session.
      the write needs a live mpv, so the memo rule is split into
      `needs_keep_open` / `keep_open_written` and tested on its own;
      `step` is three lines around them. the test covers both
      directions, because the one that pins keep-open *on* is turning
      it back off. mutation-checked by recording the write
      unconditionally again.
- [x] **`mute` and `eq_custom` were in docs/undo.md's snapshot and
      not in `Snapshot`.** `mute()` then `undo()` reported success over
      music that was still silent, and a restored flat curve read "flat
      (modified)". both snapshotted rather than struck from the doc.
      `eq_custom` is free (an `App` field). `mute` has no host-side
      mirror, `Op::Mute` being one `set_property` with mpv the only
      place it lives, so `capture` makes one bounded `get_property`:
      **the budget `paused` and the live volume were refused on**, and
      the difference is that mute cannot drift out of band, since
      `mute()`/`unmute()` are its only mutation and both are recorded
      ops. stored as `Option<bool>`, so a read mpv refuses leaves the
      restore alone instead of unmuting, and `capture`'s purity claim
      was updated rather than quietly broken.
      the tests read what the user reads: the mute one mutes with the
      same single call `Op::Mute` makes and reads the property back,
      and the eq one nudges a band through the real `adjust_eq_band`
      and asserts on `eq_label`, failing "flat (modified)" against
      "flat".
      **docs/undo.md's list was wrong in two more places** and is
      corrected: `paused` and a live fade are deliberately out. it also
      now records that **there is no mute indicator anywhere in the
      ui**, so muted is indistinguishable on screen from volume at
      zero.
- [x] **`enqueue_track`'s shuffle move was fire and forget.** it
      appended, let `append` pick a shuffle slot (mutating `entries`),
      and only then asked mpv with `let _ = player.playlist_move(..)`.
      `entries` and mpv's playlist agree by **index**, so a refused
      move left them permanently one apart and every later remove,
      move and jump addressed the wrong track, with nothing said.
      the same ordering lesson as `Op::RemoveFromQueue`, but it cannot
      take the same shape: `append` picks the destination *as* it
      moves, so there is nothing to ask mpv about beforehand. so the
      local move is reversed on refusal and the failure is reported
      ("queued X, but could not shuffle it in").
      the reversal has to be exact including the play position, which
      is what the test pins: at three different positions, append with
      shuffle, reverse, and assert the order matches a plain append and
      the playing track did not move. it asserts the shuffle actually
      moved something first, so a position with nowhere to go cannot
      make it vacuous.
- [ ] **two smaller ones from the same group are still open.**
      `core.rs` ~1539 `persist_eq` is the only writer of
      `cfg.eq_preset`/`eq_gains` and all three call sites are inside
      the eq-menu key block, so `e` to cycle the eq (or `: eq(rock)`)
      is lost on restart while the identical change made from the menu
      persists.
      and two seams the undo work hit: `TrackResolver::deliver`/`epoch`
      are `#[cfg(test)]` **private**, so undo.rs cannot inject a batch
      and its test works around it with a real off-thread request plus
      a positive control; `pub(crate)` would make it direct. core.rs's
      `CoreCtx` test harness is likewise private to its own test
      module, so undo.rs carries a near-copy, and one shared `fixtures`
      harness is the obvious cleanup.
- [ ] **`core.rs` ~1206: `go_to_previous` fires three of the five
      track-start hooks by hand** instead of calling `track_started`,
      which is the exact parity bug `track_started` was created to
      close. enable crossfade, press skip-back during a fade-out: no
      bound eq/volume/speed, and the volume is left wherever the ramp
      last wrote it for the whole track.
- [ ] **the resolver's queue append has no undo hook, and the reason
      is a timing one worth writing down before anyone starts.**
      the site is `track_resolver.poll()`'s drain loop in main.rs (grep
      `let _ = &mut surf;`, which is the tell: a `Surface` is built and
      deliberately unused). queue an album, undo, redo: the tracks
      vanish.
      **the snapshot is not missing, it is early.** `queue(album)` is
      recorded when it is typed, which is before the fan-out has
      resolved a single track, so the snapshot redo restores is a state
      that never saw them. so the fix is not "add a hook here" but
      "record again once the tracks land", which raises a question the
      audit did not: a second snapshot per async batch means an undo
      tree with a node the user never typed. the alternative is
      amending the existing node in place, which `undo::Tree` has no
      operation for. **pick deliberately and say which in the entry**;
      amending is probably right, since the user performed one action.
      relatedly `core.rs` ~450: the resolver *reads* the epoch rather
      than bumping it, which is right for `Intent::Back` but means two
      `Intent::Front` batches fight for the same insert slot, so
      queue-front album A then album B and the order is
      nondeterministic run to run.
- [ ] **`main.rs` ~2356 / `shellsong_host.rs` ~569: a settle can
      answer the wrong query.** `SearchSettle::answered` clears
      `pending_since` unconditionally and `RemoteSearchOutcome`
      carries no query. type `cre`, then within the round trip type
      `st`: "cre"'s answer settles "crest" before a request for it has
      left the debounce window, so a shellsong program gated on
      `search_settled` runs against the wrong result set.
      `a_newer_query_restarts_the_window` covers notify-then-notify
      only.

**grapheme clusters, not wide glyphs.** the wide-glyph case is already
right and must not be regressed (`textfit` exists, `ui/draw.rs` is a
width-aware single writer, CJK does not shift). the open class is
systemic: `Cargo.toml` has `unicode-width` but no
`unicode-segmentation`, and **the repo's two width functions implement
different models**: `textfit::width` uses `UnicodeWidthStr`
(string-level ligature rules) while `textfit::char_cells` uses
`UnicodeWidthChar` (no such rules), so every truncation mixes them.

- [x] ~~resolved by the overnight run~~ **`ui/draw.rs` ~73: `cursor += cells.max(1)` gives a
      zero-width codepoint its own cell.** measured in a real
      `Buffer` with a `│` at column 8: `"❤️ love song"` returns x=9 and
      overwrites the border; `"1️⃣ favourites"` x=10; `"👩‍👩‍👧 mix"` x=10
      with the three faces in separate cells; NFD `"Beyoncé"` x=8 for
      a 7-cell string; decomposed `"각 song"` (how macOS/APFS stores
      hangul) x=9, painted as three loose jamo. a zero-width cell is
      not skipped by `Buffer::diff`, so the terminal re-joins the
      cluster and everything after it lands one column left of where
      the buffer thinks. trigger: any NFD-tagged or emoji-containing
      track name, which is ordinary library content.
- [x] ~~resolved by the overnight run~~ **`ui/textfit.rs` 39/55/74: the doc contract "never wider than
      asked for" is false.** `truncate_with_ellipsis("❤️ love song", 8)`
      returns real width 9; `truncate_to_width("👩‍👩‍👧 mix", 8)` returns
      real width 4 (ZWJ components double-counted). this feeds the
      item above, and via `textfit::columns` it makes a console echo
      line exceed `avail`, wrap, and break the grid.
- [x] ~~resolved by the overnight run~~ **`swimconsole.rs` ~262 and `ui/shellsong.rs` ~303: two more
      hand-rolled per-char writers with the same `.max(1)`,** measured
      against `width()`-derived budgets. type `Beyoncé` NFD at the
      shellswim prompt and the combining acute is written to the next
      cell and then overwritten by the block caret, so an accent you
      typed is invisible; type `star(❤️)` and the caret floats two
      cells past the `)`. the overlay copy also paints past its own
      `│` (no clipping of its own) and, because truncation is
      width-based, never drops zero-width chars, so a pasted
      combining-mark run drives `col: u16` past 65535 and wraps in
      release.
- [x] ~~resolved by the overnight run~~ **`ui/header.rs` ~61: the `│` separator column comes from an
      advancing per-char `x`** while the `┴` junction uses the same
      overcounted value, so with an NFD title the rule is painted at
      buffer column 20 and renders at terminal column 19, a visible
      one-column kink.
      **fix direction for all four**: make `textfit` grapheme-based
      (cluster iteration, `UnicodeWidthStr::width` per cluster, drop
      zero-width clusters), have `draw::at_styled` write a cluster as
      one `set_symbol`, delete the `.max(1)`, and route the three
      hand-rolled writers through `draw::at`.
- [x] ~~resolved by the overnight run~~ **`ui/shellsong.rs` ~181: the ghost completion is drawn
      unconditionally at `cursor_col + 1`,** over cells the real text
      was just written into. type `queue(fl, hexed` then press Left
      seven times: the row renders `: queue(fl█atline)` with your
      typed `, hexed` gone from the screen while still in the buffer.
      `swimconsole.rs` ~157 guards exactly this with
      `cursor_col != width(&window)`.

**layout math and paint outside the rect.** root cause for several:
`ui/draw.rs` ships two classes of writer and the asymmetry is
undocumented. `put`/`at`/`at_styled` bounds-check the *buffer* only;
`line`/`centered` also clamp y to the *area*. also
`testbuf::assert_outside_untouched` exists and 12 widgets use it, but
`browse`, `results_list`, `eq_menu`, `queue`, `suggestions`, `player`,
`notify`, `miniplayer` and `shellsong` do not; several have
`degenerate_sizes_do_not_panic` tests, which catch panics but not
stray paint.

- [x] **three menus ignored `ui::scroll::offset_for`**, whose module
      doc describes precisely this bug: keybinds, settings and the
      navidrome profile list drew their rows from a fixed top, so past
      the pane height `j` moved a cursor nothing showed. on a plain
      80x24 the keybinds `>` marker, the "press a key..." prompt and
      the footer hint were all off-screen and `r` rebound an action you
      could not see; a 14-row terminal hid the listenbrainz token row
      and the add-new-profile row while both stayed selectable.
      all three reserve their footer rows first, scroll the list
      through `offset_for`, and mark a window onto a longer list with
      `▴`/`▾` in a one-column gutter **claimed only while it actually
      scrolls**, so a list that fits keeps the full width (pinned by
      the untouched 17-row settings snapshot, whose last hint still
      reaches the final column). the cue painter lives once, in
      `results_list`, which already hosts the shared `draw_header_row`
      and `fill_row_bg`; it is `lyrics_view`'s own glyph and shade, not
      a new idiom. each mutation (`offset = 0`) failed exactly its own
      menu's test.
      **and printing it caught one more thing, in the header of the
      very buffer being read**: the keybinds menu said "k to close"
      while lowercase `k` moves the selection, which the footer two
      rows down already contradicted. it reads "shift+k or esc: close"
      now. that was a separate audit finding below; it got fixed
      because the printed output put the contradiction side by side.
- [x] **five widgets painted outside their rect**, because
      `draw::put`/`at`/`at_styled` bounds-check the *buffer* and not
      the *area*, an asymmetry with `line`/`centered` that was
      undocumented.
      `results_list::draw_header_row` had no guard at all and `browse`
      fed it height-0 label rects, so `─── ALBUMS ───` landed on the
      bottom bar's rule and wiped its `┬`/`┴` junctions. browse's
      top-rule loop and its fixed three-row bio block did the same on a
      pane of 0 to 6 rows. the eq menu's "applied:" line and
      `save as: …█` row sat two under the header unconditionally and
      ate the frame's own bottom edge at seven terminal rows. the
      banner's `<`/`>` row was never clamped and punched holes through
      the `HorizontalRule`, which is drawn earlier and never repainted
      (far worse with `figlet: doh` at 90x29).
      fixes: one `owns_row` predicate shared by both `results_list`
      painters (`fill_row_bg` had guarded the bottom edge only); the
      browse top rule painted only where `rule_rows` reports one,
      **since the report already had the filter and the paint did
      not**, which was the actual bug rather than a missing check; the
      bio rect intersected with the pane before wrapping, so
      `wrap_bio` gets the real line budget; both eq rows guarded like
      their `draw::line` sibling in the same match; and the arrow row
      checked exactly like the logo grid ten lines below it.
      `assert_outside_untouched` now covers `browse`, `results_list`,
      `eq_menu` and `banner`, the four that lacked it, each with an
      eyeball printer at the broken sizes. every guard was
      mutation-checked individually.
- [ ] **`ui/shellsong.rs` ~69: the overlay console cannot scroll.**
      it never calls `Console::saw_viewport(rows)` and never reads
      `Console::scroll()`, so it always renders the live tail: open
      `:` and press PageUp and the keys are visibly dead even though
      `handle_key` moved `scroll`. knock-ons: `viewport` stays 0 so
      `scroll_pages` pages by one line; `scroll > 0` makes
      `following()` false so every later `push` increments `scroll`
      with nothing resetting it; and `SCROLLBACK_ROWS = 8` hides
      everything older with no indicator, where `swimconsole.rs` ~67
      prints `^ N back · pgdn to catch up`. `Console::viewport`'s own
      doc says the renderer must report this because it is the only
      thing that knows.

**tests that hide risk.** the pattern this repo keeps hitting
("pinned the function not the wiring") plus two new ones.

- [ ] **`core.rs` ~1012, tests at ~4198/~4217: the pairing is tested,
      the wiring is not.** the `resolve_all` tests' doc claims to
      cover "the latent desync at the seam where it lived", but
      `load_playlist` and `resync_upcoming`, the two functions the
      `core.rs` ~1035 "the pairing is the fix" comment names, have
      **zero test call sites**. collapsing the `refused` loop back to
      `map(|(i, _)| i).collect()` leaves both tests green and restores
      the exact off-by-one (wrong title, wrong scrobble, wrong art)
      the comment says shipped once.
- [x] ~~resolved by the overnight run~~ **`subsonic.rs` 1845/1865/1880 test a dead function.**
      `artist_albums_with_tracks` is `#[allow(dead_code)]` and its own
      doc says nothing calls it; the live artist page uses
      `stream_artist_tracks`, and the two are structurally different
      (shared `AtomicUsize` vs `skip(worker).step_by(workers)`). the
      connection-reuse assertion
      (`connections() <= ALBUM_FETCH_CONCURRENCY + 1`) exists **only
      on the dead path**, so a regression reinstating the recorded
      "~16 handshakes" bug fails nothing.
- [x] ~~resolved by the overnight run~~ **four assertions that cannot fail.** `shellsong.rs` ~6257
      asserts `candidates_at(..).len() <= MAX_CANDIDATES` with a
      one-item corpus against a cap of 50, while the comment two lines
      up says "the corpus is the half that still has to be capped: it
      is as long as the library"; the 50-name fixture that would
      exercise it exists at ~8186 and is used only by an `#[ignore]`d
      bench, so removing a `take(MAX_CANDIDATES)` and shipping a 20k
      row completion popup fails no test. `shellsong_host.rs` ~952
      hardcodes 300 instead of referencing
      `core::REMOTE_SEARCH_DEBOUNCE`, so it couples neither constant.
      `subsonic.rs` ~1925 sleeps 600ms and asserts only
      `requests() <= 18` of 61 with no lower bound, so it passes if
      the workers never ran. `persistence.rs` ~80 asserts
      `artist.is_none()`/`album.is_none()` on two hardcoded `None`s
      two lines apart and leaves the one computed field unasserted.
- [ ] **`core.rs` ~838 `scrobble_metadata` has zero tests,** and its
      own comment says `Silence` earned a `SearchItem` variant
      specifically so a 15-minute silence is not scrobbled as a track
      called "silence". move one variant into the `Some` arm and
      garbage goes to a public ListenBrainz profile with no failing
      test and, because `core.rs` 887/900 discard the submit result,
      no runtime signal either.
- [ ] **the undo seam is only covered by `#[ignore]`d tests.**
      `core.rs` 3220/3759/3819/3954/3979/4021: six behavioural (not
      benchmark) `seam_tests` need a live mpv, and they are the only
      coverage of undo recording through the real `apply_op`
      (`core.rs` ~3753 says so outright). a default `cargo test`
      verifies none of `recordable`, `coalesce_kind(&op)`, the
      `!matches!(report, Report::Failed(_))` guard, or suspension
      around delegated ops. reading `core.rs` the seam looks covered;
      in a normal run it is not.
- [ ] **three functions with no test call sites and stringly-typed
      contracts.** `core.rs` ~1082 `enqueue_track`: callers branch on
      `status.starts_with("error")` (~2386) versus
      `starts_with("error") || starts_with("not ")` (~2944), nothing
      pins the prefixes, and the two sites disagree about which
      matter. `core.rs` ~1350 `note_playback_started`: swapping one
      argument at ~1375 reinstates the reported "pressing enter twice
      downloads it" bug with all five `should_auto_download` tests
      green. and `PlaybackState`'s serde shape has no round-trip test
      at all while `load()` ends in `.ok()?`, so any field rename
      silently turns every saved queue into "nothing to resume".
- [ ] **accreted test modules, the recorded pattern, again.**
      `catalog.rs` ~273 `catalog_fetch_tests` holds 3 fetch tests plus
      4 client-lifetime/zeroize tests; `main.rs` 4637/4665/4680 are
      `is_starred`/`album_page` tests inside `shellsong_wiring_tests`;
      `main.rs` ~4497 is a serde-alias test inside
      `download_policy_tests`. third occurrence, so it is worth a
      convention rather than another cleanup.

**silent failures and undiscoverable keys.**

- [x] ~~resolved by the overnight run~~ **`main.rs` ~3588: the interactive remap accepts a key it will
      then drop.** it never checks `reserved_key_purpose`, so a
      binding onto a reserved key is accepted, displayed, written to
      config.toml, and silently discarded by `keymap.rs` ~309 at the
      next launch. press `K`, pick an action, `r`, `j`: the row
      updates, no warning, `j` only moves the selection, the action is
      dead, and on restart the menu shows the old key with nothing
      said. `keymap.rs` 25/261 state the opposite contract ("refusing
      the binding and saying why beats a key that silently does
      nothing"); the refusal exists only in `from_overrides`, never in
      the path most users will use.
- [x] ~~resolved by the overnight run~~ **`main.rs` ~3631 shadows `main.rs` ~4199, so `K` is broken in
      the queue view.** the unconditional
      `if key.code == Char('K') { keybinds_menu_open = true; continue; }`
      runs before the queue view's own handler, making the
      `Char('J') | Char('K')` arm unreachable for `K`: `J` moves a
      track down, `K` never moves one up, and `ui/queue.rs` ~161
      prints "enter: jump, d: remove, J/K: move, esc: close",
      teaching the broken half.
- [x] ~~resolved by the overnight run~~ **`keymap.rs` ~300: every class of `[keybinds]` config error is
      silent, and duplicates are not even detected.** a typo'd action
      name, an unrepresentable key and a reserved key all fall back to
      the default with no message; a collision (`quit = "a"`, which
      collides with Enqueue's default) is applied and silently kills
      `Quit`, since `Enqueue` is checked at `app.rs` ~295 and `Quit`
      at ~314. the interactive path does warn
      (`keybinds_menu.rs` ~84) but the warning is cleared by the next
      `j`/`k` and the menu then lists the shadowed binding as if it
      worked.
- [x] ~~resolved by the overnight run~~ **`ui/help.rs` ~164: the shellsong console is absent from the
      help pane entirely.** `:` opens a full command console with
      completion and is in `reserved_key_purpose` as "the shellsong
      console", but grepping `help.rs` for `console`/`shellsong`/`:`
      returns nothing; it is also missing from onboarding's controls
      table, the controls bar and the helpbox. an entire subsystem is
      reachable only by guessing, while `K` is listed.
- [x] ~~resolved by the overnight run~~ **`download.rs` ~155: one thread per track, no cap,** while
      every other batch network operation in the tree caps
      (`ALBUM_FETCH_CONCURRENCY`, `ENRICH_CONCURRENCY`). select an
      artist and press the download key: `download_resolved` hands the
      whole discography straight through, so 800 tracks means 800
      threads and 800 simultaneous connections, most failing with a
      storm of toasts. `ureq::get(url).call()` at ~82 also sets no
      timeout, so a black-holing server pins a thread and a `.part`
      file forever.
- [x] ~~resolved by the overnight run~~ **nine shellsong commands with no error for the obvious wrong
      input** (all `shellsong.rs`). ~2606 the `#` comment arm is the
      only separator not guarded by `depth <= 0`, so
      `: play(track #3)` truncates to `play(track` and reports
      "play( is never closed" with no mention of `#` (verified). ~2657
      the closer arm is `')' | ']'` with no opener match, so
      `: play(crest]` is silently accepted and executed (verified).
      ~2572 `fade`'s `over` is `Ty::Duration`, which accepts `off`,
      but `simple_op` only matches `Some(s)`, so `: fade(out, off)`
      silently fades over 5s. ~3325 the 0-100 ceiling lives only in
      `parse_time`'s percent branch, so `: volume(150)` is accepted
      and clamped while `: volume(150%)` errors. ~2865 a repeated
      `kind=` last-wins with no error. ~2727 `: let a == [x]` binds
      `a` to the literal `"= [x]"` and says "defined a: 1 terms".
      ~3647 `Delete` clears `cycle` but not `self.error`, unlike
      `Char` and `Backspace`.
- [x] ~~resolved by the overnight run~~ **`shellsong_host.rs` 38/352/336/588: four silent or wrong
      answers.** `save_vars`/`save_bindings` return `()` and discard
      both "no state dir" and the `fs::write` error while callers
      report success, so with an unwritable `$XDG_STATE_HOME`
      `: let morning = [mazzy star, burial]` says "defined morning: 2
      terms" and the variable is gone next launch (`queues::save` does
      report this, so the two stores disagree about the same
      failure). ~336 compares eq names with `PartialEq` instead of
      `eq::same_name`, so `: remember(artist, eq(Night))` never
      settles and every single track gets a redundant ipc write plus a
      toast. ~588 `moveto`'s `Seconds` branch does `s.max(0.0)` while
      the `Percent` branch on the next line clamps to the duration, so
      `: moveto(1h)` on a three-minute track reports "moved to 3600s"
      and skips the track.
- [x] **completion folds diacritics now, the way the search already
      did.** the corpus half was right: `search_names` folds, so it
      handed `Țara` back for `tara`. `extension` then compared the
      *unfolded* head, `țar` against `tar`, and threw every one of them
      away, so with a romanian library the feature looked entirely
      absent in the console (no ghost, no tab candidate, no listing and
      no message) while submitting the same line resolved fine.
      both sides fold in the comparison; the remainder is still sliced
      out of the **original** candidate, so nothing downstream sees a
      folded name, and folding is char for char so the char index still
      lines up. two tests through the real `candidates_at`, both
      directions (an ascii token reaching the diacritics and a typed
      `țar` reaching the ascii names), plus one pinning that the
      remainder carries the original bytes. mutation-checked by
      restoring the unfolded compare.
      **and the eyeball print earned its keep twice.** it is how the
      candidate row was confirmed non-empty at all, and its first draft
      appended the ghost to the end of the line and printed
      `play(t)ara`, which read like a bug in the autopairing; the ghost
      goes at the cursor.
      what accepting such a match *types* is a separate decision, now
      recorded in QUESTIONS.md: the remainder is appended, so the line
      becomes `play(tara)` rather than `play(Țara)`. left that way
      because `Suggestion`'s doc states "always a pure extension of
      what was typed, never a replacement" and a test enforces it, and
      because the folded line resolves anyway.
- [ ] **nine discarded results the user would expect feedback from.**
      `core.rs` 887/900 (every scrobble failure forever, so a revoked
      token is indistinguishable from working); ~640 (a failed
      favorites write while the star stays lit all session, where the
      remote path next to it correctly reverts and reports); 784-797
      (four consecutive silent returns in subtitle auto-fetch, so a
      wrong key looks identical to "no subtitles exist"); ~735
      (`open_starred` drops the `password_file` error `client_for`
      returns a `Result` to preserve); `main.rs` ~2062 (the
      restored-queue refill discards every per-track status, so a
      moved library shows a full queue in the ui and an empty playlist
      in mpv); `main.rs` ~4331 (skip-next has neither the last-track
      guard nor the message `Op::SkipNext` has); `catalog.rs` ~188 /
      `library.rs` ~392 / `core.rs` ~1150 (a full disk silently kills
      the startup cache, the queue resume and download bookkeeping).
      `config::save_or_warn` is the established pattern and is used
      correctly at all 16 settings sites.
- [x] ~~resolved by the overnight run~~ **two hardcoded keys in rendered text.** `ui/queue.rs` ~188
      hardcodes `'a'` for the remappable `Enqueue` ("enter on a song
      to start, 'a' to add"), the exact bug class
      `help::Key::Bound` exists to prevent and that the roadmap
      records fixing in onboarding. **the keybinds menu's "k to close" is
      fixed** (it reads "shift+k or esc: close"); `queue.rs`'s
      hardcoded `'a'` is still open.
- [ ] **`main.rs` ~556: `wim` has no `--help`.** it handles only
      `--version` and silently ignores every other argument, so
      `wim --help` and `wim song.mp3` both just start the TUI with no
      message. the sibling binary has a full `--help`, `-c`, script
      support and an explicit unknown-option refusal
      (`bin/shellswim/cli.rs` ~48, with the comment "guessing at a
      typo in something that controls playback is worse than saying
      so").

**checked and found fine; do not re-audit.** recorded so the next pass
does not spend itself here.

- **no reachable `usize`/`u16` underflow anywhere in `src/ui/*` or
  `src/layout.rs`.** every raw `-` was traced case by case; nothing
  wraps at 10x3 or at the 20-column sidebar minimum. the class that
  actually bites the ui is out-of-rect paint and layout-versus-hitmap
  disagreement, not wrapping arithmetic.
- `hit.rs`, `scroll.rs`, `progress.rs`, `theme.rs`, `anim.rs`
  (`clamp_hz` handles zero/negative/NaN/540Hz; every period guard is
  there; both compositor parsers are wired and have garbage-input
  tests), `controlsbar.rs`, `raster.rs`, `divider.rs`, `junctions.rs`,
  `albumart.rs`, `logos.rs`, `visualizer.rs`, `help.rs`, `helpbox.rs`,
  `login.rs`, `onboarding.rs`, `searchbox.rs`, `miniplayer.rs`,
  `lyrics_view.rs`, `queue.rs`, `suggestions.rs`.
- **every direct-index site in render code** (`results_list.rs` ~229,
  `browse.rs` ~421, `queue.rs` ~218, `suggestions.rs` ~180, eq_menu's
  `PRESETS`/`BAND_HZ`, `keybinds_menu.rs` ~59, `banner.rs` ~70)
  indexes a collection built from the same slice inside the same
  render call, so a corpus resync, a scan batch landing or a queue
  mutation between keypress and render cannot land out of range.
- **async paths with a correct guard**: `browse::Loader`'s generation
  (checked pre-send and on arrival, structural across all seven `Load`
  variants); `coverart::Cache` (identity by `Key`, not a counter, and
  `ProtocolCache` holds key and protocol as one tuple so they cannot
  desync); `download::DownloadEvent` (every variant id-tagged, looked
  up rather than applied to "the current thing"); `graphics::ArtProbe`;
  mpv ipc (per-request channel plus `request_id`, timed-out replies
  removed from `pending`); `Op::Rescan`; the remote-search merge
  (add-only, and `resync_results` re-finds the cursor by identity);
  enrichment replies.
- `PlayQueue`'s index arithmetic: `remove`/`move_entry` carry `pos`
  correctly in all cases and refuse out of range; `set_pos` is
  absolute so `-1` and over-range are `Unresolved` and a re-report is
  `Unchanged`, so track-change hooks cannot double-fire;
  `mpv::playlist_move` correctly converts mpv's displace-target
  off-by-one.
- `shellsong.rs` parsing beyond the findings above:
  `statements`/`matching`/`split_commas` use `bytes[i] as char` but
  only act on ascii bytes, so their offsets are always char
  boundaries; `extension()`, `token_bounds`, `in_string` and the edit
  primitives are char-aware and `Console::cursor` can only be a
  boundary; `parse_time`'s percent, colon and unit forms reject NaN,
  negatives, >3 parts and repeated units; `Ty::Count`/`Index`/
  `Indices`/`Speed` all range-check; one `expect` in the whole
  non-test half (a table invariant), no `unwrap`, no unchecked
  indexing; `Program::guard` range-checks every queue index.
- `eq.rs` (the Gaussian pivot in `compensated_gains` cannot be zero,
  `clamp_gains` neutralises non-finite config input,
  `build_af_filter`'s label matches `mpv::PROP_LEVEL_RMS`),
  `queues::slug` (cannot escape the queues directory),
  `Ramp::progress`/`sleep_step`/`seek_target`/`volume_target` (every
  divisor guarded), `Bindings` (keys come only from `key()` so stored
  and looked-up keys cannot drift), `identity_of` (`Silence` gets an
  empty identity).
- `term.rs`, including the panic hook: `restore()` emits no `?25h`,
  but `TerminalGuard`'s drop order runs `restore()` and then ratatui's
  `Terminal::drop`, which shows the cursor, on both the normal and the
  unwinding path.
- `notify.rs` mechanics (repeat coalescing with a refreshed timer,
  `MAX_STACKED` on both push paths, progress boxes exempt from expiry,
  `finish_progress` falling back to a fresh push so an outcome is
  never lost).
- **the wide-glyph handling that must not be regressed**:
  `ui/draw.rs`'s advance-by-`char_cells` plus blank-the-trailing-cell
  (and `Buffer::diff` skipping it), `banner::is_line_art` keeping the
  2-cell `ＷＩＭ` logo out of the picker, `lyrics_view`'s wrap,
  `browse::wrap_bio`, `searchbox`'s cursor from the writer's returned
  x, and ~20 other measured sites. legitimate byte/char length at
  `notify.rs` ~120, `core.rs` ~3009, `search.rs` ~476,
  `queues.rs` ~194, `keymap.rs` ~247.
- well-covered test areas: `eq.rs` (33, all wired to live call sites),
  `cache.rs` (34, including the pre-play-days migration), `ui/*`
  (~400, clean on the pinned-function pattern), `undo.rs`'s `Tree`
  internals (21, injected clock), `config.rs` (8, every pre-rename
  `album_art` alias). the 24 benchmark `#[ignore]`s are correctly
  labelled measurements.
- `src/mpv.rs` ~224 builds the ipc socket path under `/tmp` with pid
  plus sequence. `XDG_RUNTIME_DIR` would be the right directory on a
  multi-user box, but the remove-then-bind window is too narrow to
  call a finding.

### stage 4: the queue timeline (started)

- [x] **`wait(15s)` as an alias for `silence`** (maintainer, asked
      mid-run). one line in the command table, because aliases resolve
      before lowering, so there is no second arm to keep in step; a
      test asserts `wait(15s)` and `silence(15s)` lower identically.
      `silence` stays canonical: it names what actually plays, a
      generated silent source rather than a suspended player, which
      matters because `pause()` already exists and means something
      else.

- [x] **a silence is a real queue entry.** `SearchItem::Silence {
      secs }`, resolving to `av://lavfi:anullsrc=d=N`, so mpv loads it
      as a file like any other and `PlayQueue`'s invariant (`entries`
      *is* mpv's playlist) holds with no notion of the variant
      anywhere near it. every absolute-index path, the queue view,
      reorder, removal and the desync guards work untouched.
      **a variant, not a `LocalMusic` with an `av://` path.** the path
      spelling costs zero match arms, which is exactly why it is
      wrong: the compiler found **23 exhaustive matches** that each had
      to decide what a silence means, and four of those decisions are
      load-bearing. scrobbling is the one that settles it: a silence
      would have been sent to listenbrainz as a track called
      "silence". starring returns an error, downloading and cover art
      refuse, and `identity_of` deliberately yields an identity whose
      every tier is `None`, so **no per-identity eq or volume fires
      when a silence starts** (a gap must not change how the next
      track sounds).
      the other 19 were genuinely trivial, and are commented where the
      answer is unreachable rather than merely dull (`category`, say,
      which no silence ever reaches because none is in the corpus).
- [x] **measured: mpv honours `d` and lies about `duration`.** the
      recorded decision said mpv "already loads" the source and left
      it there. it does, and it plays for exactly the length asked
      (timed: `d=3` and `d=6` differ by three seconds of wall clock),
      **but it reports `duration` as `00:00:01` whatever `d` says**,
      at every spelling tried (`d=`, `duration=`, with and without an
      explicit rate and channel layout). loading is not honouring.
      consequence, and it is a ui one: **anything drawing the length
      of a silence must read `Silence { secs }`, never mpv's
      `duration`**, or a 15m gap draws as a one-second track. pinned
      by an `--ignored` test that drives real mpv and asserts both
      halves, so an mpv or ffmpeg upgrade cannot change it quietly.
- [x] **saved queues round-trip a silence.** `queues::Kind` gained
      `Silence` and `Entry` a `secs`, both `skip_serializing_if`, so a
      queue with no silence in it serialises byte identically to
      before (pinned). a silence is the one kind the pre-kinds
      inference could never guess, having neither a path nor a server,
      so it loads only because the discriminant landed first.
- [x] **the display decision worth not undoing.** a silence row keeps
      its duration when *not* hovered, which no other row does. for
      every other item the title is the identity and the artist is
      decoration; for a silence the duration *is* the identity, and a
      column of bare "silence" rows is unreadable. `[z]` for the kind
      code and `g` (generated) for the source code, since it came from
      neither a file nor a server. `ui::progress::fmt_span` prints a
      span as "15m"/"1h30m"/"1m30s", distinct from `fmt_time`'s
      fixed-width `m:ss` because a span is a length someone chose, not
      a position in a track, and sits nowhere near a scrubber.
- [x] **`silence(15m)` and `silence(15m, front)` exist.** the
      position parameter is `add_to_queue`'s own, rather than a second
      way to say the same thing.
      it takes a new **`Ty::Span`, not `Ty::Duration`**, and that was
      not pedantry: `Duration` accepts `off` (for `sleep(off)`), so
      `silence(off)` would have parsed cleanly and then done *nothing
      at all, silently*, since there is no silence of no length. a
      span is a length that is something; a duration is a timer that
      can be cancelled. `silence(0s)` is refused at parse time for the
      same reason, "a silence needs a length".
      appended unshuffled even with shuffle on: a silence is
      positional by nature, and the one thing worse than a gap in the
      wrong place is a gap somewhere nobody chose.
      `Silence` joined `TrackResolver::request`'s instant list while
      wiring this, which is not cosmetic: `queue(3)` can target a
      queue row that *is* a silence, and without it that spawned a
      thread to "resolve" an entry that is already its own track list.
      two existing guard tests caught the addition by design (the
      pinned completion list, and the one asserting every parameter
      type in the table is covered), which is what they are for.
- [x] **the playing silence drew from mpv's duration.** fixed by
      `core::playing_duration(item, reported)`, which prefers what the
      entry knows and falls back to mpv for everything else; kept
      narrow on purpose, since for a real file mpv is the one that has
      read the container.
      applied at **five** sites, not one: the frame's display binding,
      the crossfade ramp, both scrub-handler contexts (a drag resolves
      its seek target against the duration, so scrubbing a gap landed
      nowhere), and shellswim's bar, which matters most because
      shellswim *is* mostly that bar.
      the crossfade one was not cosmetic: with a 1s duration
      `remaining` stayed negative for the whole gap, so it wrote a
      volume to mpv **every frame for fifteen minutes** instead of
      over the last few seconds.
- [x] **the gap panel: the cover rect is a countdown while a silence
      plays.** a silence has no cover and never will, so the biggest
      rect on the full player sat blank for the length of the gap. it
      now draws the remaining time large, which is the one thing
      anyone wants off that screen during a gap, with `remaining`
      under it in the dimmest shade so a bare number cannot be read as
      elapsed.
      `src/ui/gap.rs`: a hand-authored 3x5 block digit font (nothing
      to reuse; `logos::LOGOS` is figlet art for the word "WIM" and
      has no digits). three scales, picked largest-that-fits, and a
      plain `m:ss` fallback when even the smallest will not fit. the
      scale ladder keeps x at twice y throughout, which is not taste:
      a cell is about twice as tall as it is wide, so a 3x5 glyph at
      one cell per pixel is physically 3 wide and 10 tall, a sliver.
      the caption is dropped *before* the digits shrink, since the
      number is what was asked for.
      only a gap claims the rect, `art = off` still means off, and it
      draws in the `Full*` modes too because the host has no image to
      overlay for a gap. all four pinned.
      **looked at, not just tested.** `ui::gap::eyeball` prints the
      panel at three sizes (`cargo test --lib eyeball -- --nocapture`)
      and reading it found a real bug no assertion here would have
      caught: the `1` glyph was right-aligned in its three-column slot
      like a seven-segment display's, which left a four-cell notch and
      shoved the whole number visibly off-centre against the caption.
      centred now. the print test asserts a floor as well, so it is
      not a test that cannot fail.
      still worth a human's eyes on a real terminal for the one thing
      a buffer cannot show: whether the countdown reads as calm rather
      than alarming at full size.
- [x] **three racy tests of my own, fixed, worth the note.** the resolver tests mixed a real `request` (which spawns
      a worker that tries to reach the fixture client's host) with a
      staged `deliver`, so the real reply raced the fake one. they
      passed for two sessions because dns failed slower than the test
      ran; `Intent::queue_bound` then let the real error batch through
      the epoch filter and one of them started failing on a dns error
      instead of its assertion.
      they `deliver` only now. one consequence had to be handled
      rather than accepted: dropping `request` made
      `two_albums_queued_in_a_row_both_arrive` **toothless against the
      per-request bump**, since with no request the epoch never moved
      either way. split into two tests, one asserting `request` leaves
      the epoch alone (which catches that mutation) and one asserting
      `poll` hands both batches over. both mutation-checked again
      after the split.
- [x] **`gap`, the default pause between tracks: the mechanism.** a
      property of the session applied at the join, as guessed, and the
      hook is mpv's own `keep-open`, which was found by measuring
      rather than reasoning.
      `keep-open=always` makes mpv **hold at the end of every file
      instead of rolling into the next one**, which is the right place
      for the pause: it lands in the join, not a few milliseconds into
      the next track. the obvious alternative (notice the advance, then
      pause) is a frame of ipc too late and leaks the start of the next
      track as a tick.
      **three things measured that each would have shipped broken.**
      `always`, not `yes`: `yes` holds only on the *last* entry, the
      exact opposite of what this needs. mpv **stays paused across
      `playlist-next`** once it has held, so whoever advances must
      unpause too, or the first gap is also the last. and under
      `keep-open=always` mpv **never goes idle at all**, so
      `idle-active` stops firing and the end-of-playlist and loop-all
      handling never runs; verified by watching the pushed property
      changes, where `idle-active` stayed false through two files
      ending.
      so `keep-open` is **toggled, not pinned**: on only while a gap is
      wanted *and* something is queued after this track. the last
      track therefore ends exactly as it does today and the loop-all
      block is untouched. the written value is cached, so evaluating
      it every frame costs mpv nothing until the answer changes.
      `core::BetweenTracks` is the state machine, pure apart from the
      `Instant` it is handed. the part worth knowing is the
      **re-arm latch**: `Release` sends `playlist-next`, but
      `eof-reached` stays true locally until mpv's change arrives a
      frame or two later, and without the latch that stale `true`
      starts a second gap at once, so every track gets two. two tests
      cover it and both fail when the latch is removed.
- [x] **the `gap(10s)` command and the countdown.** `gap(10s)` /
      `gap(off)`, live for the session the way `crossfade` is, with
      `config::gap_seconds` as the value it starts at.
      it takes **`Ty::Duration`, not `Ty::Span`**, which is the whole
      reason those two types are separate: `off` is meaningful here
      and meaningless for `silence`. `gap(off)` also drops a hold
      being waited out right now rather than letting it expire, since
      "off" should not mean "off after this one".
      **the ux problem worth naming: during a gap mpv really is
      paused**, so the bar showed a bare pause glyph and the honest
      reading of the screen was "something stopped this". the
      countdown replaces that glyph (`‖ 7s`, dim, since nothing is
      moving) on the bottom bar, and the full player's transport says
      `‖ gap 7s` instead of `‖ paused`. ceil, not round, so any time
      left reads as at least 1s and a stuck-looking "0s" never sits
      there.
      **the gap deliberately does *not* take the album art panel**,
      which `silence` does. ten seconds of countdown thrown over the
      cover on every single track would be worse than the problem it
      solves; `silence` earns the panel because it is long and has no
      cover to displace. the two therefore read differently on
      purpose.
      **pressing play during a gap skips the rest of it.** not a
      flourish: mpv is paused at the end of a track that has already
      finished, so unpausing resumes nothing and mpv would just report
      the same end again. routed through `core::toggle_pause` so the
      keybind, the bar click and enter on the player cannot disagree;
      `activate_transport` already claimed to be the one place a
      transport control is acted on, and now the gap case is there too.
      looked at, not just tested: `controlsbar::tests::print_the_gap_bar`
      prints the bar both ways, which is how the spacing and the
      scrubber absorbing the three extra cells were checked.
- [x] **eyeball pass on the five most on-screen widgets with no
      `mod eyeball`**: helpbox, miniplayer, lyrics_view, header,
      searchbox. printed at realistic and awkward sizes and read,
      which is the only verification available headlessly and the one
      that keeps finding things assertions do not.
      - [x] **helpbox**: `height / 2` is not the centre at even
        heights, so a two-row bar put "? help" on the *bottom* row with
        the blank one above it, hugging the frame; top-biased
        `(h - 1) / 2` now. a clipped label was hard-cut to "? h",
        which reads as a label in its own right rather than a cut one,
        so it ellipsises. and the key is BRIGHT with the word DIM,
        matching the controls bar an inch to its left, instead of one
        flat shade. splits on the *last* space so a key rebound to
        `space` stays one key.
      - [x] **miniplayer**: one left-aligned sentence with 29 dead
        columns at 72 wide, and narrowing it cut the hints to
        "(space: paus…" while the title still had room. hints are the
        right end of a bar now and drop whole below an 18-cell title
        floor; glyph/title/hints are three brightnesses over the
        NEAR_BLACK rule; and squeezed to one row it spends it on the
        title, where it used to spend it on the rule and say nothing
        about what was playing.
      - [x] **lyrics_view**: the only text pane on screen running
        column 0 to the last cell, while every list beside it indents
        by its selection marker. a cell of gutter each side, with
        `wrapped_len` insetting identically so the host's scroll clamp
        still counts the rows that actually get drawn. the freed right
        gutter carries `▴`/`▾` when verses sit off-screen, which a
        scrolled pane previously gave no sign of at all.
      - [x] **header** (heads ten panes: browse, queue, lyrics, help,
        eq, settings, login, nav, keybinds, suggestions): the joined
        "title │ instructions" was truncated blind, so a 28-cell pane
        was headed "j/k: …", six columns of ellipsis where the verb
        should be. whole hints drop now, back to the last complete one
        or to nothing so the title takes the row.
        **and the first fix was wrong in a way only printing caught.**
        it kept any fragment with two cells of "action", but a cut
        landing *inside* a key leaves no `": "` at all, which fell into
        the "hint with no key prefix" branch and was kept: 40 cells
        rendered "j/k: scroll, ente…", a half-key naming nothing. the
        fragment alone cannot tell that from "enter to advance" cut
        short, so `names_an_action` takes the original hint too. pinned
        over every width from "fits" down to "nothing fits", and
        mutation-checked; no assertion anyone had written would have
        caught it, because every case thought of cut *after* a colon.
      - [x] **searchbox**: the query sat flush against the `│` it was
        inside, the one box in the ui without the inset
        `draw::framed_inner` documents, and `┌┤` butted two
        line-drawing glyphs together. a cell inside both borders, a
        `─` lead-in when the box can afford one (additive, so the
        squeezed 18-column case keeps its label rather than losing
        it), the cursor no longer lands on the right border at widths
        with no interior, and the label is measured in cells rather
        than chars.
- [x] **clearing the queue was undone by loop-all.** `Op::ClearQueue`
      went through `PlayQueue::replace_upcoming(vec![])`, which relays
      the *same* session out again. that is right for a shuffle (mpv is
      cleared and refilled with the same tracks) and wrong here,
      because the session is what loop-all restarts from: with
      `loop(all)` on, `clear_queue()` (or deleting rows with `d` in the
      queue view, which never touched the session either) left
      `drain_decision(0, All, false)` returning `Restart`, so
      everything the user had just deleted came back the moment the
      current track ended.
      `clear_upcoming` and a `forget` helper now take the dropped
      tracks out of the session. **already-played tracks stay**: the
      user emptied the queue, not the history, so loop-all still
      restarts from what actually played. and `forget` drops *one*
      occurrence, because a track queued twice and deleted once is
      still queued once.
      three tests, three mutations: restoring `replace_upcoming`,
      dropping the forget from `remove`, and making `forget` a
      `retain` that takes every copy.
      **`Op::RemoveFromQueue` also had the ordering backwards.** it
      mutated `entries` and then asked mpv, so a refused
      `playlist-remove` returned with the index map one shorter than
      mpv's playlist, every later index off by one, and the cursor
      clamp (which sits after the early return) never running. mpv
      first now, which is the ordering `resync_upcoming` documents.
      found by an audit, not by use; see the audit section below.
- [ ] **`gap` is unverified end to end against real playback.** the
      mpv mechanism was measured on its own (hold, advance, unpause,
      and `idle-active` going quiet), and the state machine and both
      bars are unit-tested, but **nobody has played two real tracks
      with `gap(10s)` set** and listened to the join. the things to
      watch: whether the hold is audibly clean at the boundary, that
      the last track still ends the queue rather than hanging, and
      that loop-all still restarts (the `keep-open` toggle is what
      protects both, and it is the part most likely to be subtly
      wrong).

### the input-path freeze audit (complete, 2026-10-31)

The lyrics freeze prompted this: grep every network call reachable from
`dispatch_action`, `run_shellsong_op` and the key handlers, and check
whether it sits inside a thread. **Three more freezes, all confirmed on
the input path, none inside a thread.** Every fix needs `main.rs`,
which the library split owns tonight, so they are recorded with line
numbers and the pattern to use rather than half-done.

The pattern is settled, twice over: `browse::Loader` (merged) and
`lyrics::Loader` (built, waiting on two call sites). One generation
counter bumped per request and on `invalidate`, checked by the worker
before it sends and again in `poll`. Copy that shape; do not invent a
third.

- [x] **queueing an album or artist no longer blocks** (`Action::Queued`
      and `add_to_queue(front, ...)`). `EnqueueResolver` resolves the
      tracks off-thread; this was the case the audit named, since for
      an artist `resolve_tracks` is a `getArtist` plus a `getAlbum` per
      album.
      **the commonest action stays instant**, which is the part worth
      keeping: `request` answers `Resolution::Ready` for a local file
      or a single song, because those are already their own track list
      and making them wait a frame for a thread would be a regression
      dressed as a fix. only an album, artist or playlist becomes
      `Fetching`.
      the stale guard matters more here than anywhere else it has been
      used: elsewhere a late reply draws something out of date, but a
      late batch **mutates the queue**, appending tracks against a
      queue that is gone. pinned by two tests that both fail when the
      generation check is disabled.
      `append_resolved`/`front_resolved` are shared by the instant path
      and the drain, so the two cannot report differently; `front`
      still inserts reversed so a batch keeps its own order.
      shellswim's `Host` grew a `core_ctx!` macro while wiring its
      drain, for the same reason main.rs has one: the context borrows
      most of `Host` plus two locals, and a method cannot return a
      struct borrowing locals it just made.
- [x] **that resolver shipped with a bug: queueing two things in a row
      silently lost the first.** found reading it back, not from a
      symptom. the generation guard was copied from `browse::Loader`
      and `lyrics::Loader` wholesale, **including the per-request
      bump**, which is right for those two and wrong here. they fill
      one pane, so a newer request supersedes an older one; queueing
      *accumulates*. so queueing an artist (slow fan-out) and then an
      album while it ran made the album's request disown the artist's,
      and the artist's tracks were dropped on arrival with nothing
      said. the realistic way in, and it would have read as "wim just
      ignored me".
      the fix is the guard counting **queue replacements instead of
      requests**: `request` reads the epoch, only `invalidate` bumps
      it, and every worker checks the epoch it was spawned in. any
      number of requests can now be outstanding at once, which is what
      queueing means.
      `invalidate` **was dead code until now** -- written for exactly
      this and never called, which is how the per-request bump went
      unnoticed: it looked like the guard was doing its job. wired into
      the two places the queue is replaced wholesale (`start_session`)
      or emptied (`Op::ClearQueue`), so a batch cannot refill a queue
      the user just cleared.
      the test that pinned this is gone, because it pinned the bug as
      correct ("a batch from a superseded request never arrives", which
      is precisely the wrong rule). replaced with both albums landing
      in order, mutation-checked against the old `fetch_add`, plus one
      that `invalidate` disowns *every* batch in flight rather than
      only the newest, which only matters now that several can be.
- [x] **`Action::Play` on an album or artist resolves synchronously:
      wrong, there is no such freeze.** this item said the resolver
      shape applied and asked for a `play_resolved` and a `Play`
      intent. checked before building any of it, and the premise does
      not hold: **the arm above it takes every page**. `Action::Play`
      on an `Album`/`Playlist`/`Artist` is matched first and handed to
      `Surface::open_browse`, which goes through `browse::Loader` and
      was made async already. traced all five producers to be sure
      (two in `app.rs`'s enter handling, `Op::Play`, `Op::OpenPage`,
      and the suggestions row in main.rs); every one of them hands
      either a page, which is intercepted, or a single song. so the
      resolving arm only ever saw something that resolves to itself.
      what was actually there was **a duplicated body**, not a
      network call: the arm was a copy of `start_session` with
      `minimize: false`, and `resolve_tracks` on a track list it
      already had is what made it read like a round trip. it calls
      `start_session` now, 34 lines down to 4. `start_session`'s own
      doc claimed to be "the same set `Action::Play` fires", which was
      aspirational while the copy existed; corrected to say it is
      called.
      the lesson is the one this file already applies to QUESTIONS.md:
      **an entry asserting a bug is a claim and decays like any
      other.** this one named a fix precisely enough to sound verified
      and would have had me build an intent, a resolver path and a
      drain for a call that cannot block.
- [x] **`trigger_downloads` resolved synchronously too** (download
      selected on an album or artist) -- and with `Action::Play`
      disproved above, this was the **last** blocking network call
      reachable from a keypress.
      it shares the resolver rather than growing a fourth loader,
      because expanding an album is the same `resolve_tracks` fan-out
      either way; `Enqueue` became `Intent` (`Back`, `Front`,
      `Download`) and `EnqueueResolver` became `TrackResolver`, since
      a variant called `Download` on an enum called `Enqueue` is a
      lie. `trigger_downloads` split into `download_tracks` (the part
      after resolution) and `download_resolved` (the same with the
      `DownloadCtx` built from `CoreCtx`, so the three call sites do
      not each write that literal).
      **sharing it introduced a cross-concern worth knowing about**:
      `invalidate` means "the queue these were resolved for is gone",
      which is true of a queue batch and says nothing about a
      download, whose tracks go to disk. unguarded, `Op::ClearQueue`
      would have cancelled a download asked for seconds earlier. so
      `Intent::queue_bound()` gates both the worker check and `poll`,
      and a test pins it (fails if the gate is dropped).
      **a ux bug found on the way**: both keypress sites did
      `let _ = trigger_downloads(...)`, discarding the status, so
      asking to download something already cached looked like a dead
      key. they now report, but only when nothing started: each
      started track already pushes a progress toast, so "downloading 3
      track(s)" over three progress bars is noise. the drain always
      reports, because its fetch already said "resolving ..." and
      going quiet would leave that as the last word.
      the two key sites also collapsed into one `download_selected`,
      which they should have been from the start.
- [x] **`open_starred_view` fetched starred songs from every configured
      server synchronously** -- fixed by routing it through
      `browse::Loader` rather than building a fourth loader, which is
      what the audit guessed would be right. it already fanned out
      across servers, but **joined before returning**, so the keypress
      paid for the slowest one.
      `Loader::open_starred` is its own entry point, since starred is
      not a `SearchItem`, but it shares the generation guard and
      `Load::Tracks`. two consequences worth having: the call sites
      stop needing their own `invalidate()` (opening it bumps the
      generation, so anything in flight is disowned by construction),
      and **local favorites show immediately** because they are a file
      read, not a round trip; there was never a reason to make them
      wait for a network answer they do not need.
      clients are resolved in `core` and passed in, because
      `client_for` lives there and `core` already depends on `browse`;
      resolving them in the loader would have been a cycle.
      pinned both ways: the view comes back with the local half in it,
      and a reply from the album that was open before does not fold
      into the starred listing. the second fails if the generation bump
      is removed.
- [x] ~~**`open_starred_view` (main.rs ~312)**~~, from shellsong (~1543)
      and the starred key (~6923). one round trip per server on a
      keypress, in series with nothing else to look at.
- [x] **`toggle_star` is a server round trip on the keypress** -- fixed,
      and **optimistically** rather than with a loader, which is the
      right shape for this one: the local set flips and the next frame
      shows it, while the server hears on a thread. a refusal comes
      back on `star_tx` and the flip is taken back with one honest
      line.
      `StarFailed` carries **which way the flip went**, not just the
      id, because the id alone cannot say how to undo it; a refused
      star has to be removed and a refused *unstar* re-inserted, and
      the two are opposite operations on the same id.
      reverting rather than retrying, deliberately: the user saw it
      take, and a retry loop against a server that is saying no is
      worse than one line admitting it. local files stay synchronous,
      since they are a file write and not a round trip, pinned by its
      own test.
      drained in both binaries (wim's loop beside the lyrics drain,
      shellswim's `Host::pump`), so the two cannot diverge on what a
      refusal does.
- [x] ~~**`toggle_star` (main.rs ~244) is a server round trip on the
      keypress**~~ for a remote song (`star`/`unstar`), from the key
      handlers (~6606, ~7039) and shellsong (~1944). the local-file
      branch is a file write and is fine.
      this one wants different treatment from the other two: starring
      should feel instant, so the right shape is **optimistic** (flip
      `remote_starred` and redraw at once, send in the background, and
      revert with a notification if the server refuses) rather than a
      loader the user waits on.

Checked and genuinely fine: `fetch_navidrome_everything` and
`test_navidrome_connectivity` are already on their own threads, the
enrichment and catalog fetches are background, `search3` runs in the
debounced search thread, and the local-favorites path is a file write.

- [x] **pressing the lyrics key freezes the app on the network.**
      `lyrics::Client::lookup` is a round trip to lrclib and it is
      called **synchronously from the input path** at two sites in
      `main.rs`: the `OpenLyrics` key handler and the shellsong op. the
      same class of freeze as the album open fixed earlier today, and
      just as visible.
      `lyrics::Loader` is built and tested: one generation counter,
      bumped on every request and on `invalidate`, checked by the
      worker before it sends and again in `poll`, so a reply for a
      track the user has moved past is dropped rather than appearing
      over the new one. both stale paths pinned, and the guard
      mutation-checked by deleting the check and watching exactly those
      two tests fail. `cfg_attr(not(test), allow(dead_code))` until
      wired.
      both call sites were wired in `15ea442`; there is no blocking
      `lookup` left in either binary.
      **the audit it asked for is finished, and the input path is
      clean.** the guess that a third would turn up was low: there
      were four more (star, starred, queueing, download selected),
      every one of them the same shape, a client called straight from
      a key handler. all fixed. the one it was most confident about,
      `Action::Play` on an album, **was not real at all** (see above),
      which is the more useful half of the lesson: the audit found
      both more freezes than it expected and a freeze that did not
      exist.
      what the shape has in common, worth checking any new key
      handler against: a `subsonic::Client` method, or anything
      calling `resolve_tracks`, reachable from `dispatch_action` or
      `run_shellsong_op` without a channel in between.

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
      - [x] **the ranking is a list of corpus indices.**
        `score_chunk` carries a per-chunk `base` and pushes
        `(score, base + nth)` instead of `(score, &item)`;
        `rank_borrowed` became `rank_order(items, scored) -> Vec<u32>`
        and is reintroduced on top of it for `search_names`;
        `Corpus::rank_indices` is the new entry and **`search` is now
        exactly that plus the clone**, which is the only reason this
        is safe to introduce (a caller switching between them cannot
        see a different order; a test fingerprints both).
        the clone itself is still there, because the holder is still
        `App`; what is gone is the *need* for it. an index list can be
        held across an event loop where `Vec<&SearchItem>` cannot, so
        the next step (`App` holding the order) no longer needs a
        self-referential borrow of `corpus`.
        **`Corpus` grew a revision counter, and that is the load-
        bearing part.** an order is indices into one revision: a later
        `drop_local` shifts every index past the first removal, so a
        held order stops being *stale* and starts being silently
        **wrong rows**, which plays the wrong song rather than
        failing. `rev` is bumped at all three `items` write sites and
        a test pins that a removal bumps it too (the easy one to
        forget; mutation-checked by deleting the bump in
        `drop_local`).
        two more tests. every ranked index resolves, because
        `rank_order` indexes `items` directly and an out-of-range one
        is a panic on a keystroke. and the threaded path reports the
        same *indices* as the sequential one, not just the same rows:
        zeroing the per-chunk base fails that and the older
        item-fingerprint test both, but this one says "a chunk
        reported the wrong base" instead of "the list changed".
      - [x] **the art/picker group's four locals ->
        `graphics::ArtProbe`.** `picker`, `picker_probed`,
        `art_mode`/`art_mode_pending` and `warned_no_graphics`, plus
        the probe thread and its channel, which main.rs had no reason
        to hold once the answer stopped landing there. home is
        `graphics.rs` unchanged: every method is still "ask the
        terminal, or decide from what it answered", and `cycle` folded
        `warn_no_graphics` in, which had no other caller.
        **the rule is a method each**: the answer is taken exactly
        once, by *dropping* the receiver when it arrives rather than
        latching a `picker_probed` flag next to it, so a second apply
        is not representable; it may only move a mode nobody has
        cycled by hand since startup (the probe can take a second,
        which is long enough to press 'i', and an upgrade then undoes
        a deliberate keypress); and the capability it judges by is the
        protocol, never `picker.is_some()`. the three sites that each
        re-derived that from `cfg.album_art` now ask one object that
        holds it.
        **the bug it would have prevented was in the arm nobody looked
        at**: `Err(Disconnected)` set `picker_probed = true` and
        skipped the mode decision with it, so a probe thread that died
        before sending left a `full-*` config silently in ascii with no
        alert saying why. `poll` treats a dropped channel as the answer
        it is (no protocol), alert included.
        seven tests, seven mutations, each caught by the one it was
        aimed at: deleting the `mode == pending` guard, keeping the
        receiver after an answer (two answers would find `mode` back at
        `pending`, since the first degraded it to exactly that, and
        read as a free upgrade), `Disconnected` waiting instead of
        answering, dropping the once-per-session `warned` latch,
        `is_some()` as the capability, the alert unconditional, and
        `Empty` counting as an answer.
        main.rs 4814 -> 4759. the only group left is the playback
        timers, which stay loose on purpose (four unrelated features,
        one bag would name nothing).
      - [x] **the revision is an identity, and a `Ranking` carries
        it.** an adversarial review of the slice above found the
        counter did not do the job it was added for, and it was right:
        `rev` was a per-instance mutation count starting at 0, so two
        corpus states could share one. not hypothetical, because
        onboarding's library-paths step replaces `app.corpus`
        wholesale: a fresh corpus that indexes as many items as the old
        one reached the same number, a held order compared equal, and
        every index then named a different row. the derived `Clone`
        copied it too, so two clones mutated apart while still
        claiming to be the same state. it is now drawn from a
        process-global `AtomicU64`, with `Clone` hand-written to take a
        new one.
        **and the order no longer travels without it.** `rank_indices`
        returned a bare `Vec<u32>` while its own doc claimed the
        revision came along, which left every caller to remember the
        pairing: the exact failure the revision exists to prevent.
        `Corpus::rank` now returns a `Ranking { rev, order }` whose
        `row`/`rows` resolve against a corpus and **refuse on a
        mismatch**, returning no rows rather than a partial list (a
        short list reads as "the search found less", which is a
        plausible lie).
        three more fixes from the same review. `search`'s
        `filter_map(item_at)` could never filter, since `rank_order`
        already resolved every index to ask its category, so it
        advertised a graceful degradation that would actually have
        desynced `search` from `rank`; it indexes directly now.
        `(u32, u32)` became `struct Scored { score, at }`, because the
        reference version could not confuse the two and the index
        version silently could (five sites read one or the other and a
        swap compiled). and `threaded_scoring_reports_the_same_corpus_indices`
        was **vacuous on a one-core box**: its guard was
        `items.len() > thread_count()`, which passes with a single
        chunk at base 0, correct by construction; it now skips loudly
        on `thread_count() < 2`.
        new tests, all mutation-checked: two corpora built identically
        have different revisions (caught by restoring the derived
        `Clone`), and a ranking read against a mutated corpus yields
        no rows (caught by deleting the check in `row`).
        still open from that review: `group_boundaries` takes
        `&[SearchItem]` and is called from the results list, so the
        draw-a-screenful caller needs an index-based counterpart or it
        re-materializes exactly what it set out not to clone. and the
        onboarding site that replaces `app.corpus` does not
        `resync_results`, which is merely stale today and becomes a
        wrong-rows trigger the moment `App` holds the order.
      - [x] **the api change itself: `results` is private.** fourteen
        external reads across main.rs, core.rs and both shellswim
        files now go through `results()` / `result(i)` /
        `result_count()` / `has_results()` / `selected_result()`, and
        the four internal `self.results.get(self.selected)` sites
        through the last one.
        the split is the whole point rather than ceremony: `results()`
        is the **only** accessor that hands out the list, so it is the
        only one a lazy `results` has to build anything for; every
        other reader asks for one row or a count, which a ranked index
        list answers without constructing a `SearchItem`. the sites
        that *looked* like they needed the slice -- the result counter,
        `G`'s clamp, `Target::Index`, the sidebar's emptiness test --
        all turned out to be those, leaving the drawing path as the
        real consumer.
        two tests. the count and the slice agree today, pinned now
        **because the lazy version computes them from different
        things** (the index list vs the prefix built), so the day they
        can disagree this fails. and a selection past the end reads as
        no row: `selected_result` is `Option` on purpose, since
        clamping there plays whatever sits at the clamped index, a
        wrong song rather than a missing one. mutation-checked by
        making it clamp.
        what is left for the item proper: `rank` returning indices and
        `resync_results`' identity scan, which still touches every
        row.
- [x] **catalog parse beyond ~100k rows: measured, and deliberately
      not parallelised.** 34k, 100k and 300k rows timed. three measured
      reasons against the fan-out: its own overhead with no parallelism
      is **25-30% worse than not doing it at 34k**, so it would need a
      row-count floor this machine was too loaded to calibrate
      honestly; at the real size the win is tens of ms on a thread that
      already overlaps the mpv spawn; and at the size that was the
      worry it does not change the answer, since 100k rows is 300ms+
      sequential and still 220ms+ threaded, both far past an mpv spawn.
      that scale wants the parse off the startup path entirely rather
      than divided by cores. the candidate implementation sits next to
      the measurement as the evidence.
      **what shipped instead is the better find:** `CatalogRow` was an
      internally-tagged serde enum, which serde cannot deserialize in
      one pass (every row is buffered into a `Content` tree, allocating
      per field, before the variant is known). a flat struct with a `k`
      discriminant and `Cow` fields is **identical bytes on disk and a
      fifth cheaper**, with no machinery, and the old shape is kept as
      an oracle.
- [x] **`Corpus::remote_clients`: left an uncached walk, and my own
      assessment of it was wrong.** i recorded it as "a guard against
      drift onto a per-frame path, not a present emergency", on the
      premise that a few milliseconds per keypress are invisible.
      **the walk was never the cost.** the `HashSet<String>` dedup
      allocated a `String` per *item* while deduping over one or two
      servers; comparing `&str` against a `Vec` of servers found so far
      allocates nothing. 40k items went 4.9ms to 0.8-1.8ms and 150k
      went 24.3ms to 3.9-4.9ms, so the premise was borderline at 40k
      and 24ms at 150k would have been a visible per-keypress stutter.
      no memo, deliberately: it is a pure derivation of the corpus, so
      a cache would need a hook on every removal path and a missed one
      would name a server with nothing indexed, which breaks the
      documented contract rather than merely dropping a frame. the
      guard is a source scan asserting no `src/ui/*` file asks which
      servers are live, since what needs preventing is a new call site.

- [x] **album/artist identity joins on ids, with names as the
      fallback** (docs/identity.md). the three losses names caused are
      each fixed and each pinned: two same-titled albums by one artist
      stay two records, a split credit ("Bladee & Ecco2k") rolls up
      under the artist the server named by id, and a compilation's
      differently-credited tracks share their album's cover key
      instead of refetching it once per track.
      the ids could **not** go on `SearchItem`, and the reason is
      mechanical: a new field on an enum struct variant makes every
      literal construction of it a compile error, and rust has no
      defaulted fields or record update for variants, so "optional and
      defaulted" cannot rescue a construction site. main.rs holds two
      of those literals (plus one in its tests), and the same is true
      of `subsonic::{Song, Album, Artist}`, `subsonic::SearchResult`
      and `cache::ServerCatalog`, all of which main.rs's tests
      construct. so the ids ride in `subsonic::IdIndex`: three maps per
      server, shared between every `Client` for it exactly the way
      `NAV_CACHES` is, reached through the `Arc<Client>` every remote
      item already carries. docs/identity.md lists the exact main.rs
      edits that would turn them into real fields, and what of the
      index can then be deleted.
      the rule everywhere is: prefer an id; fall back to the name, but
      only onto a row that is not already some other id's; never merge
      two different ids. the middle clause is what keeps local files
      (no ids at all) joining exactly as before, and the last is what
      stops the names re-merging what the ids just separated.
      the one cost worth knowing: every cover already on disk is filed
      under a name key, so each album is fetched once more and
      refiled; the old files age out through the sweep that already
      runs.

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
- [x] **cover art in `ArtMode::Real`** (since split into `full-bw`/
      `full-color`)**.** the player has its own
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
## the overnight run (2026-10-31): all four merged

Every branch is in. **1001 tests** across three targets (the lib plus
the two binaries), clippy `-D warnings` clean, fmt clean, both binaries
build and run.

### shellswim and the library seam

`core::CoreCtx` plus `core::Surface` (`docs/library-seam.md`).
`CoreCtx` holds config, mpv, session, queue, cache, favorites,
notifications, scrobble, the channels, vars, bindings and console; it
**does not know about** focus, the browse view and its loader, the art
protocol, the six menu-open/selected pairs, lyrics state, suggestions
or the queue view.

The 17 places that did reach pane state ask `Surface`, **a trait whose
every method declines by default**. A trait rather than a drained
request queue, deliberately: wim had to stay bit-identical, and several
effects are ordered *inside* an op (`view(starred)` invalidates the
art, builds the view, then moves focus), which a queue drained
afterwards would reorder. `view()` splits on the language's own
`ViewTarget::is_screen()`, so the five listings print in the core,
identically in both binaries.

shellswim declines each pane view **by name, pointing at wim's key**
(`shellswim has no equaliser pane; wim's 'e' has it`), and
`dry(view(eq))` still describes it fully, because `dry` lowers to notes
and never reaches the op table.

**Measured, isolated XDG, machine under load:** wim's first frame
364ms, shellswim's 420ms, of which `Mpv::spawn()` is ~400ms in both.
Everything shellswim itself does costs ~4ms; it is usable in
single-digit ms and then waits for mpv. **The obvious next move,
leaving the prompt usable before mpv answers, is not done** and is the
single biggest win available there.

Also measured and worth knowing: in wim the graphics probe answers at
**1361ms, a full second after the first frame**, as a second reader on
stdin. Skipping it on terminals that implement no protocol (done
earlier) removes that for alacritty; elsewhere it still races.

- [x] **a real find from actually running it:** `-c` drew a frame and
      exited, and leaving the alternate screen **wiped the line it had
      just written**. Fixed by making one-shot never touch raw mode at
      all (plain stdout/stderr, non-zero on failure), which also makes
      it the one path here **testable with no pty** and is how every
      decline above was verified rather than assumed.
- [x] **the merge caught something that would have shipped broken.**
      master's language work landed in the two functions the split had
      just moved, giving five conflicts. Ported rather than discarded,
      and the audit was a diff of master's `main.rs` against the branch
      for unaccounted `Op`/`ViewTarget`/`Step` arms. The one that
      mattered: **`shellsong_queued`'s promotion**, without which a
      per-identity eq binding would be synthesised on track change and
      then never run.

- [x] **wire `lyrics::Loader`.** done, in both binaries. all three
      blocking call sites are gone: wim's lyrics key, wim's
      `view(lyrics)` op, and shellswim's `print_lyrics`. each now
      requests and returns at once, and the reply lands in the per-frame
      drain each binary already had (next to the subtitle drain in wim,
      inside `Host::pump` in shellswim). `core::lyrics_text` is shared,
      so the pane and the console cannot disagree about one reply, and
      the api's own `instrumental` flag still decides rather than "both
      fields happened to be empty".
      **it surfaced a design smell worth not suppressing.** once the op
      only *requests*, `WimSurface`'s `lyrics_lines`/`lyrics_track`/
      `lyrics_scroll` became write-only: the drain sets the loop's own
      locals directly, so routing the reply back through the surface
      would have been indirection for its own sake. clippy caught them
      as never-read; the three fields are removed and
      `open_wim_lyrics` no longer takes a surface at all. the seam got
      smaller, which is the right direction for it.
      my first attempt also inserted the shared helper **into the middle
      of `artist_tracks`**, splitting that function so the helper was a
      nested item and unreachable from outside the module. it compiled
      as far as the call site. worth remembering that a
      "insert before the next blank line" edit can land inside a
      function body.
- [x] **one commit on that branch should have been two** (`ede72c0`):
      the shellswim files were staged when the merge was resolved, so
      they went in with it. The message says so. **Unactionable by
      construction** -- the commit is published, and rewriting it is
      not worth it -- so this is checked off rather than left looking
      like outstanding work. Worth not repeating.

## shellswim and the undo tree (maintainer, 2026-10-31)

Two features the maintainer asked for that share one prerequisite, so
the order below is not a preference, it is a dependency.

- [x] **`shellswim`: a second binary that is just the shell** --
      shipped; `src/lib.rs` plus `src/core.rs` are the seam and
      `src/bin/shellswim/{main,host,surface,cli}.rs` the front-end, so
      the "what is missing" below is history now. the
      maintainer's shape: no panes, no sidebar, no results list. a
      full-screen shellsong console with **only the bottom bar** kept,
      showing the track and its scrubber:
      `BLADEE - FLATLINE  --:--  (*)-------------------  --:--`
      and nothing else. everything is done by command. it has to feel
      responsive and deliberate, not like typing at `python3`.
      almost all of this exists already: `ui::shellsong` is the
      console, `ui::controlsbar` is the bar, `Program::step` is the
      interpreter, and `Op` is the whole vocabulary. what is missing is
      **a host that is not main.rs**: `apply_shellsong_op` takes a
      `ShellsongCtx` assembled from ~30 locals in `main()`, so today
      the only way to run an op is to boot the entire app.
      **this crate is bin-only; there is no `src/lib.rs`.** a second
      binary cannot share any of it. so the real work is: extract a
      library (the player core: mpv, queue, corpus, cache, config, and
      the op applier), leave `main.rs` as a thin wim front-end, and add
      `shellswim` as a second front-end over the same core.
      that is **the same work as the decompose-main.rs item below**,
      which until now had no motivation beyond tidiness. it has one
      now: the decomposition is what makes a second front-end possible,
      and shellswim is the proof the seam is in the right place.
- [x] **undo and redo as a tree, not a stack.** the maintainer's words:
      "nice decision trees that should be super smart and dynamic". a
      tree rather than linear history means undoing three steps and
      then doing something new does not destroy the branch you left;
      it becomes a sibling you can walk back to, the way vim's undo
      tree works. thematically right for an app called wim.
      **correction to what this entry first claimed.** it said every
      state change funnels through one choke point,
      `apply_shellsong_op`. it does not, and the difference matters:
      `run_shellsong_op` has 40 op arms, **12 delegate to
      `dispatch_action` and 28 act directly**, and the 28 include
      `ClearQueue`, which is the single most valuable thing to be able
      to undo. hooking either function alone misses half the mutations;
      hooking both double-records the 12. the fix is to give those 28
      `Action` variants so there really is one choke point, which is
      cheapest to do **during the library split**, since that is
      already moving the mutation surface into a core. `Op::describe()`
      does exist for every op, so the labels really are free.
      the full design, including an op-by-op classification into
      snapshot-restorable, irreversible-side-effect (undo must refuse
      with a reason rather than restore around it) and not-recorded, is
      in **docs/undo.md**. a branch of the tree is then *already* a list of ops, which
      is to say **a `.shg` script**: exporting a branch is close to
      free once scripts exist, and that ties the two headline features
      together.
      design calls to make deliberately:
      - **snapshot, do not invert.** the undoable state is small (queue
        entries plus position, eq source and gains, volume, speed,
        loop, shuffle, crossfade), so storing it per node is cheaper to
        write and impossible to get subtly wrong, unlike per-op
        inverses.
      - **some ops cannot be undone and must say so** rather than
        pretending: a sent scrobble, a server-side star, a download, a
        deleted file. mark them, and have undo refuse with a reason
        instead of silently restoring half a change.
      - **restoring a queue while music plays** needs a rule: restore
        entries and position, but do not restart the current track if
        it survives the restore, or undo becomes a skip.
      - the tree view is a real layout problem (branch glyphs,
        timestamps, labels from `describe()`), and it is the part most
        likely to end up barebones; it deserves the ux self-check.
      **the tree itself is built: `src/undo.rs`.** generic over the
      snapshot (`Tree<S>`), deliberately, so none of the semantics
      depend on what wim's undoable state turns out to be; that type
      gets defined when this is wired, where it can be built from a
      real `CoreCtx` instead of guessed at. 18 tests.
      **two corrections to docs/undo.md, both found by writing the
      code rather than reading it again.**
      the doc specified the snapshot as the state *before* the op.
      that works for undo alone and **leaves redo with nothing to
      restore**, because the state after an op is recorded nowhere; a
      tree whose redo cannot move is a stack with extra steps. they
      are after-states now, the root holds the session's starting
      state, and undo/redo are symmetric (parent's snapshot /
      child's).
      that inverted the coalescing rule too. the doc said keep the
      *first* snapshot of a run "since that is the state the user
      wants back"; correct, and with after-states that state is the
      *parent's*, which coalescing never touches. so a run keeps its
      **latest** snapshot, or redo drops you back into the middle of a
      volume walk.
      two coalescing rules that were not in the design at all, both
      found by testing: it must not fold into a node that already has
      **children** (that silently rewrites a state redo can reach),
      and a **barrier never coalesces**, since undo has to name the
      one irreversible effect it stopped at.
      `classify` is an exhaustive match over all 44 ops, so a new op
      cannot be added without being filed into one of the three
      classes. the six ops added since the design was written are
      filed; `Define`/`Unset`/`Remember`/`Forget` are deliberately
      **class 1, not barriers**, because they are wim's own state
      rather than an artifact the user now has, so restoring and
      re-writing leaves nothing behind. the price is that the snapshot
      must carry vars and bindings.
- [x] **wiring the undo tree** -- done, in three slices; every
      sub-item below is closed. two pieces as scoped:
      - ~~**the `Snapshot` type and where it is taken.**~~ **done,
        and the re-pricing changed the answer.** both recorded options
        (28 new `Action` variants, or a re-entrance guard) assumed the
        choke point had to be built. it already existed: **`run_op` is
        private and `apply_op` is the only public way in.** `run_op`
        recurses into itself three times and never back out through
        `apply_op`, so one record in `apply_op` is complete for all 44
        ops with no guard and no refactor. the 28-arm refactor was
        never needed for the shellsong path at all.
        `Snapshot` carries the queue (as current plus upcoming, which
        is what restoring actually needs), base volume, loop, shuffle,
        speed, eq source and gains, crossfade, gap, query, sleep, and
        vars plus bindings.
        **three things it deliberately leaves out, each with a
        reason**: `paused`, because undo starting or stopping the music
        as a side effect of reversing something else reads as a bug and
        pressing play is already the undo of pressing pause; a live
        fade, which is a position in a transition rather than state;
        and mpv's instantaneous volume, which the crossfade ramp moves
        every frame, so capturing it would record a ramp artefact as a
        decision. `base_volume` is the volume that was *asked for*.
        capture is pure, which is why those last two are out: reading
        them means a blocking `get_property` on what is otherwise the
        input path.
        restore follows the doc's rule for the delicate half -- **do
        not restart the current track if it survives** -- by routing
        the two cases through the two functions that already write
        mpv's playlist and the index map together: `resync_upcoming`
        when the same track is playing (mpv keeps its current file,
        only what follows is rewritten) and `load_playlist` with
        `SessionStart::Keep` when it is not.
        a **failed op is not recorded**, since it changed nothing and
        the row would undo to the state it is already in. `undo` and
        `redo` are themselves classified `Ignored`, which is
        load-bearing: recording an undo would make undoing it a new
        branch instead of a redo, and the tree would fork on every
        change of mind.
        pinned by two `--ignored` tests against real mpv, which is the
        only way to exercise `apply_op`: one records an op, undoes it,
        redoes it and checks the tree never grew while walking; the
        other checks a genuinely failing op adds no node. both were
        wrong on the first run and the code was right -- the first
        picked an op that does not fail in the harness, the second
        forgot that planting the root is itself a node.
      - [x] **the keyboard path records now.** hooked in
        `dispatch_action` itself rather than at its three call sites,
        so a fourth cannot be added without being recorded.
        `Action::describe()` gives the rows their labels in the same
        voice as `Op::describe()`, and `undo::classify_action` files
        all eight variants (every mutating one is restorable; the two
        that would be barriers, `Star` and `Download`, are ops with no
        `Action` of their own).
        twelve of `run_op`'s arms delegate to `dispatch_action`, so
        `apply_op` **suspends** the recorder for the length of one op:
        one owner sets the flag, one reader honours it, which is a
        smaller thing to keep straight than the two independent hooks
        the design originally weighed. mutation-checked -- flipping
        `suspend(true)` to `suspend(false)` makes exactly
        `a_delegated_op_is_one_row_not_two` fail.
        **a real bug found by the first test failing**, and worth
        knowing generally: several actions **do not carry their own
        mutation**. `app.handle_key` has already flipped
        `loop_mode`/`shuffle`/`speed` by the time the matching
        `Action` reaches `dispatch_action`, whose arm only tells mpv
        about a change `App` already has. so a "before" snapshot taken
        there is really an "after", and planting the root lazily (as
        `apply_op` does) would have made undoing the *first* keypress
        of a session restore the state it had just left. the host
        plants the root once, up front, before any key is read. the
        test that found it now uses `Action::Queued`, which does carry
        its own mutation, and says why.
      - [x] **`activate_transport` was a duplicate, and is gone.** `core::run_op`'s
        `Op::Go(BottomBarField)` arm carries a `Field` -> `Op` match
        whose comment says it reuses the commands "rather than keeping
        a second copy of the bar's own match". `activate_transport` in
        main.rs **is** that second copy: six of its seven arms
        reimplement `CycleLoop`, `ToggleShuffle`, `SpeedUp`,
        `View(Eq)`, `ToggleCrossfade` and pause against mpv directly,
        and its shuffle arm's own comment admits it copies the `s`
        key. `Field::Progress` is the only one with no op, which core
        refuses by name.
        `core::op_for_field` is now the one table, read by both, and
        `activate_transport` is a four-line `if let` plus the scrub
        drag. **net -54 lines**, and two things fell out of it:
        `TransportCtx` dropped from 19 fields to 5, since the rest
        only existed to serve the duplicated arms, and
        `resync_upcoming` became an unused import in main.rs -- the
        duplicated shuffle arm was its only caller there, which is
        about as direct a confirmation as a refactor gets.
        every bar control now lands in the undo tree, because
        `apply_op` is where recording happens. pinned two ways: every
        `Field` but `Progress` maps to an op (the scrubber is a drag,
        not a one-shot), and each of those ops classifies as
        restorable, except `View(Eq)` which opens a menu and correctly
        is not an edit.
        **the behaviour difference, settled rather than preserved.**
        clicking the eq glyph reset `EqMenuState`; `view(eq)` did not,
        so a half-typed name editor or an armed delete could survive a
        close and reopen. routing the click through the op made that
        visible, and the reset moved into the `ViewTarget::Eq` arm so
        both paths open fresh. that is a small bug fix the
        deduplication found rather than caused.
        **undo coverage is now complete**: ops via `apply_op`, keyboard
        actions via `dispatch_action`, bar controls via `op_for_field`.
        what is left unrecorded is deliberate -- seek and volume keys
        (volume is in the snapshot but its keys mutate directly) and
        `paused`, which the snapshot excludes on purpose.
      - [x] ~~**the view is built but not opened by anything.**~~
        **`view(undo)` opens it** (alias `view(tree)`), drawn over the
        workspace like the eq menu, because a fork is unreadable in a
        sidebar-sized column. `u`/`r` step, `h`/`l` cross a fork,
        `esc` closes; `k`/`j` and the arrows are undocumented
        synonyms. undo and redo go through `apply_op` rather than the
        recorder directly, so there stays exactly one path that
        records, refuses at a barrier and reports.
        opening an empty tree **fails with "nothing has been done
        yet"** rather than showing a blank frame that reads as broken.
        `undo_menu_open` joins main.rs's `overlay` flag, which is not
        only the animation gate: it also feeds `video::wanted`, and
        without it mpv would have kept drawing a video straight over
        the modal.
        **two things the first draft got wrong, both caught by the ux
        self-check rather than by a test.** it drew a bare frame
        instead of `draw::overlay`, so the workspace showed through
        the gaps between branch lines; `overlay` scrims every cell, and
        `assert_no_bleed_through` now pins it. and it had **no key
        hint at all**, which for a modal whose verbs are `u`/`r`/`h`/
        `l` is the definition of undiscoverable. the hint and
        main.rs's handler read the same `UNDO_KEY`/`REDO_KEY`/
        `LEFT_KEY`/`RIGHT_KEY` constants, which is the lesson
        `eq_menu` already recorded: a hint that spells its own letter
        can otherwise advertise a key nothing acts on.
        still wants a human at a real terminal for the thing a buffer
        cannot show: whether a fork is legible at a glance when the
        tree is twenty nodes deep rather than five.
      - ~~**the tree view**~~ **built: `src/ui/undo_tree.rs`.**
        box-drawing spines (not ascii pipes), the current row plated
        with `anim::selection_bg` like every other selection in the
        app so it breathes when focused and sits flat when not, a
        right-aligned age column reserved up front so labels cannot
        shove it around, and barrier rows marked `!` with the current
        one's reason spelled out in a footer, because "why can I not
        go further" is the question the view exists to answer and a
        toast would have scrolled away by the time anyone opened it.
        brightness carries the hierarchy as specified: current
        `NEAR_WHITE`, ancestors `MID`, other branches `DIM`, and the
        branch lines themselves stay dim even on the current row,
        since they are structure rather than content.
        `Tree::rows` grew `on_path` and `last_child` to feed it; the
        first is the brightness tier, the second decides tee vs
        corner.
        **printing it found a bug ten passing tests did not.** the
        spine column indexed ancestors from depth 0 rather than depth
        1, so a child of a forked node drew a blank gap where its
        parent's line should continue, reading as though that branch
        had ended. the box-drawing assertion missed it completely
        because every glyph drawn was still box-drawing.
        `undo_tree::tests::print_the_tree` prints it at two widths;
        `a_forked_parents_line_continues_through_its_children` pins
        the fix.
        what it still needs is a human at a real terminal for the one
        thing a buffer cannot show: whether the plate pulse reads as
        alive rather than restless next to a static tree.

- [ ] **decompose main.rs.** the plan's order holds: ~~focus/
      InputMode~~, then the playback free functions, then the
      sync/channel plumbing, then a `struct Ui` for the ~35 loose
      locals, then the draw closure, and only then the input tiers
      (their ordering is load-bearing and heavily commented).
      **the "~3.9k lines" in this entry was stale: it was 6.2k when
      the decomposition started.** down to 4.9k over six slices.
      worth re-measuring rather than trusting, since every feature
      slice adds to it.
      - [x] **the eq menu's three locals -> `ui::eq_menu::EqMenuPane`.**
        `eq_menu_open`, `eq_menu_selected` and the `EqMenuState`
        editor, behind `open`/`close`/`selected`/`state_mut`. named
        `EqMenuPane` because `EqMenu` is already the widget.
        **the rule it makes structural was broken once for real**:
        opening must reset the editor *and* place the cursor on the
        curve in use, and the two openers disagreed -- the eq-glyph
        click reset `EqMenuState`, `view(eq)` did not, so a half-typed
        name or an armed delete survived a close and reopen. that was
        fixed by hand at the second site; `open` means a third opener
        cannot get it wrong.
        five tests, and the fresh-start one **opens over an
        already-open menu with no close in between**: `close` resets
        too, so a close-then-open sequence passes even with the reset
        deleted from `open`. the first draft did exactly that and the
        mutation walked through it. reopening while open is also a
        real path, since `view(eq)` does not care what is on screen.
        main.rs 4819 -> 4814. groups left: art/picker (4) and the
        playback timers, which stay loose on purpose (four unrelated
        features, one bag would name nothing).
      - [x] **both art-protocol caches -> `graphics::ProtocolCache`.**
        `player_art_protocol`, `real_art_protocol` and
        `browse_art_key` become two instances of one type.
        another duplication, not a grouping: the full player and the
        browse view follow the *same* rule -- rebuild only when the art
        or the tone changes, `ratatui-image` owning the per-instance
        resize and encode caching -- and each had its own copy. the
        code even said so ("the browse view's own protocol, same
        rule").
        **the browse copy was desync-by-construction**: it kept the key
        in a separate local from the protocol, updated at different
        sites, with nothing making them agree. a third site
        (`browse_loader.poll` handing over art) set the protocol with
        *no key at all*. `ensure(picker, key, || image)` makes the pair
        inseparable, and that third site now clears instead, letting
        the keyed path rebuild.
        the image is a closure because building it is the expensive
        half (`full-bw` greyscales a copy) and a cache hit must not pay
        it -- pinned by a test that asserts the closure never runs on a
        hit. two more: no picker holds nothing (so the first frame
        after a probe lands does not think it is current), and clearing
        forgets the key too (a stale key would suppress the rebuild).
      - [x] **the gap mechanism's three locals folded into
        `BetweenTracks`.** `gap_secs` and `keep_open_held` joined the
        hold timer, and `BetweenTracks::step` replaced the per-frame
        block.
        **this one removed a real duplication rather than just
        grouping**: the `keep-open` toggle was written out twice, once
        in main.rs and once in shellswim's `pump`, because when the
        gap shipped each front-end needed its own copy of the rule.
        both call `step` now, and `CoreCtx` lost its `gap_secs` field
        -- the live gap belongs to the thing that waits it out, so
        `Op::SetGap` goes through `set_gap`, which also drops a hold
        in progress, because "off" must mean off rather than "off
        after this one". a test pins that.
        two more tests: the length round-trips, and a negative gap
        clamps to zero rather than meaning a reverse one.
        main.rs 4836 -> 4820, shellswim's host smaller too.
      - [x] **the enrichment warm-up's four flags -> `enrich::Warmup`.**
        `enrich_probe`, `enrich_job_in_flight`, `suggestions_stale`,
        `suggestions_open_last_frame`.
        the win here is not the line count (4836) but that **the one
        real rule became a method**: at most one job in flight, and
        only while the menu is open. that was previously spelled out
        across three separate statements in the loop, and the ordering
        between them was load-bearing -- the open-edge has to be seen
        *before* the probe is consumed, or opening the menu fires
        nothing until some unrelated poke. `should_fetch` holds that
        ordering in one place.
        five tests, two of which pin things the loose version could
        get wrong silently: a second job waiting for the first, and a
        *failed* send not counting as in-flight (otherwise one dropped
        channel stops the warm-up forever).
      - [x] **the lyrics pane's four locals -> `LyricsPane`**
        (`src/lyricspane.rs`). the first `struct Ui` slice, and done
        as a **small cohesive group rather than one bag**: a single
        struct holding all fifty-three loose locals relocates the
        problem without naming anything, so the plan's "struct Ui" is
        being read as "group what moves together".
        these four do move together -- open the pane and all four are
        set, close it and all four stop mattering -- so `is_open()` is
        now a question about the group instead of a convention about
        one field being `Some`, and `open()` resets the scroll rather
        than every caller remembering to. four tests, one of which
        pins exactly that reset: keeping the old offset lands you
        mid-song in a different song.
        **53 loose locals, not ~35** as the entry claimed. the natural
        groups left: scrub (3, already has `TransportCtx`), eq menu
        (3), art/picker (4), enrichment probe (4), playback timers (7).
      - [x] **the eq menu's keys and persistence ->
        `src/eq_menu_keys.rs`** (136 lines). `adjust_eq_band`,
        `eq_label`, `persist_eq`, `eq_saved_row_hint`,
        `eq_menu_editor_key`, `eq_commit_name`. the seam is the menu:
        each one answers a key it owns, names the curve on screen, or
        writes the live curve back to config.toml. the curve maths is
        `eq.rs`; this is the menu around it. main.rs 4985 -> 4855.
        `save_config` moved to `config::save_or_warn` rather than into
        this module: both main.rs and the eq menu call it, and its
        home is the thing it saves. that gives `config` a dependency
        on `notify`, which is fine (notify is a leaf) and is the
        honest shape -- "persist, and say so if you cannot" is one
        operation.
      - [x] **terminal graphics capability -> `src/graphics.rs`** (304
        lines). `wants_full_art`, `has_real_protocol`, `real_graphics`,
        `art_mode_for_picker`, `warn_no_graphics`, `protocol_image`,
        `cell_aspect`, `graphics_plausible`/`_in`, and nine tests.
        one seam, stated as a question: every decision in there is
        "ask the terminal, or decide from what it answered". **main.rs
        5267 -> 4985**, under five thousand for the first time, from
        6.2k.
        **the mixed-test-module pattern repeated, which makes it a
        pattern rather than an accident.** last slice it was
        `catalog_fetch_tests` holding the graphics-probe tests; this
        slice it was `startup_tests` holding *seven* art-mode tests
        alongside the two `loading_message` ones. both times the
        module was named for the first thing someone tested in it and
        then accreted whatever came next. worth expecting in the
        remaining slices rather than discovering again: check which
        tests a module actually holds before assuming its name
        describes it.
      - [x] **catalog fetching and the sync thread -> `src/catalog.rs`**
        (355 lines). `AUTO_SYNC_INTERVAL`, `BackgroundSyncResult`,
        `spawn_background_sync`, `FetchedCatalog`, `fetched`,
        `fetch_navidrome_catalog`, `merge_catalog_into_app`,
        `fetch_navidrome_everything`, `index_navidrome_server`,
        `test_navidrome_connectivity`, plus `catalog_fetch_tests`.
        that finishes the channel plumbing step; `recompute_suggestions`
        went to `enrich.rs` instead, since rebuilding the menu belongs
        with the thing that enriches it rather than with fetching.
        **main.rs 5622 -> 5267**, and 6.2k -> 5.3k over four slices.
        **this one was messier than the three before it and the reason
        is worth recording.** `catalog_fetch_tests` was not a clean
        unit: it also held the two `graphics_plausible_in` tests,
        which belong to the terminal probe and had to be split back
        out into their own `graphics_probe_tests`. a test module named
        after one subsystem containing another's tests is the kind of
        thing only a move surfaces.
        `cargo fix` made it worse rather than better: run with
        `--bins --lib` it cannot see test code, so it stripped imports
        (`CacheIndex`, `subsonic`) that only the tests used, and the
        build then failed in the tests instead. two of those are
        qualified at the use site now, because an import needed only
        under `cfg(test)` is otherwise an unused-import error in the
        binary. **do not reach for `cargo fix` on a file with inline
        test modules**; fix the imports by hand.
      - [x] **background enrichment -> `src/enrich.rs`** (461 lines
        with its tests). the most self-contained part of the channel
        plumbing the plan calls for: one job goes out over `job_rx`,
        its answers come back over `event_tx`, and the main loop's
        only involvement is draining them.
        **main.rs 6062 -> 5622**, the biggest cut of the three so far,
        and the whole `enrichment_plumbing_tests` module went with it
        -- the same tell as `focus`: when the tests move wholesale,
        the cut was at a seam.
        the four `ENRICH_*` constants moved too. they are the real
        bound on the work (twelve album round trips per job, four in
        flight, one job at a time) and their reasoning is long, so
        leaving them behind in main.rs would have split a decision
        from the code that implements it.
      - [x] **the transport controls -> `src/transport.rs`** (160
        lines). `ScrubAt`, `TransportCtx`, `activate_transport`,
        `commit_scrub`, `audio_level` and `now_playing_is_video`. the
        seam is `controlsbar::Field`: everything in there either
        presses one of those or reads playback to draw one, and none
        of it knows about panes, menus or the input tiers.
        it is a smaller win in lines than it looks (main.rs 6187 ->
        6062) because `activate_transport` had already shrunk to four
        lines plus the drag when the bar's controls became commands.
        the value is the seam, not the count.
        two tests came with it, one new: `audio_level`'s fallback has
        to *move* rather than read a constant, since the point of it
        is a meter that breathes when mpv reports nothing instead of
        sitting dead.
        **the doc-comment hazard noted below recurred, and this one
        was pre-existing**: `TransportCtx` carried two glued doc
        blocks, the older of which ("everything activating a transport
        control can touch") stopped being true when the struct shrank
        to five fields earlier the same day. the move surfaced it
        rather than causing it; the stale half is gone.
      - [x] **focus/InputMode -> `src/focus.rs`** (155 lines with its
        tests). the cheapest possible first slice and chosen for that:
        `InputMode`, `focus_direction_key` and `focus_in_direction`
        are pure, self-contained, and already had six tests, so
        nothing load-bearing moved and the diff is almost entirely
        relocation. main.rs lost its whole `focus_tests` module, which
        is the tell that the cut was at a real seam rather than
        through the middle of something.
        one thing to watch for in the next slices: the doc comment
        above `focus_in_direction` travelled separately from the
        function and landed above `InputMode`, because a
        walk-backwards over `///` lines does not stop at a blank line
        between two doc blocks. check the docs line up with what they
        document after each move.
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