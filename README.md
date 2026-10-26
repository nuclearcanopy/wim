# wim

terminal omniplayer for music and video.

- navidrome/subsonic + local files in one unified, fuzzy-matched search
  (artists, albums, songs, playlists, local tracks, local video)
- playback via mpv (json ipc): loop, loop-one, shuffle, speed, 10-band eq,
  replaygain, transcoding
- auto lyrics lookup, auto opensubtitles lookup for local video
- full subtitle control: delay, language, size, color, outline, background
  opacity/blur/radius, position
- album art: real images (sixel/kitty/iterm2, auto-detected) or a
  pixelated unicode-block mode for a pure-TUI look
- strictly monochrome ui (grayscale only), vim-like navigation
- config is a plain toml file, or generate it declaratively from a nix
  flake via the home-manager module (see `nix/`)

## status

early development, not usable yet.

## build

```
nix develop
cargo build
```

## config

on first run, writes a template to `$XDG_CONFIG_HOME/wim/config.toml` (or
the platform equivalent). fill in your navidrome url/credentials and
library paths, or manage the same file declaratively via the
home-manager module instead. `library_paths` defaults to `~/Music` and
`~/Videos` (whichever exist) when left empty.

## keybindings

vim-like, settled as the app grows; this list is the source of truth.

- `/` — start/edit the live search query
- `j` / `k` (or down/up) — move selection
- `gg` / `G` — jump to first/last result
- `enter` — play the selected result immediately
- `a` — add the selected result to the queue (doesn't interrupt playback)
- `l` — toggle loop on the current track
- `s` — toggle shuffle (reshuffles the current queue immediately)
- `[` / `]` — speed down/up (0.25x–3x, 0.1x steps)
- `q` / `esc` — quit

the queue auto-advances when the current track ends; nothing plays next
if the queue is empty.
