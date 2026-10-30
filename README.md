# wim

terminal omniplayer for music and video.

- navidrome/subsonic + local files in one unified, fuzzy-matched search
  (artists, albums, songs, playlists, local tracks, local video)
- playback via mpv (json ipc): loop, loop-one, shuffle, speed, 10-band eq,
  replaygain, transcoding
- auto lyrics lookup, auto opensubtitles lookup for local video
- full subtitle control: delay, language, size, color, outline, background
  opacity/blur/radius, position
- album art in four modes: pixelated unicode blocks or real inline images
  (sixel/kitty/iterm2, auto-detected), each in greyscale or colour
- strictly monochrome ui (grayscale only, album art aside), vim-like
  navigation
- config is a plain toml file, or generate it declaratively from a nix
  flake via the home-manager module (see `nix/`)

## status

early development, not usable yet.

## build

```
nix develop
cargo build
```

or build the packaged binary directly: `nix build .#wim` (mpv is
wrapped onto its `PATH` automatically, no system-wide install needed).

## nix

packaged as `packages.<system>.wim` (and `.default`). a home-manager
module is exposed as `homeManagerModules.default`:

```nix
{
  imports = [ wim.homeManagerModules.default ];
  programs.wim = {
    enable = true;
    # a list; every configured server feeds the same unified search,
    # no "active server" to switch between. the in-app 'n' profile
    # manager can add more on top of whatever's declared here.
    navidromeServers = [
      {
        url = "https://navidrome.example.com";
        username = "me";
        passwordFile = config.age.secrets.navidrome-password.path; # or any path
      }
    ];
    albumArt = "auto"; # auto | ascii-bw | ascii-color | full-bw | full-color | off
    libraryPaths = [ ]; # defaults to ~/Music, ~/Videos when empty
  };
}
```

there's deliberately no plaintext `navidrome.password` option; nix
store paths are world-readable, so `passwordFile` (composes with
agenix/sops-nix) is the only way to set credentials declaratively.

## config

on first run, writes a template to `$XDG_CONFIG_HOME/wim/config.toml` (or
the platform equivalent). fill in your navidrome url/credentials and
library paths, or manage the same file declaratively via the
home-manager module instead. `library_paths` defaults to `~/Music` and
`~/Videos` (whichever exist) when left empty.

`mouse = true` (the default) captures the mouse: click a pane to focus
it, click a row to select it, click a control on the bottom bar, drag
the scrubber to seek, wheel over any list to scroll it. capture takes
the terminal's own click-drag text selection away from the terminal;
hold `shift` while dragging to get it back (works in most terminals),
or set `mouse = false` to give it back for good.

`video_output = "auto"` (the default) picks how `v` draws a local
video. mpv draws it itself, filling the terminal, while wim stops
drawing: `tct` is true-color half-blocks and works in any terminal,
`kitty` is the kitty graphics protocol and needs a terminal that speaks
it. `auto` uses kitty only when the terminal actually answered the
graphics probe with it, `tct` otherwise. `off` disables the key. music
playback is never affected.

## keybindings

press `?` in the app for the full list. it resolves every key from the
live keymap, so it stays correct after rebinding with `K`; a static
list here would go stale the moment anything moved. what follows is
only the fixed conventions, which are not rebindable.

two modes, named at the left of the bottom bar:

- `tab`; enter move mode
- move mode: `wasd` or arrows focus a pane (sidebar / workspace /
  bottom bar), `enter` or `esc` returns to interact mode. nothing else
  responds while it's active, by design.
- interact mode: everything else, acting on the focused pane.

within a pane:

- `/`; start/edit the live search query
- `j` / `k` (or down/up); move selection
- `gg` / `G`; jump to first/last
- `enter`; play or open the selection
- `esc`; close whatever's open
- `left` / `right`; switch column in the artist profile
- `h` / `l`; adjust a value inside a menu

the queue auto-advances when the current track ends; nothing plays next
if the queue is empty.
