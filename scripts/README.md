# Scripts

Operational scripts, bash only, run from any directory (each resolves the repo root itself).

Conventions: `set -euo pipefail`; `-h`/`--help` or missing arguments print usage; `die()` for
errors; required env checked up front with a clear message; secrets never echoed or copied;
safe to run twice.

To add one: `cp scripts/_template.sh scripts/<name>.sh`, edit, `chmod +x`, add a row below.

| Script | Purpose |
| --- | --- |
| `build-release.sh [arch]` | Cross-build a Linux release binary (x86_64 or aarch64); prints its path. |
| `deploy.sh <ssh-host>` | Build, upload, swap in, restart, and health-check on the host. |
| `server-setup.sh` | One-time host setup (user, data dir, env file, optional Chromium); run on the host. |
| `logs.sh <ssh-host> [lines]` | Follow the service's journald logs. |

Deploy details: [../deploy/README.md](../deploy/README.md).
