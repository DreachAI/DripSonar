#!/usr/bin/env bash
# One-line purpose of the script.
set -euo pipefail

usage() {
  cat <<'USAGE'
Usage: NAME.sh <arg> [optional]
  What it does, in one line.
Env:
  REQUIRED_VAR  what it is
USAGE
}

die() { echo "error: $*" >&2; exit 1; }

case "${1:-}" in -h | --help | "") usage; exit 0 ;; esac

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

: "${REQUIRED_VAR:?REQUIRED_VAR must be set}" # never echo secret values

# Body: safe to run twice; check state before changing it.
