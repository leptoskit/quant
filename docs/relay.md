# The Quant relay

A small server that lets phones, sensor bridges, and MCP servers reach
`quant daemon` from anywhere, while seeing only TLS bytes. It joins two
WebSockets — one the daemon keeps open, one a client opens — and copies
binary frames between them. TLS runs end to end, from the client to the
daemon, inside that pipe.

The protocol lives in the root workspace's [`quant_relay`](../crates/quant_relay)
crate, which this server, the daemon, and clients all build from; the routes
and limits here are `quant_relay::server` over this app's `leptoskit_db`
storage, so the relay the daemon's tests run in-process is the same one this
app serves.

## What it can and cannot see

The relay sees ciphertext and metadata, nothing else:

- the rendezvous id (`rid`), which is the first 16 bytes of SHA-256 of a
  public key the daemon derives from the vault key — never the vault key or
  anything derived from it beyond the public half;
- the public key itself, when the daemon registers, and when it was last
  seen;
- how many bytes flow, per rendezvous id, per day;
- the invite code a daemon presented, when the relay gates registration.

It never sees a request, a response, a pairing code, a certificate, or any
key. It stores no IP addresses, and it logs none at `info` level. A relay
that lies can drop or delay traffic, but it cannot read or alter it: TLS
fails on the first altered record, and a fake daemon fails the client's
pinned fingerprint. A relay cannot impersonate a daemon either: a client
reaches a `rid` only after that daemon proved ownership by signing the
relay's challenge with the key the `rid` hashes.

## Running it locally

```sh
cargo run -p relay        # serves on 127.0.0.1:3000, SQLite in ./relay.sqlite3
DATABASE_URL=sqlite::memory: cargo run -p relay   # or in memory
```

It applies its migrations on startup and answers `/healthz` and `/readyz`
(the readiness check passes once migrations have run). Put TLS in front of
it for anything off localhost — a reverse proxy that preserves the `Host`
header, since the handshake signatures cover it.

## Deploying it

The SDK and Docker build CLI pin the same latest verified Leptoskit `main`
commit as the desktop app. Update them together from the repository root with
`python3 scripts/update-leptoskit.py`, then run the affected package tests before merging.
Preserve the Dockerfile's Git revision install when regenerating deployment files.

The Docker, Compose, Fly, and systemd files in `deploy/relay` are what
`leptoskit generate deploy` writes; run the generator again after changing
the app and commit what it writes.

```sh
docker build -f deploy/relay/Dockerfile -t quant-relay .  # repository root
docker run --rm --read-only --tmpfs /tmp \
  -v relay-data:/data -p 127.0.0.1:3000:3000 quant-relay

docker compose -f deploy/relay/compose.yaml up --build                     # the same, with a volume
fly launch --config deploy/relay/fly.toml --copy-config --no-deploy
fly volumes create data --size 1 --config deploy/relay/fly.toml
fly deploy --config deploy/relay/fly.toml
```

The container listens on `0.0.0.0:3000` (its proxy should terminate TLS),
keeps its SQLite database on the `/data` volume, runs as an unprivileged
user, and writes nothing else to its own filesystem. Keep WebSocket
connections in mind when scaling: each relay instance holds its daemons'
sockets in memory, so a relay is one machine (a volume, one replica), which
is plenty for thousands of vaults.

## Configuration

| Variable | Default | Meaning |
| --- | --- | --- |
| `DATABASE_URL` | `sqlite://relay.sqlite3` | SQLite path, or a Postgres URL when built with `relay_db/postgres`. |
| `LEPTOS_SITE_ADDR` | `127.0.0.1:3000` | Where to listen; the container sets `0.0.0.0:3000`. |
| `RELAY_MAX_CONNECTIONS` | `16` | Concurrent client connections per rendezvous id. |
| `RELAY_BYTES_PER_DAY` | `2147483648` | Bytes per rendezvous id per UTC day, both directions. |
| `RELAY_MAX_FRAME` | `1048576` | Largest binary frame carried; larger frames end the connection. |
| `RELAY_IDLE_SECS` | `300` | Idle time before a spliced connection is closed. |
| `RELAY_ACCEPT_SECS` | `10` | How long a client's connect waits for the daemon to accept. |
| `RELAY_PENDING_CONNECTS` | `64` | Connects waiting for their daemon per rendezvous id. |

Byte counts are flushed to the database when each connection ends, so a
crash can lose the counts for the connections it dropped. Usage is enforced
per connection against the day's remaining budget regardless.

### Invites

By default any daemon that proves its key may register. Creating a single
invite turns gating on for the whole relay: from then on, a rendezvous id
not yet registered must present a valid, unused invite on its first listen.

```sh
cargo run -- invite add          # prints a random code
cargo run -- invite add my-code
cargo run -- invite list
```

## How a daemon uses it

On the owner's machine:

```sh
quant relay set wss://relay.example.com        # asks for the passphrase; audited
quant relay set wss://relay.example.com --invite my-code   # on gated relays
quant relay status
```

`quantd` notices the setting (restart it, or wait for its listener to
reconnect) and keeps a connection open to `wss://<relay>/v1/listen`,
reconnecting with backoff and pinging every 30 s; `quant daemon --no-relay`
holds off. `quant link` then prints a link carrying the relay and the
daemon's rendezvous id:

```
quant://link?addr=192.168.1.4:7717&fp=…&vault=…&relay=wss%3A%2F%2Frelay.example.com&rid=…
```

A client that scans it tries the direct address first with a two-second
connect timeout and falls back to the relay; either way it runs the same
TLS to the daemon with the pinned fingerprint and signs every request with
its own key. The desktop daemon treats relay traffic exactly like LAN
traffic: same certificate, same signature checks, same grants.
