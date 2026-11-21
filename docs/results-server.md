# The results server

`brokkr-results-server` serves the bench store over HTTP, so runs from
several machines can live in one database on one host. The bench store is
the per-user store of script-bench runs (`brokkr man results
script-benches`). This page covers how brokkr uses a server, running it,
access, a deployment example, upgrades, deploying after an install, and
backups.

The server is a binary of its own, not a brokkr subcommand. `brokkr install`
installs it beside `brokkr`, and it is unused until you start it.

## Using a server

brokkr uses a server when your user-wide config names it (`brokkr man
config`, The user-wide brokkr.toml):

```toml
# ~/.config/brokkr/brokkr.toml
results_server = "https://bench.example.ts.net:8443"
```

Every `brokkr bench` on that machine then publishes its runs there, and
`brokkr results` reads script-bench runs from there, in every project.
brokkr no longer reads or writes the local `bench.db`; runs already in it
stay there. Without the key, nothing changes: brokkr keeps its own local
store.

A configured server that can't be reached fails the command after retrying
for a bounded time; there is no offline spool. `brokkr bench` checks that
the server answers, with this brokkr's protocol, before it builds anything,
so an unreachable server costs no measurement. A server lost during the run
fails the publication, and the error says whether the run may still have
been stored; `brokkr results UUID` then shows whether it was.

## Running

Start the server with a database path and a listen address:

```sh
brokkr-results-server serve --db /srv/brokkr/bench.db --listen 127.0.0.1:8650
```

It takes two flags:

- **`--db PATH`**: the SQLite file the server owns, an absolute path,
  created with its directory if missing. It must be on a local filesystem:
  SQLite in WAL mode doesn't work over a network mount, and the server
  refuses a file where WAL doesn't take. Give it a file of its own, not the
  local store of a brokkr on the same machine
  (`$XDG_DATA_HOME/brokkr/bench.db`), which a newer brokkr would migrate
  under the running server.
- **`--listen ADDR`**: the address and port to serve on. Nothing is
  compiled in: there is no default path, host or port.

At startup the server locks the file, migrates it to the current schema, and
refuses one written by a newer brokkr. A second server on the same file,
under any name, exits at once naming the lock. The server then logs one
line, `serving FILE on ADDR, protocol N`, and serves until stopped. It logs
failures on stderr, one line each; there is no access log.

The server exits `1` when it can't start (an unusable path, a held lock, a
newer or foreign database, an address it can't bind) or when its database
worker stops, so a service manager restarts it. It exits `2` for a usage
error.

## Access

The server has no authentication. Anyone who can reach its address can read
and add runs, so listen on loopback and put access control in front of it: a
TLS proxy reachable only from your own network or tailnet. The server speaks
plain HTTP/1.1.

`GET /bench` answers the server's identity, for a health check:

```text
$ curl -s http://127.0.0.1:8650/bench
{"build":"5f0c...","protocol":...,"server":"brokkr-results-server",
 "stamp":"sha 1f0c... clean y","version":"0.1.0"}
```

The example wraps the answer for width; the server sends one line. The
fields are these:

- **`build`**: a digest of the executable the process runs, read when it
  started; absent if the server couldn't read its own executable.
- **`stamp`**: the build stamp `brokkr install` wrote into that executable
  (`brokkr man run`, Build stamp); absent for a server not installed by
  `brokkr install`.

`brokkr --version` asks this route and prints the server's stamp and
protocol under its own. `GET /bench?probe=HEX` also echoes the probe, 1 to
64 lowercase hex characters, so a caller knows the answer was made for its
request. Every answer the server composes says `Cache-Control: no-store`;
the HTTP library's own refusals of malformed HTTP don't.

The protocol is versioned, and a client and server of different protocols
refuse each other: upgrade them together.

## Deployment example

One way to run the server is a systemd user service on a host that stays
up, published to a tailnet with Tailscale Serve. Adapt the paths, port and
names to your setup. This unit runs the server:

```ini
# ~/.config/systemd/user/brokkr-results-server.service
[Unit]
Description=brokkr results server

[Service]
Type=exec
ExecStart=%h/.cargo/bin/brokkr-results-server serve --db %h/.local/share/brokkr-server/bench.db --listen 127.0.0.1:8650
Restart=on-failure

[Install]
WantedBy=default.target
```

These commands enable it, keep it running after logout, and show its log:

