#!/usr/bin/env bash
# Build and deploy sonar to an EC2 host over ssh.
set -euo pipefail

usage() {
  cat <<'USAGE'
Usage: deploy.sh <ssh-host>
  Builds, uploads, swaps the binary (old one kept as sonar.prev), refreshes the
  unit, restarts, and polls /health for up to 30 s. Run server-setup.sh first.
Env:
  SONAR_ARCH  x86_64 (default) or aarch64; must match the host
USAGE
}

die() { echo "error: $*" >&2; exit 1; }

case "${1:-}" in -h | --help | "") usage; exit 0 ;; esac

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
host="$1"

bin="$("$ROOT/scripts/build-release.sh" "${SONAR_ARCH:-x86_64}")"
[[ -x "$bin" ]] || die "build produced no binary at $bin"

scp -q "$bin" "$host:/tmp/sonar.new"
scp -q "$ROOT/deploy/sonar.service" "$host:/tmp/sonar.service"

# The env file is never touched here; server-setup.sh creates it once.
ssh "$host" 'sudo bash -s' <<'REMOTE'
set -euo pipefail
[[ -f /etc/sonar/sonar.env ]] || { echo "run server-setup.sh first" >&2; exit 1; }
install -m 0755 -o root -g root /tmp/sonar.new /usr/local/bin/sonar.new
rm -f /tmp/sonar.new
[[ -f /usr/local/bin/sonar ]] && cp -p /usr/local/bin/sonar /usr/local/bin/sonar.prev
mv -f /usr/local/bin/sonar.new /usr/local/bin/sonar
if ! cmp -s /tmp/sonar.service /etc/systemd/system/sonar.service; then
  install -m 0644 /tmp/sonar.service /etc/systemd/system/sonar.service
  systemctl daemon-reload
fi
rm -f /tmp/sonar.service
systemctl enable sonar >/dev/null 2>&1
systemctl restart sonar
REMOTE

for _ in $(seq 1 30); do
  if ssh "$host" 'curl -fsS http://127.0.0.1:3002/health' >/dev/null 2>&1; then
    echo "healthy: $host"
    exit 0
  fi
  sleep 1
done
die "no healthy /health within 30 s; see scripts/logs.sh $host"
