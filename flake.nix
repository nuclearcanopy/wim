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
            # shellswim drives the same mpv layer, so it needs it too.
            postInstall = ''
              for bin in wim shellswim; do
                wrapProgram $out/bin/$bin --prefix PATH : ${pkgs.lib.makeBinPath [ pkgs.mpv ]}
              done
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
            # sccache cannot cache incremental compilation, and
            # Cargo.toml sets incremental = true on both profiles, so
            # every call was marked non-cacheable: measured 0 hits
            # against 2137 misses while the incremental dirs across 16
            # worktrees reached 22G. the env var takes precedence over
            # the manifest. the trade is slower repeated edits inside
            # one worktree, which is the right way round here:
            # incremental state is per-target-dir and cannot be shared,
            # where the sccache store is shared by all of them.
            export CARGO_INCREMENTAL=0
            export SCCACHE_DIR="$HOME/.cache/sccache"
          '';
        };
      });
    };
}
