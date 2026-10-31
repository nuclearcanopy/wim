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
- [ ] **BLOCKER (for the pull-frames-ourselves route only): there is no
      working way to get frames out of mpv.**
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

- [ ] **two latent bugs that must be fixed as part of the timeline**,
      both found during that survey and both currently unreachable only
      by luck:
      - `load_playlist` (~main.rs:691) and `resync_upcoming` (~743)
        skip an item whose `resolve_playable` returns `None` when
        appending to mpv **but still store the full vec in
        `PlayQueue`**. Callers pre-run `resolve_tracks` today, so it
        cannot fire; a new entry kind that fails to resolve hits it and
        puts every later index off by one.
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

- [ ] **`resolve_tracks` (main.rs ~118) is the worst of the three.**
      called from `dispatch_action`'s `Action::Play` and `Action::Queued`
      arms (~1063, ~1099) and from shellsong (~1920). for an album it
      is one `getAlbum`. **for an artist it calls `artist_tracks`,
      which is a `getArtist` plus one `getAlbum` per album**, so
      pressing enqueue on a prolific artist blocks the whole ui for as
      long as that fan-out takes. the bounded pool made it 5x faster;
      it is still seconds, and still synchronous.
      note the irony: `browse::Loader` already streams exactly this
      data album by album for the *browse* path. queueing it re-fetches
      the same thing synchronously. the fix may be to route queueing
      through the loader rather than to build a fourth one.
- [ ] **`open_starred_view` (main.rs ~312) fetches starred songs from
      every configured server synchronously**, from shellsong (~1543)
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

- [ ] **pressing the lyrics key freezes the app on the network.**
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
      **what is left is two call sites**: replace each `lookup` with
      `request`, drain `poll` next to the other channel drains in the
      main loop, and `invalidate` when the lyrics view closes or the
      track changes. blocked only because `main.rs` is owned by the
      library split tonight.
      **while in there, audit the input path for the same pattern.**
      the two found so far (album/artist open, lyrics) were both a
      client call straight from a key handler, so grepping for network
      calls reachable from `dispatch_action` and `run_shellsong_op` is
      the cheap way to find the third.

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
- [ ] **one commit on that branch should have been two** (`ede72c0`):
      the shellswim files were staged when the merge was resolved, so
      they went in with it. The message says so. Nothing to fix, worth
      not repeating.

## shellswim and the undo tree (maintainer, 2026-10-31)

Two features the maintainer asked for that share one prerequisite, so
the order below is not a preference, it is a dependency.

- [ ] **`shellswim`: a second binary that is just the shell.** the
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
- [ ] **undo and redo as a tree, not a stack.** the maintainer's words:
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