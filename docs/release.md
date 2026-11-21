# Release

Pushing a `v*` tag runs `.github/workflows/release.yml` (windows-latest): builds
`wim.exe`/`shellswim.exe`, bundles mpv, builds a zip and an Inno Setup installer,
writes `SHA256SUMS`, and publishes all three to the GitHub Release. The tag must
match the `Cargo.toml` version. Only Windows artifacts are built.

## Pinned mpv

shinchiro/mpv-winbuild-cmake release `20261111`, asset
`mpv-x86_64-20261111-git-bcde6b93cc.7z` (baseline x86_64, not the v3 build),
SHA256 `f259b5fd4b5c395617c948a4fadcdf55bba1c5282fb7c13e88626b3f71072baf`.
Contains `mpv.exe`. To bump: change `MPV_URL` and `MPV_SHA256` in release.yml.
