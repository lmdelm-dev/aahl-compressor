#!/usr/bin/env bash
# Builds the AAHL CLI as a statically linked musl binary for Linux.
#
# Usage:
#   scripts/build-linux.sh            # -> target/release/aahl (musl, static)
#   AAHL_TARGET=x86_64-unknown-linux-musl scripts/build-linux.sh
#
# Requires: rustup + the musl target + a musl linker. Easiest setups:
#   Ubuntu/Debian:  apt-get install musl-tools && rustup target add x86_64-unknown-linux-musl
#   Alpine:         apk add musl-dev build-base && rustup target add x86_64-unknown-linux-musl
#
# The result is fully static (`file` should report "statically linked") and
# runs on any glibc or musl distribution without dependencies.

set -euo pipefail

TARGET="${AAHL_TARGET:-x86_64-unknown-linux-musl}"
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

cd "$ROOT"

# Use the musl target's own linker if available (musl-gcc on Ubuntu).
export CARGO_TARGET_X86_64_UNKNOWN_LINUX_MUSL_LINKER="${CARGO_TARGET_X86_64_UNKNOWN_LINUX_MUSL_LINKER:-cc}"

echo "Building aahl (CLI only) for $TARGET ..."
cargo build --release --target "$TARGET" -p aahl

BIN="target/$TARGET/release/aahl"
echo ""
echo "Built: $BIN"
if command -v file >/dev/null 2>&1; then
  file "$BIN"
fi
echo "Size: $(du -h "$BIN" | cut -f1)"