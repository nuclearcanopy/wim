# wim roadmap

terminal omniplayer. full spec lives in README.md; this file tracks what's
done and what's next, for both the maintainer and the overnight build loop.

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

- [ ] subsonic client (`src/subsonic.rs`): token auth (`t = md5(password +
      salt)`, random `s` salt per request), `ping`, `search3` (artists/
      albums/songs), playlist listing, `stream` url construction with
      transcoding params. hand-rolled over `ureq`, no existing subsonic
      crate (they're unmaintained/incomplete). unit-test the token math
      and url construction without needing a live server.
- [ ] local library scanner (`src/library.rs`): walk `config.library_paths`
      via `walkdir`, classify by extension into music/video, extract
      basic tags (consider the `lofty` crate for audio metadata) for
      better search matches than bare filenames.
- [ ] unified search (`src/search.rs`): one `nucleo-matcher` index over
      local tracks + local videos + navidrome artists/albums/songs/
      playlists, merged and ranked together, not per-source silos. this
      is the most-emphasized requirement — make it actually good, not a
      token substring filter.
- [ ] app event loop: replace the smoke-test `main.rs` with a real
      `App` struct (state machine: library/search view, now-playing
      view, etc.) driving mpv + search + ui together. vim-like nav
      (hjkl, gg/G, `/` search, etc. — document exact bindings in
      README as you settle them).
- [ ] playback controls: loop / loop-one / shuffle / speed, wired to the
      existing mpv methods, exposed as keybinds + visible state in ui.
- [ ] 10-band eq via mpv `superequalizer` (or `lavfi-complex` equalizer
      chain) af filter, ISO center frequencies (31/62/125/250/500/1k/2k/
      4k/8k/16k Hz) unless told otherwise.
- [ ] replaygain: read replaygain tags (local files) / use subsonic
      `replayGain` fields where the server exposes them, apply via mpv
      `replaygain` property.
- [ ] transcoding: let navidrome/subsonic transcode server-side per its
      own configured profile by default (stream request params); only
      add client-side bitrate/format override if asked.
- [ ] lyrics: auto-fetch on now-playing. default to lrclib.net (free, no
      api key) unless told otherwise — log the choice in a commit so
      it's easy to swap later.
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
- [ ] nix packaging: `packages.<system>.wim` derivation
      (`rustPlatform.buildRustPackage`) in flake.nix.
- [ ] home-manager module (`nix/home-manager.nix` + flake
      `homeManagerModules.default`): declarative options mirroring
      config.toml 1:1 (navidrome url/username/passwordFile, album_art,
      library_paths, etc.), generates the same toml file home-manager
      already understands how to write.
