# Architecture

## Data model

Everything Quant knows arrives as one of two shapes, both tagged with a stream id such as `health.heart_rate` and the device that captured it.

- **Records** (`quant_core::Record`) are discrete things that happened: a screen frame, a transcript segment, a focus span, a sleep stage, a note. A record has a start, an optional end, searchable text, structured attributes, and optionally media (a screenshot or audio clip stored separately and sealed). Records carry a UUIDv7 id assigned at capture, so resending one after a failed sync stores it once.
- **Series batches** (`quant_core::SeriesBatch`) are runs of numeric samples: heart rate at 1 Hz, acceleration at 50 Hz, position fixes. A batch names its channels and holds row offsets in milliseconds plus row-major values. Sensors send one batch every few seconds rather than one record per sample. Batches are stored as `f32` (`f64` for location), with per-channel statistics alongside, so most summaries are answered without decoding samples.

Every stream belongs to one of a fixed set of families (`screen`, `audio`, `apps`, `input`, `health`, `motion`, `location`, `device`, `comms`, `sensor`, `memory`). Grants are written against families and streams, so the families never grow; new hardware adds streams under `sensor`, and the owner can register a `StreamSpec` describing its channels and units.

Stream specs also name `summary_keys`: the attributes that summary-level access may group by. Focus spans may be grouped by `app` but not by window `title`; posture spans by `posture`.

## Storage

A vault is a directory holding `vault.json` (the manifest), `quant.db` (SQLCipher), and `media/` (sealed blobs named by their BLAKE3 hash).

- The **vault key** is 256 random bits. It is stored only wrapped: under the owner's passphrase with Argon2id (64 MiB, 3 passes) in the manifest, and in the OS keychain so the daemon can start at login without asking.
- **SQLCipher** encrypts every database page with a subkey derived from the vault key. Records are indexed by stream and time, and their text by FTS5 (`unicode61`, diacritics folded). Search terms are quoted before they reach FTS5, so user input cannot inject query syntax.
- **Media** is sealed with XChaCha20-Poly1305 under a separate subkey, with the content hash as associated data, so one blob cannot be swapped for another. Each stream sets how long its media is kept (screenshots 7 days, audio 24 hours by default); a sweep deletes expired files while keeping the records that pointed at them.
- `secure_delete` is on, so purged rows are overwritten rather than left in free pages.

## Access

Nothing reads the vault directly except the daemon, `quantd`, which listens on a Unix socket readable only by the owner's account (inside the vault, or in a private `/tmp/quant-<uid>` directory when the vault path is too long for a socket).

**Network.** `quant daemon --tcp 0.0.0.0:7717` also serves the API over TLS for phones and sensor bridges. The daemon makes a self-signed certificate once and keeps it in the vault; `quant link` prints a `quant://link?addr=…&fp=…&vault=…` link and QR code, and devices pin that SHA-256 fingerprint instead of trusting any certificate authority. Requests over the network are signed exactly as on the socket.

**Clients.** Every client (the CLI, an MCP server instance, a phone, a sensor bridge) generates its own Ed25519 key and asks to pair. Pairing creates a pending client with no access; at most 16 may wait at once. Every later request is signed over its method, path, timestamp, nonce, and body hash. The daemon refuses stale timestamps (±60 s), altered requests, and replayed nonces. The daemon stores only public keys, so a copy of the database cannot impersonate anyone.

**Owner.** Approving or refusing pairings, revoking grants, purging, registering streams, and reading the full audit log need an owner session, opened with the passphrase and lasting 15 minutes. Wrong guesses double the wait before the next attempt, up to an hour. The CLI reads the passphrase from the terminal, never from an argument or environment variable.

**Grants.** A grant names scopes, a lookback window, an optional expiry, rate limits, and whether text is redacted. Each request is checked in this order: the grant is live, a request token is available, every stream touched is in scope at the needed level, and the time range is clipped to the lookback. Answers are trimmed to the remaining record and byte budget rather than refused, and say so. A request for a mix of permitted and forbidden streams returns the permitted ones and lists the rest as withheld. Media a grant may not see answers exactly like media that does not exist.

