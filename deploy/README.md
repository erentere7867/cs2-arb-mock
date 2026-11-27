# VPS operations

The jobs run as `ubuntu` from the existing checkout at `/home/ubuntu/cs2-arb-mock`.
The VPS needs systemd, jq, Rust/Cargo (installed for `ubuntu`), the workspace's build
prerequisites and GitHub access. From that checkout, install or refresh the VPS files:

```sh
umask 077
test -f .env || install -m 600 /dev/null .env
chmod 600 .env
sudo install -m 644 deploy/*.service /etc/systemd/system/
sudo install -m 755 deploy/cs2-arb-run deploy/cs2-arb-update /usr/local/bin/
sudo systemctl daemon-reload
cs2-arb-update main
sudo systemctl enable cs2-arb-watch cs2-arb-mock@cs2 cs2-arb-mock@rust cs2-arb-dashboard
```

To update once the next PR lands, run **`cs2-arb-update main`** as `ubuntu`. It
also accepts another branch on `origin`. It refuses a dirty checkout and builds
the exact remote commit before switching the runtime release and restarting.
The deployed branch and commit are in `/home/ubuntu/cs2-arb-mock-data/current/revision`.

Add `CSFLOAT_API_KEY` to `/home/ubuntu/cs2-arb-mock/.env` when available (systemd
`KEY=value` syntax, no `export`), then run
`sudo systemctl restart cs2-arb-mock@cs2`. The CS2 and Rust jobs use their
branch's default live venues.
Skinport reads are public; DMarket's existing offers adapter requires both
credentials and opt-in, so these jobs do not use it by default.

To block new simulated orders in both jobs, create
`/home/ubuntu/cs2-arb-mock-data/KILL_SWITCH` (not a file in the checkout).
To resume, remove the file and restart both mock units to clear the latched switch.

## Read results

The dashboard listens on **TCP 8787** (`0.0.0.0:8787`) once deployed.
In Nginx Proxy Manager, create a **Proxy Host** with scheme **http**, forward
hostname/IP set to the VPS address reachable from NPM (use the Docker host
gateway or the VPS's private address if NPM runs in Docker), and forward
port **8787**. Enable HTTPS and restrict access using an NPM Access List.
The dashboard has no authentication of its own: firewall TCP 8787 so only
the proxy can reach it, or bind to loopback if the proxy runs on the host.
No WebSocket setup is required.

The dashboard polls a local, read-only snapshot every five seconds. It uses
the completed Markdown reports, the in-progress marker, the kill-switch file,
the watcher's SQLite data (read-only), and, if readable by the service user,
bounded samples from the two mock jobs' systemd journals. It displays observed
CSFloat throttle signals, not an IP-ban diagnosis; missing journal samples or
zero signals do not establish that CSFloat is healthy. The dashboard does not
load the engine's `.env` or make marketplace requests. Inspect its startup
with `sudo journalctl -u cs2-arb-dashboard -n 50`.

To override the listening address or reports path, use
`sudo systemctl edit cs2-arb-dashboard`, then restart it after saving the
drop-in. The watcher DB and kill-switch file are found next to the reports directory.

```ini
[Service]
Environment=CS2_ARB_DASHBOARD_BIND=127.0.0.1:8787
Environment=CS2_ARB_DASHBOARD_REPORTS_DIR=/home/ubuntu/cs2-arb-mock-data/reports
```

For command-line inspection, replace `REPORT.md` with a filename from the listing.

```sh
ls -1t /home/ubuntu/cs2-arb-mock-data/reports/{cs2,rust}/*.md
less /home/ubuntu/cs2-arb-mock-data/reports/cs2/REPORT.md
sudo journalctl -u cs2-arb-mock@cs2 -u cs2-arb-mock@rust -u cs2-arb-watch -n 100
```

The jobs repeat bounded live GET-only batches. Each completed batch saves a
UTC-timestamped Markdown report with mock realized, unrealized and total P&L and
the peg dashboard. Batches do not share simulated positions or accumulated P&L;
do not sum them as one portfolio. If no usable live quotes arrive, see the
HTTP errors in the journal; the job retries and does not write a fake report.
Reports and the CS2 Steam watcher database (`events.db`) are outside Git and
survive restarts and updates.
