# home-manager module generating wim's config.toml declaratively. Takes
# `wimPackage` as an extra argument (partially applied by flake.nix,
# which is where this module actually has access to the flake's own
# `packages.${system}.wim`) rather than hardcoding a package lookup here.
{ wimPackage }:
{ config, lib, pkgs, ... }:
let
  inherit (lib) mkEnableOption mkOption mkIf types;
  cfg = config.programs.wim;
  tomlFormat = pkgs.formats.toml { };

  navidromeServerModule = { ... }: {
    options = {
      url = mkOption {
        type = types.str;
        description = "Navidrome/Subsonic server URL.";
      };
      username = mkOption {
        type = types.str;
      };
      # Deliberately no plaintext `password` option: Nix store paths are
      # world-readable, so a literal password value here would leak to
      # every local user. passwordFile composes with agenix/sops-nix;
      # point it at a decrypted secret path. See docs/architecture.md.
      passwordFile = mkOption {
        type = types.path;
        description = ''
          Path to a file containing the Navidrome password (e.g. an
          agenix secret), trailing newline trimmed. Required to
          actually authenticate; there is no plaintext-password option
          by design.
        '';
      };

      # server-side transcoding already happens by default per
      # navidrome's own configured profile; these are optional
      # client-side overrides for this specific server.
      maxBitrateKbps = mkOption {
        type = types.nullOr types.int;
        default = null;
        description = "Client-side max bitrate override (kbps) sent on stream requests to this server. Null leaves it up to the server.";
      };

      transcodeFormat = mkOption {
        type = types.nullOr types.str;
        default = null;
        description = "Client-side transcode format override (e.g. \"opus\", \"mp3\") sent on stream requests to this server. Null leaves it up to the server.";
      };
    };
  };
in
{
  options.programs.wim = {
    enable = mkEnableOption "wim, the terminal omniplayer";

    package = mkOption {
      type = types.package;
      default = wimPackage;
      description = "wim package to install.";
    };

    # a list, not a single server: wim merges every configured server
    # into one unified search corpus rather than picking an "active"
    # one; see ROADMAP.md. the in-app 'n' profile manager can add more
    # on top of whatever's declared here.
    navidromeServers = mkOption {
      type = types.listOf (types.submodule navidromeServerModule);
      default = [ ];
      description = "Navidrome/Subsonic servers to connect on startup.";
    };

    albumArt = mkOption {
      type = types.enum [ "auto" "blocks" "sixel" "kitty" "iterm2" "off" ];
      default = "auto";
      description = ''
        "blocks" forces the pixelated unicode-block look (desaturated
        to grayscale) even on terminals that support real image
        protocols, for users who want the ui to stay purely monochrome.
      '';
    };

    libraryPaths = mkOption {
      type = types.listOf types.str;
      default = [ ];
      description = ''
        Directories to scan for local music/video. Left empty, wim
        defaults to `~/Music` and `~/Videos` on its own.
      '';
    };

    crossfadeEnabled = mkOption {
      type = types.bool;
      default = false;
      description = ''
        Fades the end of one track into the start of the next rather
        than a hard cut, separate from (and on top of) gapless
        playback, which is always on. Toggleable at runtime with 'c'.
      '';
    };

    crossfadeSeconds = mkOption {
      type = types.float;
      default = 3.0;
      description = "Length of the crossfade fade-out/fade-in ramp, in seconds.";
    };

    subtitles = {
      delaySeconds = mkOption {
        type = types.float;
        default = 0.0;
        description = "Positive delays subtitles later. Also adjustable live with ','/'.' while something's playing.";
      };
      fontSize = mkOption {
        type = types.int;
        default = 42;
      };
      position = mkOption {
        type = types.int;
        default = 10;
        description = "0 (bottom) to 150 (top), mpv's own sub-pos range.";
      };
      outlineSize = mkOption {
        type = types.float;
        default = 2.0;
      };
      backgroundOpacity = mkOption {
        type = types.float;
        default = 0.0;
        description = "0.0 (fully transparent) to 1.0 (opaque).";
      };
      blur = mkOption {
        type = types.float;
        default = 0.0;
      };
      colorLevel = mkOption {
        type = types.float;
        default = 1.0;
        description = ''
          0.0 (black) to 1.0 (white) text color; always a single
          grayscale level, never a hue, per the monochrome rule.
        '';
      };
    };

    # same plaintext-vs-file split as navidromeServers.passwordFile,
    # same reason (world-readable nix store paths); neither set means
    # the opensubtitles auto-fetch feature never runs.
    opensubtitlesApiKeyFile = mkOption {
      type = types.nullOr types.path;
      default = null;
      description = "Path to a file containing an opensubtitles.com api key. Null disables auto-fetch (manually-placed subtitle files alongside a video still work regardless).";
    };

    subtitleLanguage = mkOption {
      type = types.str;
      default = "en";
      description = "Preferred language code for opensubtitles auto-fetch.";
    };
  };

  config = mkIf cfg.enable {
    home.packages = [ cfg.package ];

    xdg.configFile."wim/config.toml".source = tomlFormat.generate "wim-config.toml" ({
      album_art = cfg.albumArt;
      library_paths = cfg.libraryPaths;
      crossfade_enabled = cfg.crossfadeEnabled;
      crossfade_seconds = cfg.crossfadeSeconds;
      subtitles = {
        delay_secs = cfg.subtitles.delaySeconds;
        font_size = cfg.subtitles.fontSize;
        position = cfg.subtitles.position;
        outline_size = cfg.subtitles.outlineSize;
        background_opacity = cfg.subtitles.backgroundOpacity;
        blur = cfg.subtitles.blur;
        color_level = cfg.subtitles.colorLevel;
      };
      subtitle_language = cfg.subtitleLanguage;
      navidrome_servers = map (s: {
        url = s.url;
        username = s.username;
        password_file = toString s.passwordFile;
      }
      // lib.optionalAttrs (s.maxBitrateKbps != null) { max_bitrate_kbps = s.maxBitrateKbps; }
      // lib.optionalAttrs (s.transcodeFormat != null) { transcode_format = s.transcodeFormat; }) cfg.navidromeServers;
    } // lib.optionalAttrs (cfg.opensubtitlesApiKeyFile != null) {
      opensubtitles_api_key_file = toString cfg.opensubtitlesApiKeyFile;
    });
  };
}
