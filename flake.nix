{
  description = "wim — terminal omniplayer for music and video";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";

  outputs = { self, nixpkgs }:
    let
      systems = [ "x86_64-linux" "aarch64-linux" "aarch64-darwin" "x86_64-darwin" ];
      forAll = f: nixpkgs.lib.genAttrs systems (sys: f nixpkgs.legacyPackages.${sys});
    in {
      devShells = forAll (pkgs: {
        default = pkgs.mkShell {
          packages = with pkgs; [
            cargo rustc rustfmt clippy rust-analyzer
            mold sccache clang
            mpv
          ];

          shellHook = ''
            export RUSTC_WRAPPER="${pkgs.sccache}/bin/sccache"
            export SCCACHE_DIR="$HOME/.cache/sccache"
          '';
        };
      });
    };
}
