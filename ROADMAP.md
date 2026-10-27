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

**rules for anyone (human or loop) working from this file:**
- small progressive commits, one coherent change each, lowercase subject,
  no ai co-author trailer (this is the maintainer's own repo)
- every commit must leave `nix develop --command cargo build` passing
  (and `cargo clippy` clean where reasonable) before you commit
- strictly monochrome ui: every color is `Color::Rgb(v,v,v)` via
  `src/theme.rs` helpers, never a hue, anywhere, including album art in
  `blocks` mode (real sixel/kitty/iterm2 art can stay true color)
- no stock ratatui `Block`/`Paragraph`/`List` widgets — custom-painted
  widgets only (see `src/ui/banner.rs` for the pattern)
- can't test raw-mode TUI rendering headlessly (no real tty in CI/loop
  context) — rely on `cargo build`/`cargo check`/unit tests for logic,
  note in the commit body what still needs a human to eyeball
- genuinely blocked on a decision only the maintainer can make? don't
  stop — write it to QUESTIONS.md and move to the next unblocked item

## done

- [x] scaffold: flake.nix devshell, AGPL-3.0, README
- [x] cargo init, full dependency set pinned
- [x] config: toml file at `$XDG_CONFIG_HOME/wim/config.toml`, templated
      on first run, `password_file` support for agenix-style secrets
- [x] mpv: json ipc control layer (spawn, socket, request/reply +
      event demuxing), play/pause/seek/volume/speed/loop-file wired
      but unused
- [x] ui: custom-painted `Banner` widget proving the no-stock-widgets,
      monochrome-gradient, animated approach

## next (roughly in order, pick whichever is actually unblocked)

- [x] subsonic client (`src/subsonic.rs`): token auth, ping, search3,
      playlists, stream url. token math unit-tested against the
      official api docs example.
- [x] local library scanner (`src/library.rs`): walks `config.library_paths`
      (defaulting to `~/Music`/`~/Videos` via `default_library_paths()`
      when unset — most people shouldn't have to configure paths just
      to get started), classifies by extension, reads audio tags via
      `lofty`.
- [x] unified search (`src/search.rs`): one corpus over local tracks +
      local video + navidrome artists/albums/songs/playlists, merged
      and ranked together. includes typo tolerance (nucleo subsequence
      fuzzy + damerau-levenshtein edit-distance fallback) — see
      `docs/features.md` for why that's not optional.
- [x] app key-handling state machine (`src/app.rs`): vim-like nav
      (j/k, gg/G, `/` to edit the query live, enter, q/esc), fully
      unit-tested, decoupled from rendering.
- [x] app event loop in `main.rs`: real render loop driving `App` +
      `mpv::Mpv` + a custom results list + status bar. local
      music/video items load into mpv on enter; remote items report
      "not playable yet" since live navidrome search isn't merged into
      the loop yet (needs a real server to test against).
- [x] play queue: `a` enqueues, auto-advances on mpv end-file. reorder/
      view-the-queue ui still pending (queue currently has no visible
      list of its own, just a count in the status bar).
- [x] playback controls: loop-file (`l`), speed (`[`/`]`), shuffle (`s`,
      reshuffles the queue in place + randomizes future inserts).
      "avoid immediate repeats" smart-shuffle is more meaningful once
      there's a "shuffle whole library into queue" feature — not
      applicable to today's small explicit queue the same way.
- [x] gapless playback: queue (`a`) now appends to mpv's own native
      playlist instead of us reactively reloading on `end-file`, so mpv
      auto-advances internally with no ipc round-trip gap.
- [ ] optional crossfade (toggle + duration, off by default) — separate
      from gapless; mpv doesn't do this natively the way it handles
      playlist auto-advance, needs its own mechanism (likely dual
      mpv instances or a scripted volume ramp). see `docs/features.md`.
- [x] 10-band eq (`src/eq.rs`) with presets (flat/bass boost/vocal
      boost/treble boost/rock/classical), `e` cycles. applied via
      chained lavfi `equalizer` filters on mpv's `af` property. preset
      gain values are heuristic starting points, not acoustically
      tuned — a "custom" user-editable preset is still open.
- [ ] now-playing audio level visualizer: animated unicode-block meter
      (think `cava`) fitting the monochrome custom-painted aesthetic
      already established. genuinely loved in the terminal-tool space
      this app lives in, not a stretch feature.
- [ ] playback state persistence: resume queue/position/volume across
      restarts.
- [x] replaygain: mpv reads embedded tags itself once `replaygain=track`
      is set — enabled by default at startup, no extraction needed on
      our end for local files. subsonic's replayGain fields for remote
      tracks deferred along with the rest of live navidrome wiring.
- [ ] transcoding: let navidrome/subsonic transcode server-side per its
      own configured profile by default (stream request params); only
      add client-side bitrate/format override if asked.
- [x] lyrics (`src/lyrics.rs`): lrclib.net (free, no api key), `L` to
      view/close, j/k to scroll. known gap: blocking fetch on the
      render thread (visible hitch during lookup), and SearchItem
      doesn't carry duration so local lookups always use the fuzzy
      /search endpoint rather than the exact /get one — both noted
      inline, not blocking, fine to revisit.
- [ ] album art (`ratatui-image`): wire the `album_art` config mode
      (auto/blocks/sixel/kitty/iterm2/off). `blocks` must desaturate to
      grayscale to match the monochrome ui; other modes show true color
      since that's real image data, not ui chrome.
- [ ] video playback + opensubtitles: auto-fetch subs for local video.
      needs an opensubtitles api key from the maintainer — build the
      integration behind a config field, note in QUESTIONS.md that it
      needs a real key to live-test, keep building everything else.
- [ ] subtitle styling: delay, language, size, outline, background
      opacity/blur/radius, position — all via mpv `sub-*` properties;
      color stays grayscale per the monochrome rule.
- [x] nix packaging: `packages.<system>.wim` (and `.default`) via
      `rustPlatform.buildRustPackage`. mpv is a runtime subprocess dep
      (not linked), so the binary is wrapped with `makeWrapper` to put
      it on `PATH` rather than requiring it installed system-wide.
      verified with `nix build` + `nix flake check` and manually ran
      the wrapped binary with an emptied inherited `PATH` to confirm
      mpv really comes from the wrapper, not the ambient environment.
- [ ] home-manager module (`nix/home-manager.nix` + flake
      `homeManagerModules.default`): declarative options mirroring
      config.toml 1:1 (navidrome url/username/passwordFile, album_art,
      library_paths, etc.), generates the same toml file home-manager
      already understands how to write.
