#!/bin/sh
# Builds the Rust core for iOS and assembles ios/WimCore: the generated
# Swift bindings and WimCore.xcframework. Needs macOS with Xcode and rustup.
set -eu
cd "$(dirname "$0")/.."
pkg=ios/WimCore
out=target/ios-gen
device=aarch64-apple-ios
sim=aarch64-apple-ios-sim
# C deps (zstd-sys) and rustc must agree on the minimum iOS, or the link fails.
export IPHONEOS_DEPLOYMENT_TARGET=16.0

rustup target add "$device" "$sim"
for t in "$device" "$sim"; do
  cargo build -p wim-mobile --release --locked --target "$t"
done

rm -rf "$out"
ios/gen-swift.sh "target/$sim/release/libwim_mobile.a" "$out"
mkdir -p "$pkg/Sources/WimCore"
cp "$out/wim_mobile.swift" "$pkg/Sources/WimCore/"

# xcframework headers: the module map must be named module.modulemap.
mkdir -p "$out/headers"
cp "$out/wim_mobileFFI.h" "$out/headers/"
cp "$out/wim_mobileFFI.modulemap" "$out/headers/module.modulemap"

rm -rf "$pkg/WimCore.xcframework"
xcodebuild -create-xcframework \
  -library "target/$device/release/libwim_mobile.a" -headers "$out/headers" \
  -library "target/$sim/release/libwim_mobile.a" -headers "$out/headers" \
  -output "$pkg/WimCore.xcframework"
