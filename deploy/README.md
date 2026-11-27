# VPS operations

The jobs run as `ubuntu` from the existing checkout at `/home/ubuntu/cs2-arb-mock`.
The VPS needs systemd, jq, Rust/Cargo (installed for `ubuntu`), the workspace's build
prerequisites and GitHub access. From that checkout, install or refresh the VPS files:

```sh
test -z "$(git status --porcelain)" &&
  git fetch --prune origin &&
  git switch --detach origin/main
```

Proceed only if the checkout succeeded and that revision includes the dashboard.
Use the updater from this checkout for the first rollout, not a previously
installed copy:

```sh
umask 077
test -f .env || install -m 600 .env.example .env
chmod 600 .env
sudo install -m 644 deploy/*.service /etc/systemd/system/
sudo install -m 755 deploy/cs2-arb-run deploy/cs2-arb-update /usr/local/bin/
sudo systemctl daemon-reload
./deploy/cs2-arb-update main
sudo systemctl enable cs2-arb-watch cs2-arb-mock@cs2 cs2-arb-mock@rust
sudo systemctl enable --now cs2-arb-dashboard
```

Run `sudo systemctl enable --now cs2-arb-dashboard` once on first rollout so
the dashboard starts now and on later boots. The updater restarts it on
revisions that include it; it does not change whether it is enabled at boot.

To update once the next PR lands, run **`cs2-arb-update main`** as `ubuntu`. It
also accepts another branch on `origin`. It refuses a dirty checkout and builds
the exact remote commit before switching the runtime release and restarting.
The deployed branch and commit are in `/home/ubuntu/cs2-arb-mock-data/current/revision`.

See [`.env.example`](../.env.example) for the credential names. If `.env`
already exists, edit `/home/ubuntu/cs2-arb-mock/.env` in place rather than
replacing it. Use systemd `KEY=value` syntax, without `export`.

Skinport and Waxpeer reads are public and need no keys. CSFloat makes
unauthenticated requests without `CSFLOAT_API_KEY`; those requests can be rate limited.
DMarket has no public fallback. Both jobs automatically add DMarket's signed,
read-only requests when `DMARKET_PUBLIC_KEY` and `DMARKET_SECRET_KEY` form a
valid key pair. If either key is blank, missing, or invalid, the jobs skip
DMarket and continue with the other venues. Once the keys are in `.env`,
restart the jobs; no config edit is needed:

```sh
sudo systemctl restart cs2-arb-mock@cs2 cs2-arb-mock@rust
```

To block new simulated orders in both jobs, create
`/home/ubuntu/cs2-arb-mock-data/KILL_SWITCH` (not a file in the checkout).
To resume, remove the file and restart both mock units to clear the latched switch.

## Read results

The dashboard listens on **TCP 8787** (`0.0.0.0:8787`) once deployed.
In Nginx Proxy Manager, create a **Proxy Host** with scheme **http**, forward
hostname/IP set to the VPS address reachable from NPM (use the Docker host
gateway or the VPS's private address if NPM runs in Docker), and forward
port **8787**. Enable HTTPS and restrict access using an NPM Access List.
The dashboard has no authentication of its own: **do not expose TCP 8787 to
the public internet**. Only NPM should be able to reach it. For example, with
UFW and default-denied inbound traffic, allow only the actual NPM Docker
subnet (`sudo ufw allow from 172.18.0.0/16 to any port 8787 proto tcp`, replacing
the example subnet) and remove any broader rule for 8787. If the proxy runs
on the host, bind the dashboard to loopback instead.
No WebSocket setup is required.

The dashboard polls a local, read-only snapshot every five seconds. It uses
the completed Markdown reports, the in-progress marker, the kill-switch file,
the watcher's SQLite data (read-only), and, if readable by the service user,
bounded samples from the two mock jobs' systemd journals. It displays observed
CSFloat throttle signals, not an IP-ban diagnosis; missing journal samples or
zero signals do not establish that CSFloat is healthy. The dashboard does not
load the engine's `.env` or make marketplace requests. Inspect its startup
with `sudo journalctl -u cs2-arb-dashboard -n 50`.

To let the dashboard process running as `ubuntu` read the system journal
when it cannot already, add this drop-in via
`sudo systemctl edit cs2-arb-dashboard`, then run
`sudo systemctl restart cs2-arb-dashboard`:

```ini
[Service]
SupplementaryGroups=systemd-journal
```

This grants the dashboard process access to the system journal, not just the
two mock units; enable it only if that is acceptable. The dashboard queries
only those two units and exposes counters, not raw logs. Leave the override
out if you prefer to keep journal access off.

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
