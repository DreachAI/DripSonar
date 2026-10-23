#!/usr/bin/env bash
# Build a Linux release binary of sonar.
set -euo pipefail

usage() {
  cat <<'USAGE'
Usage: build-release.sh [x86_64|aarch64]   (default x86_64)
  Builds target/<triple>/release/sonar with cargo zigbuild, else cross.
USAGE
}

die() { echo "error: $*" >&2; exit 1; }

case "${1:-}" in -h | --help) usage; exit 0 ;; esac

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

arch="${1:-x86_64}"
case "$arch" in
  x86_64 | aarch64) triple="$arch-unknown-linux-gnu" ;;
  *) die "unknown arch '$arch' (x86_64 or aarch64)" ;;
esac

if cargo zigbuild --version >/dev/null 2>&1; then
  cargo zigbuild --release --locked --bin sonar --target "$triple" >&2
elif command -v cross >/dev/null 2>&1; then
  cross build --release --locked --bin sonar --target "$triple" >&2
else
  die "need cargo-zigbuild (cargo install cargo-zigbuild; install zig) or cross (cargo install cross)"
fi

echo "$ROOT/target/$triple/release/sonar"
