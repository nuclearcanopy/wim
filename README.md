# wim

[![CI](https://github.com/nuclearcanopy/wim/actions/workflows/ci.yml/badge.svg)](https://github.com/nuclearcanopy/wim/actions/workflows/ci.yml)
[![Weekly commit activity](https://img.shields.io/github/commit-activity/w/nuclearcanopy/wim?label=commit%20activity)](https://github.com/nuclearcanopy/wim/graphs/commit-activity)
[![Rust](https://img.shields.io/badge/written%20in-Rust-dea584?logo=rust)](https://www.rust-lang.org/)
[![Nix flake](https://img.shields.io/badge/Nix-flake-5277c3?logo=nixos)](flake.nix)
[![License: AGPL-3.0-or-later](https://img.shields.io/badge/license-AGPL--3.0--or--later-blue)](LICENSE)

Wim is a terminal player for local media and Navidrome/Subsonic libraries.
Search spans local files and configured servers; mpv handles playback. Video
opens in an mpv window rather than being rendered in the terminal.

The project is in early development and is not yet considered usable. Expect
rough edges and changes to configuration and behavior.

## What it does

- Searches artists, albums, songs, playlists, local music and video together.
- Plays and queues files, with shuffle, repeat, speed, ReplayGain and a 10-band EQ.
- Shows album art using terminal graphics protocols or Unicode blocks, with a
  fallback for terminals without image support.
- Looks up and follows lyrics; supports local sidecars and server lyrics as well
  as optional online providers. See [lyrics](docs/lyrics.md).
- Provides subtitle controls for video and a separate TorBox workspace. See
  [video](docs/video.md) and [TorBox](docs/torbox.md).
- Offers keyboard and mouse navigation, configurable keybindings, and a
  Home Manager module. `shellswim` provides a command-oriented interface; see
  [shellsong](docs/shellsong.md).

Wim is written in Rust. It controls a separate mpv process over JSON IPC.

## Install and run

With Nix, build from a clone:

```sh
nix build .#wim
./result/bin/wim
```

The packaged `wim` has mpv on its `PATH`. To build from source instead:

```sh
nix develop
cargo build --release
./target/release/wim
```

Outside the Nix development shell, install Rust and mpv and ensure `mpv` is on
`PATH`. The source build also produces `shellswim`.

```sh
wim                    # open the player
wim --resume           # resume the saved session
wim album/*.flac       # play the first file and queue the rest
wim --help
shellswim --help
```

On first run, Wim creates a config at `$XDG_CONFIG_HOME/wim/config.toml` (or the
platform equivalent) and offers setup in the app. Local library paths default
to `~/Music` and `~/Videos` when present. Use the in-app profile manager (`n`)
to configure servers, or use the Home Manager module below.

## Controls

Press `?` in Wim for the full, current keymap; `K` opens keybinding settings.

| Key | Action |
| --- | --- |
| `Tab` | Switch to pane-movement mode |
| `w` `a` `s` `d` or arrows | Focus a pane in movement mode |
| `Enter` or `Esc` | Return to interaction mode |
| `/` | Search |
| `j` / `k`, `gg` / `G` | Move through a list |
| `Enter` | Play or open the selection |
| `L` | Open lyrics |

Mouse capture is on by default; set `mouse = false` in the config to disable
it. With capture enabled, holding Shift while dragging usually restores the
terminal's text selection.

Environment switches: `NO_COLOR=1` forces greyscale (including album art left
on `auto`), `WIM_COLOR=semantic` adds state colours, `WIM_REDUCED_MOTION=1` (or
`reduced_motion = true` in the config) stills the banner sweep and selection
pulse, `WIM_BACKGROUND=light|dark` overrides light-terminal detection
(`COLORFGBG`), and `WIM_TRUECOLOR=1` skips the 256-colour fallback chosen from
`COLORTERM`/`TERM`.

## Home Manager

The flake exports `homeManagerModules.default`. With the flake available as
`wim`, for example:

```nix
{ config, wim, ... }: {
  imports = [ wim.homeManagerModules.default ];

  programs.wim = {
    enable = true;
    navidromeServers = [{
      url = "https://navidrome.example.com";
      username = "me";
      passwordFile = config.age.secrets.navidrome-password.path;
    }];
    # libraryPaths = [ "/home/me/Music" ];
  };
}
```

Pass `wim` through your Home Manager module arguments. The example assumes an
existing agenix secret; any suitably protected password file can be used. Do
not put passwords in Nix values: the Nix store is world-readable. Multiple
servers feed the same search. See [`nix/home-manager.nix`](nix/home-manager.nix)
for the available options.

## Performance

The repository includes existing search benchmarks, a Linux process benchmark,
and an operator-assisted resource comparison with Feishin and Supersonic.
There are **no published comparative results yet**; the suite does not measure
end-to-end search, time to audible playback, or the Navidrome web client. See
[performance methodology](docs/performance.md) for commands, limitations, and
how to regenerate a results table. No speed or memory figure is a substitute
for the workload and machine it was measured on.

## License

[AGPL-3.0-or-later](LICENSE).
