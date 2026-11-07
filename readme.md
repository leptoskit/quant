# Quant

Quantify your life, and decide exactly which AI may read it.

Quant records what you do — what is on your screens, what you hear and say, which apps you use, and what your body is doing (heart rate, sleep, steps, posture, location, and any sensor you strap on later) — into an encrypted vault on your own machine. AI agents reach that vault through an MCP server, but only with a grant you approved, scoped like GitHub's fine-grained permissions, rate limited, redacted, and written to a tamper-evident audit log.

> **Status: early.** The vault, access control, daemon, CLI, MCP server, and macOS capture work and are tested end to end. The Android collector (built with [leptoskit](https://github.com/leptoskit/leptoskit)) comes next. Single user by design.

## Why

Questions like "how did I sleep after late coffee?", "when do I focus best?", or "what did we agree on in Tuesday's call?" are answerable from data you already produce. The data is also the most sensitive you have. Quant treats both facts as requirements:

- **Everything is captured, quietly.** Capture runs in a background daemon at low priority, skips frames that did not change, pauses when you are idle or the screen is locked, and never records password managers or private browsing windows.
- **Nothing is readable by default.** A new agent sees one tool that tells it how to ask you for access.
- **Access is specific.** `health:summary` lets an agent see your average heart rate per hour, never a raw sample, a screenshot, or a window title. `screen:read` gives on-screen text with secrets redacted, but no images. `audio:raw` is a deliberate choice.
- **Extraction is bounded.** Each grant has a lookback window, an expiry, and limits on requests per minute, records per hour, and bytes per day.
- **Granting needs you.** Approving an agent, purging data, or reading the audit log needs your passphrase. An agent with a shell on your machine cannot approve itself.

## How it fits together

```text
 macOS capture ──┐                        ┌── quant CLI (signed)
 Android app ────┤ signed ingest          ├── quant mcp ── Claude Code, other agents
 watch / vest ───┘        │               │      (signed, scoped, rate limited)
                          ▼               │
                ┌───────────────────────────────┐
                │ quantd                         │
                │  grants · limits · redaction   │
                │  audit chain                   │
                │  ┌──────────────────────────┐  │
                │  │ vault (SQLCipher + FTS5) │  │
                │  │ sealed media (XChaCha20) │  │
                │  └──────────────────────────┘  │
                └───────────────────────────────┘
                 private Unix socket (0600)
```

| Crate           | What it does                                                                                   |
| --------------- | ---------------------------------------------------------------------------------------------- |
| `quant_core`    | Records, sensor series, stream ids and families, the `Sink` capture sources write to.          |
| `quant_access`  | Scopes, grants, rate limits, Ed25519 request signing, redaction, the hash-chained audit log.  |
| `quant_crypto`  | The vault key, its passphrase (Argon2id) and keychain copies, media sealing.                  |
| `quant_store`   | The encrypted vault: SQLCipher records and series, full-text search, sealed media, retention. |
| `quant_api`     | The daemon's wire types.                                                                       |
| `quant_derive`  | Derived memory: posture spans from body-worn accelerometers, daily summaries.                 |
| `quant_capture_macos` | macOS capture: screen text (Vision OCR), focused apps and windows, input counts, microphone speech. |
| `quant_daemon`  | `quantd`: serves the API on a private socket and enforces every grant.                        |
| `quant_client`  | A signing client for the CLI, the MCP server, and devices.                                    |
| `quant_collector` | A device-side encrypted spool that queues captures offline and drains them to the vault.   |
| `quant_mcp`     | The MCP server agents talk to; its tool list follows the agent's grant.                        |
| `quant_cli`     | The `quant` command.                                                                           |

See [docs/architecture.md](docs/architecture.md) for the data model, the security model, and the roadmap.

## Quick start (macOS)

```sh
cargo install --path crates/quant_cli

quant init                 # choose a passphrase; the key also goes in your keychain
quant service install      # run the daemon at login

# Connect Claude Code. The first run asks the vault to pair and prints a code.
claude mcp add quant -- quant mcp --profile claude --scope '*:summary'
quant approve K7QD-M2XA --scope health:summary --scope apps:summary --lookback-hours 720
```

The daemon captures this Mac as soon as it starts. macOS asks once for **Screen Recording** (screenshots and window titles); grant it to `quant` in System Settings → Privacy & Security. Microphone capture is off until you turn it on:

```sh
quant status                       # what each source is doing, and why one is stopped
quant capture set --audio on       # also asks for Microphone access
quant capture set --exclude-app com.apple.Notes --ocr accurate
quant capture pause --minutes 30   # or `quant capture resume`
```

Password managers and private or incognito browser windows are never captured. Changing what is captured, or pausing it, needs your passphrase, so an agent cannot quietly turn capture off. Use `quant daemon --no-capture` to serve the vault without recording.

Pair a phone over the local network (TLS, certificate pinned by the link):

```sh
quant service install --tcp 0.0.0.0:7717
quant link                        # shows a QR code for the Quant app
```

Query from the terminal with your own, separately approved profile:

```sh
quant pair --scope '*:read'
quant approve <code>
quant overview --from yesterday
quant time apps.focus --by app --from today
quant series health.heart_rate --from -12h --bucket 1800
quant search "quarterly budget" --from -7d
quant audit                # who read what, and whether the log was tampered with
```

## Custom hardware

Anything that can print numbers can feed Quant. Register the stream, give the device its own write-only grant, and pipe samples in:

```sh
quant stream add sensor.vest.accel --channel x:m/s2 --channel y:m/s2 --channel z:m/s2 \
  --description "Chest vest accelerometer"
quant derive posture --stream sensor.vest.accel --placement torso   # read posture from it

quant --profile vest pair --kind device --scope sensor.vest:write
quant approve <code>
cat /dev/cu.usbmodem1101 | quant --profile vest ingest --stream sensor.vest.accel   # CSV: t,x,y,z
```

`quant ingest` reads CSV (`timestamp,value,…`, with an optional header naming the channels) or JSON lines, batches samples, and retries when the grant's rate limit is reached. Timestamps may be RFC 3339 or Unix seconds or milliseconds; an empty timestamp means now. With `--records`, each JSON line becomes an event record instead.

## Scopes

A scope is `<target>:<permission>`. Targets are `*`, a family (`health`), or a stream and its children (`sensor.vest`). Read permissions nest: `raw` includes `read`, which includes `summary`.

| Permission | Allows                                                                                       |
| ---------- | -------------------------------------------------------------------------------------------- |
| `summary`  | Aggregates: counts, durations by app or posture or sleep stage, min/mean/max per time bucket. |
| `read`     | Individual records and raw samples, with text sanitised and secrets redacted.                |
| `raw`      | Also screenshots and audio.                                                                  |
| `write`    | Sending data in (for capturing devices). Never implies reading.                              |

`audit:read` lets a client read its own audit entries.

| Family     | Streams                                                                                    |
| ---------- | ------------------------------------------------------------------------------------------ |
| `screen`   | `screen.frame`: on-screen text, app, window title, screenshot                               |
| `audio`    | `audio.transcript`: speech and its transcript; audio kept 24 hours by default               |
| `apps`     | `apps.focus`: which app and window had focus, and for how long                              |
| `input`    | `input.activity`: key, click, and scroll counts per minute (never content)                  |
| `health`   | `health.heart_rate`, `health.hrv`, `health.spo2`, `health.steps`, `health.sleep`, `health.workout` |
| `motion`   | `motion.accelerometer`, `motion.gyroscope`, `motion.orientation`, `motion.posture`, `motion.gait` |
| `location` | `location.fix`, `location.visit`                                                            |
| `device`   | `device.battery`, `device.state`                                                            |
| `comms`    | `comms.notification`                                                                        |
| `sensor`   | Anything from custom hardware: `sensor.<vendor>.<channel>`                                   |
| `memory`   | `memory.note`, `memory.episode`, `memory.daily`: what you or an agent wrote or derived      |

## Inspiration

- [kizuki](https://github.com/Illuminfti/kizuki) for its capture-to-memory ledger, agent grants, and redaction.
- [screenpipe](https://screenpi.pe) for continuous screen and audio capture exposed to agents.
- Backtrack for keeping the last day of audio around in case it matters.

## License

[Unlicense](license).
