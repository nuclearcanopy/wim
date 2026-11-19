#!/bin/sh
# Generates the Swift bindings for wim-mobile from a built library.
# Works on any host: ios/gen-swift.sh target/debug/libwim_mobile.so /tmp/out
set -eu
[ $# -eq 2 ] || { echo "usage: $0 <library> <out-dir>" >&2; exit 2; }
cd "$(dirname "$0")/.."
lib=$(cd "$(dirname "$1")" && pwd)/$(basename "$1")
mkdir -p "$2"
cargo run -q -p wim-mobile --features cli --bin uniffi-bindgen -- \
  generate --library "$lib" --language swift --out-dir "$2"
