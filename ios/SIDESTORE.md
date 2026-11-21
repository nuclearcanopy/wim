# Sideloading with SideStore

Source URL to add in SideStore:

    https://github.com/nuclearcanopy/wim/releases/download/ios-latest/altstore-source.json

One-time SideStore setup (pairing, anisette, certificate): follow the
SideStore setup guide at https://sidestore.io.

Updates: run the `ios-release` workflow (Actions, manual dispatch) on the
branch. It rebuilds an unsigned IPA and replaces the rolling `ios-latest`
prerelease. The version comes from `Cargo.toml`, the build number from the CI
run number; SideStore sees the new build when it refreshes the source. SideStore
signs the IPA on the device with your own Apple ID, so no signing data lives in
CI or the repo.
