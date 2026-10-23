# Deploy to EC2

Single binary `sonar` under systemd (`sonar.service`), config in `/etc/sonar/sonar.env`
(template: `sonar.env.example`), data in `/var/lib/sonar`.

- **Instance:** start with `c7g.large` (arm64, `SONAR_ARCH=aarch64`) or `c7i.large` (x86_64); Amazon Linux 2023 or
  Ubuntu; a gp3 root volume sized for the stored page captures. Resize after measuring.
- **Network:** no public inbound on 3002 (SSH only, from your address). Reach the API through
  `ssh -L 3002:127.0.0.1:3002 <host>` or a private VPC address. Require IMDSv2 and set the metadata
  hop limit to 1 so fetched pages cannot reach instance credentials; the app's destination guard is
  the first line of defence, this is the second.
- **First time:** `ssh <host> 'sudo bash -s' < scripts/server-setup.sh` (add `INSTALL_CHROME=1` via
  `sudo env INSTALL_CHROME=1 bash -s` for the browser tier), then edit
  `/etc/sonar/sonar.env` (set `SONAR_API_KEY` if the API is reachable by anything but you).
- **Browser tier:** Chrome runs as its own unit, `sonar-chrome.service` (user `sonar-chrome`), whose
  IP filter allows only loopback, so its one route out is sonar's guarded forward proxy on
  `127.0.0.1:3003`. Install it once (`deploy.sh` does not):
  `scp deploy/sonar-chrome.service <host>:/tmp/` then
  `ssh <host> 'sudo install -m 0644 /tmp/sonar-chrome.service /etc/systemd/system/ && sudo systemctl daemon-reload && sudo systemctl enable --now sonar-chrome'`,
  and set `SONAR_CHROME_WS=http://127.0.0.1:9222` and `SONAR_PUBLIC_URL` (the base of screenshot
  links) in `/etc/sonar/sonar.env`. Before relying on it, prove the filter blocks direct egress: this
  must fail: `sudo systemd-run --wait --pipe -p User=sonar-chrome -p IPAddressDeny=any -p IPAddressAllow=localhost curl -m 5 -sS https://example.com`.
- **Deploy:** `SONAR_ARCH=aarch64 scripts/deploy.sh <host>` builds, uploads, swaps, restarts, and waits for `/health`.
- **Logs:** `scripts/logs.sh <host> [lines]`.
- **Rollback:** `ssh <host> 'sudo mv /usr/local/bin/sonar.prev /usr/local/bin/sonar && sudo systemctl restart sonar'`.
  Each deploy overwrites `sonar.prev` with the binary it replaced.
