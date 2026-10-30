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
  # one of the user's own named eq curves, same shape as
  # eq::CustomPreset. Declared here and saved in-app with 's' both
  # land in the same config.toml list.
  eqPresetModule = { ... }: {
    options = {
      name = mkOption {
        type = types.str;
        description = "Name shown in the eq menu's \"saved\" group and accepted by eqPreset.";
      };
      gains = mkOption {
        type = types.listOf types.float;
        description = ''
          10 per-band dB gains (31Hz..16000Hz order, -12..12 each).
          The *requested* curve, exactly as the menu shows it; wim
          compensates for band overlap at apply time (see
          docs/audio-quality.md), so these are not filter gains.
        '';
      };
    };
  };

  builtinEqPresets = [ "flat" "bass boost" "vocal boost" "treble boost" "rock" "classical" ];
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

    videoOutput = mkOption {
      type = types.enum [ "auto" "tct" "kitty" "off" ];
      default = "auto";
      description = ''
        How 'v' draws a local video. mpv draws it itself, filling the
        terminal, while wim stops drawing. "tct" is true-color
        half-blocks and works in any terminal; "kitty" is the kitty
        graphics protocol and needs a terminal that speaks it. "auto"
        uses kitty only when the terminal answered the graphics probe
        with it, "tct" otherwise. "off" disables the key. Music
        playback is never affected.
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

    # same plaintext-vs-file split as opensubtitlesApiKeyFile, same
    # reason; neither set means scrobbling never runs. the token comes
    # from listenbrainz.org -> profile -> settings, not a password.
    listenbrainzUserTokenFile = mkOption {
      type = types.nullOr types.path;
      default = null;
      description = "Path to a file containing a ListenBrainz user token. Null disables scrobbling entirely.";
    };

    scrobble = {
      thresholdPercent = mkOption {
        type = types.float;
        default = 50.0;
        description = "Percent of a track's duration that must have played before scrobbling (listenbrainz's own convention: half the track). Whichever of this or thresholdSeconds is crossed first wins.";
      };
      thresholdSeconds = mkOption {
        type = types.float;
        default = 240.0;
        description = "Seconds of elapsed listening that trigger a scrobble regardless of track length (listenbrainz's own convention: 4 minutes).";
      };
    };

    logoStyle = mkOption {
      type = types.nullOr types.str;
      default = null;
      description = "Name of a home-screen logo style from the generated figlet/toilet batch (cycle with left/right in-app to find one, then set its exact name here). Null keeps the default ANSI Shadow logo.";
    };

    keybinds = mkOption {
      type = types.attrsOf types.str;
      default = { };
      example = { skip_next = "f5"; toggle_shuffle = "x"; };
      description = "Overrides for the remappable keybinds (action name -> key name, same as 'K' in-app writes to config.toml; see keymap::KeyAction::name/key_label in the wim source for the exact strings). Only needs entries for actions actually remapped away from their default.";
    };

    eqPreset = mkOption {
      type = types.nullOr types.str;
      default = null;
      description = ''
        Name of the eq curve to start with: either a built-in (e.g.
        "bass boost"), cycled through with 'e' in-app, or one of
        eqCustomPresets. Picked directly from the eq menu too (enter
        on the bottom bar's eq field while it has focus). Null falls
        back to eqGains, or flat if that's also unset.
      '';
    };

    eqGains = mkOption {
      type = types.nullOr (types.listOf types.float);
      default = null;
      description = ''
        10 hand-tuned per-band dB gains (31Hz..16000Hz order, -12..12
        each) for a nameless custom eq, used when eqPreset is null.
        Must have exactly 10 entries. Set alongside eqPreset, it is
        read as that curve nudged since (the eq menu shows it as
        "<name> (modified)"); leave it null to get the named curve
        exactly.
      '';
    };

    eqCustomPresets = mkOption {
      type = types.listOf (types.submodule eqPresetModule);
      default = [ ];
      example = [{
        name = "night bus";
        gains = [ (-4.0) (-2.0) 0.0 1.0 2.0 3.0 2.0 0.0 (-2.0) (-4.0) ];
      }];
      description = ''
        Named curves listed in the eq menu's "saved" group next to the
        built-ins, loadable by name via eqPreset. The in-app menu
        saves, renames and deletes entries in this same list ('s' /
        'r' / 'd d'), so anything declared here reappears on the next
        home-manager activation if the app deleted it.
      '';
    };

    cacheDaysThreshold = mkOption {
      type = types.int;
      default = 2;
      description = ''
        On how many distinct calendar days a remote song must have been
        played before wim automatically downloads it to the local cache
        (~/.cache/wim/tracks); playing it twice in one sitting does not
        count, coming back to it another day does. Tweakable live in the
        settings menu ('u').
      '';
    };

    sidebarWidth = mkOption {
      type = types.int;
      default = 32;
      description = ''
        Sidebar width in columns, clamped to 20-60. Also set live by
        resize mode ('r'), which writes the chosen size back to
        config.toml; set it here to pin a layout declaratively.
      '';
    };

    bottomBarHeight = mkOption {
      type = types.int;
      default = 4;
      description = ''
        Bottom bar height in rows, clamped to 2-10. Same resize-mode
        interaction as sidebarWidth.
      '';
    };

    cacheEvictionDays = mkOption {
      type = types.int;
      default = 7;
      description = ''
        How many days a cached track can go unplayed before its
        downloaded file is evicted to free disk space (play history
        itself is kept). Also tweakable in the settings menu.
      '';
    };

    mouse = mkOption {
      type = types.bool;
      default = true;
      description = ''
        Click to focus a pane or press a bottom-bar control, drag the
        scrubber to seek, wheel to scroll whichever list is under the
        pointer. On by default. Mouse capture takes over the
        terminal's own click-drag text selection; most terminals still
        let you hold shift to select and copy normally, and setting
        this false gives selection back unconditionally.
      '';
    };
  };

  config = mkIf cfg.enable {
    assertions = [
      {
        assertion = cfg.eqGains == null || builtins.length cfg.eqGains == 10;
        message = "programs.wim.eqGains must have exactly 10 entries (one per eq::BAND_HZ band) when set.";
      }
      {
        assertion = lib.all (p: builtins.length p.gains == 10) cfg.eqCustomPresets;
        message = "every programs.wim.eqCustomPresets entry must have exactly 10 gains (one per eq::BAND_HZ band).";
      }
      {
        # wim resolves a name against the built-ins first and refuses
        # to save over one (eq::NameCheck::Builtin), so a custom curve
        # sharing a built-in's name would simply be unreachable.
        assertion = lib.all
          (p: !(lib.elem (lib.toLower p.name) builtinEqPresets))
          cfg.eqCustomPresets;
        message = "programs.wim.eqCustomPresets names must not be one of the built-ins (${lib.concatStringsSep ", " builtinEqPresets}); wim resolves those first.";
      }
      {
        assertion = lib.length (lib.unique (map (p: lib.toLower p.name) cfg.eqCustomPresets))
          == lib.length cfg.eqCustomPresets;
        message = "programs.wim.eqCustomPresets names must be unique (wim compares them case-insensitively).";
      }
    ];

    home.packages = [ cfg.package ];

    xdg.configFile."wim/config.toml".source = tomlFormat.generate "wim-config.toml" ({
      album_art = cfg.albumArt;
      video_output = cfg.videoOutput;
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
      sidebar_width = cfg.sidebarWidth;
      bottom_bar_height = cfg.bottomBarHeight;
      cache_days_threshold = cfg.cacheDaysThreshold;
      cache_eviction_days = cfg.cacheEvictionDays;
      mouse = cfg.mouse;
      scrobble = {
        threshold_percent = cfg.scrobble.thresholdPercent;
        threshold_seconds = cfg.scrobble.thresholdSeconds;
      };
      navidrome_servers = map (s: {
        url = s.url;
        username = s.username;
        password_file = toString s.passwordFile;
      }
      // lib.optionalAttrs (s.maxBitrateKbps != null) { max_bitrate_kbps = s.maxBitrateKbps; }
      // lib.optionalAttrs (s.transcodeFormat != null) { transcode_format = s.transcodeFormat; }) cfg.navidromeServers;
    } // lib.optionalAttrs (cfg.opensubtitlesApiKeyFile != null) {
      opensubtitles_api_key_file = toString cfg.opensubtitlesApiKeyFile;
    } // lib.optionalAttrs (cfg.listenbrainzUserTokenFile != null) {
      listenbrainz_user_token_file = toString cfg.listenbrainzUserTokenFile;
    } // lib.optionalAttrs (cfg.logoStyle != null) {
      logo_style = cfg.logoStyle;
    } // lib.optionalAttrs (cfg.keybinds != { }) {
      keybinds = cfg.keybinds;
    } // lib.optionalAttrs (cfg.eqPreset != null) {
      eq_preset = cfg.eqPreset;
    } // lib.optionalAttrs (cfg.eqGains != null) {
      eq_gains = cfg.eqGains;
    } // lib.optionalAttrs (cfg.eqCustomPresets != [ ]) {
      eq_custom_presets = map (p: { inherit (p) name gains; }) cfg.eqCustomPresets;
    });
  };
}
