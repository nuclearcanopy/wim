# Release

Pushing a `v*` tag runs `.github/workflows/release.yml`. The tag must match the
`Cargo.toml` version (checked in the `meta` job and the Windows job). Jobs:

- `linux`: ubuntu-latest, `wim-v<ver>-linux-x86_64.tar.gz`.
- `macos`: macos-latest, one tarball per arch (`macos-aarch64`, `macos-x86_64`,
  both `cargo build --release --target ...`; no universal binary).
- `freebsd`: FreeBSD 14.5 VM (same pinned action as ci.yml),
  `wim-v<ver>-freebsd-x86_64.tar.gz`.
- `windows`: zip and Inno Setup installer with bundled mpv (below).
- `publish`: the only job with `contents: write`. Downloads all artifacts,
  writes `SHA256SUMS` over every asset and creates the GitHub release.

Tarballs hold `wim`, `shellswim`, `README.md` and `LICENSE`
(`packaging/unix/package.sh`). Unix builds do not bundle mpv.
Manual `workflow_dispatch` runs default to `dry_run`: they build and upload
artifacts but never publish. iOS, Android and signing are not part of this flow.

## Pinned mpv

shinchiro/mpv-winbuild-cmake release `20261111`, asset
`mpv-x86_64-20261111-git-bcde6b93cc.7z` (baseline x86_64, not the v3 build),
SHA256 `f259b5fd4b5c395617c948a4fadcdf55bba1c5282fb7c13e88626b3f71072baf`.
Contains `mpv.exe`. To bump: change `MPV_URL` and `MPV_SHA256` in release.yml.
