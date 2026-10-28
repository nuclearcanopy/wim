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
      tuned; a "custom" user-editable preset is still open.
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

## in flight (branches, NOT merged; do not start work that touches these)

five branches exist with uncommitted-to-master work. check
`git worktree list` and `git branch` before picking anything up; a loop
run that edits these files will collide.

- `feat/shellsong` + `wim-wt-shellsong`: "shellsong", a music command
  language on `:` (`src/shellsong.rs`, `src/ui/shellsong.rs`,
  `docs/shellsong.md`). call-only grammar, `play(crest)`,
  `queue([hex, 4 star crest])`, `queue_aa(...)`, `favorite(...)`,
  `add_to_queue(front, ...)`, `moveto(10%)`, `seek(+30s)`. deliberately
  scoped to pure modules with zero main.rs edits; **host wiring is
  still to do** and is the one remaining step.
- `feat/pulse-animations` + `wim-wt-pulse`: selection shown by a
  pulsing row background instead of border colour, plus one `src/anim.rs`
  driving every animation off wall-clock time scaled to the display's
  refresh rate (the loop currently polls a flat 100ms, so everything
  animates at ~10fps).
- `fix/nowplaying-queue` + `wim-wt-queue`: "now playing" shows the
  wrong track because the app mirrors mpv's playlist position by
  counting end-file events instead of observing `playlist-pos`; any
  missed event desyncs it permanently. also moves the download trigger
  from "played twice" to "played on two distinct days".
- `feat/controls-bar-icons` + `wim-wt-controls`: bottom controls bar to
  glyphs instead of `loop: off` text, arrow-key selection within the
  bar, enter to interact, and enter on the progress bar entering a
  scrub mode.

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
- [ ] **suggestions: the three background fetches.** only the
      history-driven generators can fire today (daily rotation, album
      gap, genre under-exploration, rediscovery, starred-unplayed, and
      the cold-start paths). `TrackGap` and `AlbumNeighbour` need
      `getAlbum` for real coverage and track order, `SimilarArtist`
      needs `getArtistInfo2`, `PopularUnplayed` needs `getTopSongs`.
      `suggest::enrichment_targets` already says which artists to fetch
      top-affinity first, and `EnrichmentCache::needs_*` remembers what
      has been asked including negative answers; the view takes an
      `enriching` flag so a thin list says "so far" rather than looking
      final. `Client::stream_artist_tracks` already fetches exactly the
      `getAlbum` data for the artist view and discards it.
- [ ] **drop `suggest.rs`'s module-level `allow(dead_code)`.** the menu
      now calls `suggest`, `Catalog::from_corpus` and `Request::new`,
      but the tuning knobs, `enrichment_targets` and `EnrichmentCache`
      stay unread until the fetches above land.
- [ ] **wire shellsong to the host.** the language is complete and
      tested; nothing runs it yet. needs the console opened on `:`,
      keys and paste routed to `Console`, `ShellsongConsole` rendered,
      a `Candidates` impl over the local corpus (never firing a remote
      search), `Env::focus`/`Env::search_settled` supplied, and the
      `Op` -> host table. unimplemented ops: `OpenPage`, all twelve
      `View` targets, `SetVolume`, `Mute`, `Sleep`, `Fade`, `Rescan`,
      `SaveQueue`/`LoadQueue`, `RemoveFromQueue`, `GotoQueue`,
      `ShuffleQueue`. the queue ops can reuse `PlayQueue::remove`/
      `move_entry` and the mpv playlist commands already added.
- [ ] **album/artist identity is name-based.** `SearchItem` carries no
      `album_id`/`artist_id`, so the suggestion catalog joins on
      trimmed-lowercased names. two same-named albums by one artist
      collapse, and a split-credit artist ("Bladee & Ecco2k") rolls up
      under neither. widening `SearchItem` touches every construction
      site, so it was left alone deliberately; revisit if the menu's
      suggestions look wrong in practice.

## open sub-tasks discovered during the review pass

