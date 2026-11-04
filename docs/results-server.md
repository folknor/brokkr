# The results server

`brokkr-results-server` serves the bench store (the per-user store of
script-bench runs, `brokkr man results`, Script benches) over HTTP, so runs
from several machines can live in one database on one host. It is a binary of
its own, installed beside `brokkr` by `brokkr install` and unused until
started; it is not a brokkr subcommand.

brokkr itself does not talk to a results server yet: every brokkr reads and
writes its own local store. Running a server today gives a store that only
direct HTTP clients reach.

## Running it

```sh
brokkr-results-server --db /srv/brokkr/bench.db --listen 127.0.0.1:8650
```

- `--db PATH`: the SQLite file the server owns, an absolute path, created
  with its directory if missing. It must be on a local filesystem (SQLite in
  WAL mode does not work over a network mount; the server refuses a file
  where WAL does not take). Give it a file of its own: not the local store
  of a brokkr on the same machine (`$XDG_DATA_HOME/brokkr/bench.db`), which
  a newer brokkr would migrate under the running server.
- `--listen ADDR`: the address and port to serve on. Nothing is compiled in:
  there is no default path, host or port.

At startup the server locks the file (a second server on the same file,
under any name, exits at once naming the lock), migrates it to the current
schema, and refuses one written by a newer brokkr. It then logs one line,
`serving <file> on <addr>, protocol <n>`, and serves until stopped. Failures
are logged on stderr, one line each; there is no access log.

Exit status: `1` when it cannot start (an unusable path, a held lock, a
newer or foreign database, an address it cannot bind) or when its database
worker stops, so a service manager restarts it; `2` for a usage error.

## Who can reach it

The server has no authentication. Anyone who can reach its address can read
and add runs, so listen on loopback and put access control in front of it:
a TLS proxy reachable only from your own network or tailnet. The server
speaks plain HTTP/1.1.

`GET /bench` answers the server's identity, for a health check:

```sh
curl -s http://127.0.0.1:8650/bench
{"protocol":1,"server":"brokkr-results-server","version":"0.1.0"}
```

The protocol is versioned, and a client and server of different protocols
refuse each other: upgrade them together.

## Deployment example

One way to run it: a systemd user service on a host that stays up, published
to a tailnet with Tailscale Serve. Adapt the paths, port and names to your
setup.

```ini
# ~/.config/systemd/user/brokkr-results-server.service
[Unit]
Description=brokkr results server

[Service]
ExecStart=%h/.cargo/bin/brokkr-results-server --db %h/.local/share/brokkr-server/bench.db --listen 127.0.0.1:8650
Restart=on-failure

[Install]
WantedBy=default.target
```

```sh
systemctl --user daemon-reload
systemctl --user enable --now brokkr-results-server.service
sudo loginctl enable-linger "$(id -un)"
journalctl --user -u brokkr-results-server.service
```

Linger keeps the user's service manager, and so the server, running after
logout and across reboots. Then publish the loopback port to the tailnet
with HTTPS:

```sh
sudo tailscale serve --bg --https=8443 http://127.0.0.1:8650
```

Serve terminates TLS and forwards to the loopback port; it exposes the
server within the tailnet only. Do not use Funnel, which would put an
unauthenticated server on the internet.

## Upgrades

Install the new binary (`brokkr install` in a brokkr checkout) and restart
the service. A new schema is migrated at startup. A database migrated by a
newer server cannot be opened by an older one, so downgrading needs a backup
taken before the upgrade.

## Backups

Take a consistent copy of the live database with SQLite's backup, never by
copying the file, whose WAL may hold committed runs:

```sh
sqlite3 ~/.local/share/brokkr-server/bench.db ".backup '/backups/bench.db'"
```

To restore, stop the service, put the backup at the configured path with no
`-wal` or `-shm` files beside it, and start the service again.