**Text.** All text served to clients is sanitised: invisible and control characters are removed, and anything shaped like a markup tag is defanged, so a web page read off the screen cannot open or close a block in an agent's prompt. Unless the grant is unredacted, secrets are replaced with `[redacted:<kind>]`: private keys, JWTs, cloud and API keys, bearer tokens, URL credentials, seed phrases, `password=` style assignments, and Luhn-valid card numbers. String attributes such as window titles and URLs are cleaned the same way.

**Audit.** Every client request, refusal, owner action, and pairing request is appended to a hash-chained log. Each entry records who acted, the request's shape (streams, range, search terms, limits) and how many records and bytes were served, never the data itself. Removing or editing an entry breaks every hash after it; `quant audit` checks the chain.

## Capture

Capture sources write to a `Sink`. In the daemon the sink writes straight into the vault; on a phone it queues for signed upload with a device grant (`health:write`, `motion:write`, …), which never implies read access.

- **macOS** (`quant_capture_macos`, run inside the daemon): focus spans and window titles; a ScreenCaptureKit screenshot every 10 seconds, kept only when a perceptual hash says the screen changed, read with Vision OCR; key, click and scroll counts per minute from system counters (no event tap, never content); lock, display sleep, idle and system sleep transitions; and, when enabled, microphone speech segments found by a voice activity detector, with optional on-device Whisper transcription. Password managers and private browser windows are excluded from frames, and a frame is skipped when one has focus. Everything pauses while the machine is idle or locked. The settings live in the vault (`capture.config`); changing them or pausing capture is an owner action and is audited.
- **Android** (next): a foreground service collecting Health Connect data (heart rate, HRV, sleep, steps), motion and posture, location visits, notifications, and microphone segments, built on leptoskit. This needs upstream leptoskit support for foreground services and new plugins for Health Connect and audio capture.
- **Wearables**: Bluetooth LE bridges and custom hardware send series batches under `sensor.*`.

## Derived memory

The daemon recomputes derived records every ten minutes (`quant_derive`). Derived records come from the nil device, carry a `derived_from` attribute, and have ids computed from what they summarise, so recomputing replaces rather than duplicates them.

- **Posture.** Each configured accelerometer is read in 5-second windows. The mean vector is gravity, so its angle to the sensor's "up" axis is the tilt; the RMS spread around it is movement. Movement above 0.8 m/s² is walking and above 3.5 m/s² running. Otherwise a torso sensor (chest strap, vest) reads upright under 35°, reclined under 60°, and lying beyond; a thigh sensor reads standing or sitting-or-lying; a loose sensor (a phone) only reads still. A torso and a thigh sensor together tell sitting from standing. Windows merge into `motion.posture` spans, and a blip shorter than 20 seconds between two spans of the same posture is absorbed.
- **Gait.** Walking bouts of 30 seconds or more become `motion.gait` records, one per bout. Peak detection on the acceleration magnitude with gravity filtered out sets the cadence — a thigh sensor peaks once per stride, so each of its peaks is two steps, while a torso or loose sensor peaks once per step — and autocorrelation at the dominant step lag says how regular the stride was. When cadence falls more than 10% below the day's earlier bouts, the bout where it dropped says so in its text and is marked `slowed`.
- **Daily summaries.** One `memory.daily` record per local day: screen time by app, steps, heart rate range with a resting estimate (the mean of the lowest tenth of minute averages), sleep by stage for the night ending that day (18:00 to 18:00), posture totals, and minutes of speech, with a one-paragraph text that search and agents can read.
- **Hourly episodes.** One `memory.episode` record per hour where anything happened: the top apps by focus time, the place, posture minutes, heart rate range and average, steps, and minutes of speech, plus a one-paragraph text an agent can read. Empty hours produce nothing.

## Roadmap

1. macOS capture in the daemon, with a menu bar app (leptoskit) for status, pausing, and approving agents.
2. Android collector app on leptoskit; upstream foreground service, Health Connect, and audio plugins.
3. Phone-to-desktop sync away from home, through a relay that only sees TLS traffic.
4. More derived memory: purging derived records with their sources.
5. iOS collector (HealthKit, background modes).
