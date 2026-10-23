#!/usr/bin/env bash
# Tail the sonar service logs from journald.
set -euo pipefail

usage() {
  cat <<'USAGE'
Usage: logs.sh <ssh-host> [lines]   (default 100, then follows)
USAGE
}

die() { echo "error: $*" >&2; exit 1; }

case "${1:-}" in -h | --help | "") usage; exit 0 ;; esac

lines="${2:-100}"
[[ "$lines" =~ ^[0-9]+$ ]] || die "lines must be a number"
exec ssh "$1" "sudo journalctl -u sonar -n $lines -f --no-pager"
