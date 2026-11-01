{
  description = "wim, a terminal omniplayer for music and video";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";

  outputs = { self, nixpkgs }:
    let
      systems = [ "x86_64-linux" "aarch64-linux" "aarch64-darwin" "x86_64-darwin" ];
      forAll = f: nixpkgs.lib.genAttrs systems (sys: f nixpkgs.legacyPackages.${sys});
    in {
      packages = forAll (pkgs:
        let
          wim = pkgs.rustPlatform.buildRustPackage {
            pname = "wim";
            version = "0.1.0";
            src = ./.;
            cargoLock = { lockFile = ./Cargo.lock; };

            nativeBuildInputs = [ pkgs.makeWrapper ];

            # the frame source spawns a real mpv and reads rawvideo
            # back off a fifo, so its tests need the binary at check
            # time, not only at runtime. without this the sandbox has
            # no mpv and 19 of them fail, which is what `nix build`
            # was failing on while the fmt step hid it.
            nativeCheckInputs = [ pkgs.mpv ];

            # mpv is a runtime subprocess dependency (driven over its
            # json ipc socket, see docs/architecture.md), not a linked
            # library: wrap the binary so it's on PATH without needing
            # it installed system-wide.
            postInstall = ''
              wrapProgram $out/bin/wim --prefix PATH : ${pkgs.lib.makeBinPath [ pkgs.mpv ]}
            '';

            meta = {
              description = "terminal omniplayer for music and video";
              homepage = "https://github.com/nuclearcanopy/wim";
              license = pkgs.lib.licenses.agpl3Plus;
              mainProgram = "wim";
            };
          };
        in {
          default = wim;
          inherit wim;
        });

      homeManagerModules.default = { config, lib, pkgs, ... }@hmArgs:
        import ./nix/home-manager.nix {
          wimPackage = self.packages.${pkgs.stdenv.hostPlatform.system}.wim;
        } hmArgs;

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
