#!/usr/bin/env bash
# Build and deploy sonar to an EC2 host over ssh.
set -euo pipefail

usage() {
  cat <<'USAGE'
Usage: deploy.sh <ssh-host>
  Builds, uploads, swaps the binary (old one kept as sonar.prev), refreshes the
  unit, restarts, and polls /health for up to 30 s. Run server-setup.sh first.
  Also installs or refreshes sonar-chrome.service and restarts it before sonar
  when SONAR_CHROME=1 or the unit is already installed on the host.
Env:
  SONAR_ARCH    x86_64 (default) or aarch64; must match the host
  SONAR_CHROME  1 to install the browser-tier unit (needs server-setup.sh with INSTALL_CHROME=1)
USAGE
}

die() { echo "error: $*" >&2; exit 1; }

case "${1:-}" in -h | --help | "") usage; exit 0 ;; esac

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
host="$1"
chrome=0
[[ "${SONAR_CHROME:-0}" == 1 ]] && chrome=1

bin="$("$ROOT/scripts/build-release.sh" "${SONAR_ARCH:-x86_64}")"
[[ -x "$bin" ]] || die "build produced no binary at $bin"

scp -q "$bin" "$host:/tmp/sonar.new"
scp -q "$ROOT/deploy/sonar.service" "$host:/tmp/sonar.service"
scp -q "$ROOT/deploy/sonar-chrome.service" "$host:/tmp/sonar-chrome.service"

# The env file is never touched here; server-setup.sh creates it once.
ssh "$host" "sudo env SONAR_CHROME=$chrome bash -s" <<'REMOTE'
set -euo pipefail
[[ -f /etc/sonar/sonar.env ]] || { echo "run server-setup.sh first" >&2; exit 1; }
chrome_unit=/etc/systemd/system/sonar-chrome.service
[[ -f "$chrome_unit" ]] && SONAR_CHROME=1
if [[ "$SONAR_CHROME" == 1 ]] && ! id sonar-chrome >/dev/null 2>&1; then
  echo "no sonar-chrome user: run server-setup.sh with INSTALL_CHROME=1" >&2
  exit 1
fi
install -m 0755 -o root -g root /tmp/sonar.new /usr/local/bin/sonar.new
rm -f /tmp/sonar.new
[[ -f /usr/local/bin/sonar ]] && cp -p /usr/local/bin/sonar /usr/local/bin/sonar.prev
mv -f /usr/local/bin/sonar.new /usr/local/bin/sonar
reload=0
if ! cmp -s /tmp/sonar.service /etc/systemd/system/sonar.service; then
  install -m 0644 /tmp/sonar.service /etc/systemd/system/sonar.service
  reload=1
fi
if [[ "$SONAR_CHROME" == 1 ]] && ! cmp -s /tmp/sonar-chrome.service "$chrome_unit"; then
  install -m 0644 /tmp/sonar-chrome.service "$chrome_unit"
  reload=1
fi
rm -f /tmp/sonar.service /tmp/sonar-chrome.service
if [[ "$reload" == 1 ]]; then systemctl daemon-reload; fi
if [[ "$SONAR_CHROME" == 1 ]]; then
  systemctl enable sonar-chrome >/dev/null 2>&1
  systemctl restart sonar-chrome
fi
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
