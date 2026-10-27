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
      # every local user. passwordFile composes with agenix/sops-nix —
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
  };

  config = mkIf cfg.enable {
    home.packages = [ cfg.package ];

    xdg.configFile."wim/config.toml".source = tomlFormat.generate "wim-config.toml" {
      album_art = cfg.albumArt;
      library_paths = cfg.libraryPaths;
      navidrome_servers = map (s: {
        url = s.url;
        username = s.username;
        password_file = toString s.passwordFile;
      }) cfg.navidromeServers;
    };
  };
}
