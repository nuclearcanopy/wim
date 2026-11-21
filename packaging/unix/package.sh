#!/bin/sh
# Usage: package.sh <label> [release-dir]
# Packages wim and shellswim (plus README and LICENSE) from <release-dir>
# (default target/release) into wim-v<version>-<label>.tar.gz in the cwd.
# The version comes from Cargo.toml. POSIX sh: also runs inside the FreeBSD VM.
set -eu
label=$1
dir=${2:-target/release}
version=$(sed -n 's/^version *= *"\([^"]*\)".*/\1/p' Cargo.toml | head -n 1)
name="wim-v${version}-${label}"
rm -rf "$name"
mkdir "$name"
for f in wim shellswim; do
  [ -f "$dir/$f" ] || { echo "missing $dir/$f" >&2; exit 1; }
  cp "$dir/$f" "$name/"
done
for f in README.md LICENSE; do
  [ -f "$f" ] && cp "$f" "$name/"
done
tar -czf "$name.tar.gz" "$name"
rm -rf "$name"
echo "$name.tar.gz"