```sh
systemctl --user daemon-reload
systemctl --user enable --now brokkr-results-server.service
sudo loginctl enable-linger "$(id -un)"
journalctl --user -u brokkr-results-server.service
```

Linger keeps your user's service manager, and so the server, running after
logout and across reboots. Then publish the loopback port to the tailnet
with HTTPS:

```sh
sudo tailscale serve --bg --https=8443 http://127.0.0.1:8650
```

Serve terminates TLS and forwards to the loopback port, exposing the server
within the tailnet only. Don't use Funnel, which would put an
unauthenticated server on the internet.

`Type=exec` makes a missing or unrunnable binary fail the start. The server
doesn't notify systemd of readiness, so systemd counts it started before it
has migrated its database and bound its port. A unit ordered after it may
start early, and `TimeoutStartSec` doesn't bound that startup.
`brokkr-results-server deploy` (see Deploying after an install) learns
readiness from the server itself instead.

## Upgrades

Install the new binary (`brokkr install` in a brokkr checkout) and restart
the service, by hand or with `brokkr-results-server deploy`. The server
migrates a new schema at startup. An older server can't open a database a
newer one migrated, so downgrading needs a backup taken before the upgrade.

## Deploying after an install

`brokkr-results-server deploy` restarts this host's server, if the host has
one, and confirms it now runs the binary deploy itself was started as. Run
it after every install on every machine, from a hook that installs brokkr
across several hosts. The host decides by its own state, and nothing names a
host, a port or a service manager.

A host serves when it has `~/.config/brokkr-results-server/deploy.toml`. The
path is fixed under the home directory, without `XDG_CONFIG_HOME`, so a hook
running with a stripped environment reads the same file a login shell does:

```toml
# ~/.config/brokkr-results-server/deploy.toml
url = "http://127.0.0.1:8650/bench"
restart = ["env", "XDG_RUNTIME_DIR=/run/user/1000", "systemctl", "--user",
           "restart", "brokkr-results-server.service"]
timeout_secs = 300
```

The declaration has three keys:

- **`url`**: the server's own endpoint on this host, normally its loopback
  listen address, not a proxy in front of it. No query or fragment.
- **`restart`**: the command that restarts the server, run directly (no
  shell) from the home directory with deploy's own environment. A hook that
  strips `XDG_RUNTIME_DIR`, which `systemctl --user` needs, gets it back
  here, as above, with your uid.
- **`timeout_secs`** (default 300): the deadline for the whole deploy,
  including reading its own executable, the restart command and every
  probe. It runs on the monotonic clock, so time asleep doesn't count.
  Deploy stops waiting for its work at most a second after the deadline, and
  starts no restart once it has passed. Writing its last line can still wait
  on a caller that doesn't read its output.

The restart command's output is deploy's own. A command that leaves a
process behind holding it, one that daemonizes without closing its output,
keeps the caller's pipe open after deploy exits; `systemctl` doesn't.

Deploy reads the digest of its own executable and runs `restart`. It then
probes `url` with a fresh random probe each time, without proxies or
redirects, until an answer made for that probe reports the same build. On a
host without the file, it prints that there is nothing to deploy and exits
0, touching nothing.

Exit status `0` means the restart command succeeded and, within the
deadline, the endpoint answered a fresh probe with this executable's digest.
It doesn't mean the store answers bench requests or that the server stays
up. After two deploys of different builds overlap, it doesn't say which one
is serving at the end.

Exit status `1` is everything else, with the reason:

- an unreadable or invalid declaration
- a restart command that fails or outlasts the deadline; deploy tries to
  kill it, and the command, or what it started, may still complete
- a deadline passed, with the last thing the endpoint said, such as another
  build's digest

A hook that retries failures retries the deploy.

A server already running this build counts as deployed, whatever the restart
command did. A restart command that restarts nothing is found out at the
next install that changes the executable, when the old build answers.

Removing the declaration withdraws nothing: deploy then leaves the host's
server as it is. To take a host out, stop and disable its unit and remove
the declaration.

## Backups

Take a consistent copy of the live database with SQLite's backup, never by
copying the file, whose WAL may hold committed runs:

```sh
sqlite3 ~/.local/share/brokkr-server/bench.db ".backup '/backups/bench.db'"
```

To restore, stop the service, put the backup at the configured path with no
`-wal` or `-shm` files beside it, and start the service again.