- [x] **queue view** (`src/ui/queue.rs`, `Q`): three labelled runs in
      play order (now playing / up next / played, the last one
      newest-first), the current track marked structurally with `▌`
      rather than by brightness alone so selection and now-playing stay
      readable on the same row, played entries sunk to NEAR_BLACK, an
      index gutter sized to the widest number actually on screen, and a
      real empty state instead of a blank pane. enter writes
      `playlist-pos` to jump (no reload, so `entries` stays aligned with
      mpv), esc closes. reads `PlayQueue::entries`/`pos`, so it shows
      mpv's actual playlist rather than a mirror.
- [x] **queue view: remove and reorder.** `d` drops an entry, `J`/`K`
      move one. `PlayQueue::remove`/`move_entry` carry `pos` with the
      *track* rather than the index, so the entry list stays aligned
      with mpv's playlist through every edit; the observed
      `playlist-pos` still corrects anything that drifts. reorder is
      deliberately upcoming-only: moving a played entry does nothing
      useful, and in the played run the display is reversed so `J`
      would visibly move a row the wrong way. found and fixed a latent
      bug while here: `Mpv::playlist_move` passed the destination
      straight through as mpv's displaced-entry index, which is only
      correct when moving upwards (its doc admitted as much); it now
      converts, so downward moves land where asked. the old single
      caller only moved upwards, so it is unaffected.
- [ ] **queue view: the ops shellsong will need.** `remove(n)`/
      `goto(n)`/`shuffle_queue()` from the shellsong branch can now
      reuse `PlayQueue::remove`/`move_entry` plus the mpv commands
      added here, but still need the `Op` -> host wiring.
- [x] **wide characters overlapping their neighbour.** `src/ui/draw.rs`
      is now the one place text reaches a `Buffer`: it advances by
      `char_cells` and blanks the trailing cell of a wide glyph, so a
      CJK or emoji title no longer shifts everything after it one cell
      left and overruns the row. every widget that renders
      *user-supplied* text delegates to it (results_list, browse,
      queue, player, miniplayer, notify, header, lyrics_view,
      searchbox), which also deleted eight near-identical `draw_line`
      copies and fixed two latent bugs found on the way: `help.rs`'s
      copy was missing the bottom-bounds guard the other seven had, and
      `player.rs` centred by char count so a wide-char title drifted
      right by its own overhang. the search box's cursor position now
      comes from the writer's returned x rather than a char count, so
      typing wide characters no longer parks the block inside the last
      glyph.
- [ ] **migrate the remaining chrome widgets to `ui::draw`**: eq_menu,
      keybinds_menu, settings_menu, subtitle_menu, helpbox, help,
      nav_manager, login, onboarding, banner's menu row. all draw
      static mostly-ascii labels, so they are correctness-neutral
      today; worth doing for the dedup and so a translated or
      user-supplied string cannot reintroduce the bug. `banner.rs`'s
      logo grid is deliberately excluded (figlet art, one cell per
      char by construction).
- [ ] **`help.rs` still mislabels four remappable actions as fixed**:
      `Key::Fixed("[ / ]")` and `Key::Fixed(", / .")` cover SpeedUp/
      SpeedDown and SubtitleDelayUp/Down, which are rebindable, so
      those two rows go stale after a remap.
- [ ] **`HELP_BOX_WIDTH` is 10**, so a help key rebound to `space`
      renders clipped ("space hel"). widen it or shorten the label.
- [ ] **mpv survives an unclean wim exit.** `PR_SET_PDEATHSIG` would
      close it with the parent; `Mpv::spawn` also passes no
      `--no-config`, so a user mpv.conf silently alters playback.
- [ ] **custom user-editable eq preset** (noted long ago, still open).
- [ ] **decompose main.rs** (~3.9k lines). a reviewed, ordered plan
      exists: extract focus/InputMode, then the playback free functions,
      then the sync/channel plumbing, then a `struct Ui` for the ~35
      loose locals, then the draw closure, and only then the input
      tiers (their ordering is load-bearing and heavily commented).
- [ ] **~10 blocking mpv ipc round-trips per frame**: `playback_info` is
      called 2-3x per loop iteration plus `audio_level` and
      `cell_aspect`'s ioctl; `observe_property` exists and is now used
      for playlist-pos, so the rest should follow and the per-frame
      polling should go.
