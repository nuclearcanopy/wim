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
    albumArt = "auto"; # auto | blocks | sixel | kitty | iterm2 | off
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
