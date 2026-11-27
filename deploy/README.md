# VPS operations

The jobs run as `ubuntu` from the existing checkout at `/home/ubuntu/cs2-arb-mock`.
The VPS needs systemd, jq, Rust/Cargo (installed for `ubuntu`), the workspace's build
prerequisites and GitHub access. From that checkout, install or refresh the VPS files:

```sh
umask 077
test -f .env || install -m 600 .env.example .env
chmod 600 .env
sudo install -m 644 deploy/*.service /etc/systemd/system/
sudo install -m 755 deploy/cs2-arb-run deploy/cs2-arb-update /usr/local/bin/
sudo systemctl daemon-reload
cs2-arb-update main
sudo systemctl enable cs2-arb-watch cs2-arb-mock@cs2 cs2-arb-mock@rust
```

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
read-only requests when `DMARKET_PUBLIC_KEY` and `DMARKET_SECRET_KEY` are set.
If either key is blank or missing, the jobs skip DMarket and continue with
the other venues. Once the keys are in `.env`, restart the jobs; no config
edit is needed:

```sh
sudo systemctl restart cs2-arb-mock@cs2 cs2-arb-mock@rust
```

To block new simulated orders in both jobs, create
`/home/ubuntu/cs2-arb-mock-data/KILL_SWITCH` (not a file in the checkout).
To resume, remove the file and restart both mock units to clear the latched switch.

## Read results

Replace `REPORT.md` with a filename from the listing.

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
