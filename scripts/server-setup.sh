#!/usr/bin/env bash
# One-time host setup. Run ON the instance as root:
#   ssh host 'sudo bash -s' < scripts/server-setup.sh
set -euo pipefail

usage() {
  cat <<'USAGE'
Usage: ssh <host> 'sudo bash -s' < scripts/server-setup.sh
  Creates user sonar, /var/lib/sonar, and /etc/sonar/sonar.env (if missing).
Env:
  INSTALL_CHROME=1  also install Chromium (apt or dnf) and create user sonar-chrome
                    for deploy/sonar-chrome.service
USAGE
}

die() { echo "error: $*" >&2; exit 1; }

case "${1:-}" in -h | --help) usage; exit 0 ;; esac
[[ "$(id -u)" -eq 0 ]] || die "run as root"

id sonar >/dev/null 2>&1 || useradd --system --shell /usr/sbin/nologin --home-dir /var/lib/sonar --no-create-home sonar
install -d -m 0750 -o sonar -g sonar /var/lib/sonar
install -d -m 0750 -o root -g sonar /etc/sonar

if [[ ! -f /etc/sonar/sonar.env ]]; then
  cat > /etc/sonar/sonar.env <<'ENV'
SONAR_BIND=127.0.0.1:3002
SONAR_DATA_DIR=/var/lib/sonar
RUST_LOG=info
ENV
  chown root:sonar /etc/sonar/sonar.env
  chmod 0640 /etc/sonar/sonar.env
  echo "created /etc/sonar/sonar.env (see deploy/sonar.env.example for options)"
fi

if [[ "${INSTALL_CHROME:-0}" == 1 ]]; then
  if command -v apt-get >/dev/null 2>&1; then
    apt-get update -qq && apt-get install -y -qq chromium || apt-get install -y -qq chromium-browser
  elif command -v dnf >/dev/null 2>&1; then
    dnf install -y -q chromium
  else
    die "no apt-get or dnf; install Chromium manually"
  fi
  id sonar-chrome >/dev/null 2>&1 || useradd --system --shell /usr/sbin/nologin --home-dir /nonexistent --no-create-home sonar-chrome
fi

echo "setup done (file descriptor limit comes from the unit: LimitNOFILE)"
