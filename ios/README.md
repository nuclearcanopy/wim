# shellswim (iOS)

Chat front end for wim: each message you send is a wim command.

## Build

1. `ios/build-rust.sh` (builds the Rust core, Swift bindings, xcframework; macOS).
2. `brew install xcodegen`, then in `ios/`: `xcodegen generate`.
3. Copy `ios/Config/Local.xcconfig.example` to `ios/Config/Local.xcconfig` (gitignored)
   and put your Apple team ID in it.
4. Open `ios/Shellswim.xcodeproj` and run on a device or simulator.

Bundle ID `dev.loki.shellswim`. The `.xcodeproj` is generated from `ios/project.yml`
and not committed.

## Server

- The phone talks to Navidrome over HTTPS only; `http://` URLs are rejected.
- Create a dedicated low-privilege Navidrome user for the phone (no admin rights).
- Server URL, user and password are entered in Settings and stored in the Keychain
  (service `dev.loki.shellswim`). The password is handed to the core in memory and
  is never written to disk or logged.
- The transcript (last 500 bubbles) is stored as JSON in Application Support.

Background audio and lock-screen controls are not implemented yet (M4).
