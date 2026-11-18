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
- **Media** is sealed with XChaCha20-Poly1305 under a separate subkey, with the content hash as associated data, so one blob cannot be swapped for another. Each stream sets how long its media is kept (screenshots 7 days, audio 24 hours by default); a sweep deletes expired files while keeping the records that pointed at them. A saved `audio.clip` clears its media's expiry outright, so a clip outlives the buffer chunks it was joined from.
- `secure_delete` is on, so purged rows are overwritten rather than left in free pages.

## Access

Nothing reads the vault directly except the daemon, `quantd`, which listens on a Unix socket readable only by the owner's account (inside the vault, or in a private `/tmp/quant-<uid>` directory when the vault path is too long for a socket).

**Network.** `quant daemon --tcp 0.0.0.0:7717` also serves the API over TLS for phones and sensor bridges. The daemon makes a self-signed certificate once and keeps it in the vault; `quant link` prints a `quant://link?addr=…&fp=…&vault=…` link and QR code, and devices pin that SHA-256 fingerprint instead of trusting any certificate authority. Requests over the network are signed exactly as on the socket.

**Relay.** Off the home network, clients reach the same daemon through a relay: `quant relay set <url>` (an owner action, audited, stored in the vault's meta) points the daemon at one, and `quantd` then keeps an outbound WebSocket open to it. The daemon derives an Ed25519 rendezvous key from the vault key and proves ownership of its rendezvous id (a hash of the public half) by signing the relay's challenge, so nobody else can take its place; `quant link` adds `relay=…&rid=…` to the link it prints. A client connects to the relay, the relay joins the two sockets, and TLS runs end to end from the client to the daemon inside that pipe: the same pinned fingerprint, the same signed requests, the same grants. On the daemon each accepted relay stream is fed into the same TLS acceptor and router as `--tcp`, so relay traffic is never a byte more trusted than LAN traffic. Clients try the link's direct address first (2 s connect timeout) and fall back to the relay. The relay (`crates/quant_relay_server`, a leptoskit backend with `leptoskit_db`) stores only the rid, the public key, created/last-seen times, the invite used, and daily byte counts; it enforces per-rid limits (connections, bytes per day, frame size, idle time, accept timeout, pending connects) and can gate first registration on single-use invites. See [hosting](hosting.md).

**Clients.** Every client (the CLI, an MCP server instance, a phone, a sensor bridge) generates its own Ed25519 key and asks to pair. Pairing creates a pending client with no access; at most 16 may wait at once. Every later request is signed over its method, path, timestamp, nonce, and body hash. The daemon refuses stale timestamps (±60 s), altered requests, and replayed nonces. The daemon stores only public keys, so a copy of the database cannot impersonate anyone.

**Agents.** An agent is a client that answers questions about the owner, so it has to say exactly what it is and why it is reading before it gets either access or data. Its pairing request carries an `AgentIdentity` — product, vendor, model, version, host, and an optional operator — which is cleaned and stored with the pending client, shown with the pairing request, and kept with the grant; no other kind of client may wear one, and an agent that pairs without one is refused and the attempt audited. The identity is the agent's own words about itself: the daemon checks that it is present and sane, not that it is true, so the owner still reads it before approving. Each request then states three things in its headers — `x-quant-purpose` (why the data is needed), `x-quant-question` (the question it serves, when one is open), and `x-quant-model` (the model actually running this session, when it differs from the one given at pairing). These are signed with the rest of the request in signing scheme **V2** (`QUANT-SIG-V2`, a `QUANT-SIG-V1` line plus the body hash plus the three values), so nothing in flight can strip or rewrite the reason for a read. Scheme V1 signatures still verify for clients that never send purposes — the CLI, devices, the app — but never for an agent. An agent read (`/v1/query/…`, `/v1/media`, `/v1/context`, `/v1/memory/profile`) without a purpose of 8 to 500 characters is refused as `purpose_required`, with a 400, and the refusal is audited; a question id it names must be its own and still open, or the request answers as an invalid request, never revealing whether the id exists.

**Questions.** Agents work question by question rather than page by page. `POST /v1/questions` opens one with the question text, the purpose, and the model, and returns an id; reads that serve it carry that id, and their totals (records and bytes served) accumulate on the question. `POST /v1/questions/{id}/answer` records the agent's answer with up to 50 citations, and only the client that opened the question may answer it; every cited record must exist and be readable by that grant at read level, or the whole answer fails as not found. Questions left unanswered for 24 hours are abandoned, and a grant's `questions_per_hour` cap (60 by default) keeps a chatty agent from filling the ledger. The owner reads the ledger in the app's **Questions** tab (each question with who asked, the purpose, the model, the answer, its citation count, and what was served under it; audit rows show the purpose too) and with `quant questions [--recent N] [--agent <client>]` and `quant questions show <id>`, which prints the question, the answer, its citations, and every audit entry that carried the question id — what was asked for, what was served, and what was withheld. An answer is text about the owner, so it is served only to the owner and to the client that wrote it, sanitised and redacted like every other served text.

**MCP.** The MCP server pairs as an agent on the MCP `initialize` message, not before: it fills its `AgentIdentity` from `QUANT_MCP_PRODUCT`, `QUANT_MCP_VENDOR`, `QUANT_MCP_MODEL`, `QUANT_MCP_VERSION`, and `QUANT_MCP_OPERATOR`, fills an empty product or version from `initialize`'s `clientInfo`, and keeps the client id it gets in the profile so the agent's identity stays stable across restarts, so the owner sees which host and model is really asking. Every data tool takes a required `purpose` and an optional `question_id`, `quant_begin_question` and `quant_answer` run the ledger, and the server's instructions tell the agent to begin a question, carry its id on every read that serves it, and record the answer with the record ids it used.

**Owner.** Approving or refusing pairings, revoking grants, purging, registering streams, and reading the full audit log need an owner session, opened with the passphrase or an owner device (below) and lasting 15 minutes. Wrong guesses double the wait before the next attempt, up to an hour. The CLI reads the passphrase from the terminal, never from an argument or environment variable.

**Owner devices.** The owner can also open the session with a hardware key on their Mac or phone: a non-exportable P-256 key in the Secure Enclave or Android Keystore that signs ECDSA P-256/SHA-256 only after Touch ID or a fingerprint check. Enrolment (a passphrase session, at most eight active devices, audited with a key fingerprint) stores only the public half in a new vault table, so the private key never leaves the device's secure hardware — not for the daemon, not for a copy of the vault, not for anyone. Unlocking then works everywhere the socket, LAN TLS, and relay reach: the daemon hands out a 60-second single-use challenge, and the device signs `"quant owner unlock v1\n" ‖ vault id ‖ "\n" ‖ challenge`, which binds the answer to this vault alone. A device session is a full owner session for everything except enrolment: nothing in the API changes the passphrase or exports the vault key, and enrolment is kept for passphrase sessions because a fingerprint proves the person is holding their hardware, while only the passphrase proves the person — otherwise a stolen unlocked phone, or malware on it with a 15-minute window, could enrol more keys under the owner's name. Revocation takes effect on the next attempt and fails exactly like a wrong signature, as do an unknown device, a spent or expired challenge, and a bad signature; every failure is audited without saying which check failed, and shares the passphrase unlock's doubling backoff, so alternating the two gains nothing. `quant owner devices add|list|revoke` manages the roster; the CLI never signs, because it has no hardware key.

**Grants.** A grant names scopes, a lookback window, an optional expiry, rate limits, and whether text is redacted. Each request is checked in this order: the grant is live, a request token is available, every stream touched is in scope at the needed level, and the time range is clipped to the lookback. Answers are trimmed to the remaining record and byte budget rather than refused, and say so. A request for a mix of permitted and forbidden streams returns the permitted ones and lists the rest as withheld. Media a grant may not see answers exactly like media that does not exist.

A grant may also **deny sensitivity classes** (`health`, `intimate`, `location`, `others`, `financial`). Every built-in stream carries the classes of its family (`health.*` → `health`, `location.*` → `location`, `audio.*` and `comms.*` → `others`; `memory.daily` and `memory.episode` → `health` and `location`, because their text quotes heart rate, sleep, and places), and claims carry their own. Any stream or claim with a denied class is treated exactly like an out-of-scope one everywhere — records, search, series, durations, overview, media, and context packets — and is listed as withheld where withheld streams are listed. `quant approve <code> --deny health,intimate` is how an owner keeps health data away from an agent.

**Text.** All text served to clients is sanitised: invisible and control characters are removed, and anything shaped like a markup tag is defanged, so a web page read off the screen cannot open or close a block in an agent's prompt. Unless the grant is unredacted, secrets are replaced with `[redacted:<kind>]`: private keys, JWTs, cloud and API keys, bearer tokens, URL credentials, seed phrases, `password=` style assignments, and Luhn-valid card numbers. String attributes such as window titles and URLs are cleaned the same way.

**Status.** Any approved client may ask `GET /v1/capture/status` which capture sources run, whether capture is paused and until when, and which macOS permission a stopped source needs. It answers nothing that was captured: no counts, no capture times, and no free-text details, which stay behind the owner's `/v1/owner/status`. This is what lets the menu bar show its status line without the passphrase.

**Audit.** Every client request, refusal, owner action, and pairing request is appended to a hash-chained log. Each entry records who acted, the request's shape (streams, range, search terms, limits) and how many records and bytes were served, never the data itself. For an agent it also records the purpose it gave, the model it ran, and the question the request served, all hashed into the chain like the rest, so editing a purpose breaks verification exactly as editing a count does. Removing or editing an entry breaks every hash after it; `quant audit` checks the chain, and `quant questions show <id>` reads the entries that carry one question's id.

## Capture

Capture sources write to a `Sink`. In the daemon the sink writes straight into the vault; on a phone it queues for signed upload with a device grant (`health:write`, `motion:write`, …), which never implies read access.

- **macOS** (`quant_capture_macos`, run inside the daemon): focus spans and window titles; a ScreenCaptureKit screenshot every 10 seconds, kept only when a perceptual hash says the screen changed, read with Vision OCR; key, click and scroll counts per minute from system counters (no event tap, never content); lock, display sleep, idle and system sleep transitions; when enabled, microphone speech segments found by a voice activity detector, with optional on-device Whisper transcription; and **Rewind**, a rolling buffer of the last day of microphone and system audio as 60-second AAC `audio.buffer` chunks (`audio.clip` is a saved moment joined from them). Password managers and private browser windows are excluded from frames, and a frame is skipped when one has focus. Everything but Rewind pauses while the machine is idle or locked — Rewind keeps recording until the owner pauses capture, because its whole purpose is to hold what already happened. The settings live in the vault (`capture.config`); changing them or pausing capture is an owner action and is audited.
- **Desktop app** (`platforms/macos`, built with leptoskit): a menu-bar remote control for the daemon, which still does all the capturing. It pairs as its own `device` client asking for `audio:write audio:read audio:raw`, shows the status line, saves the last minutes of the rolling audio buffer, pauses and resumes capture, and opens a window to approve agents, revoke grants, and read the audit log. Owner actions take the passphrase once per 15-minute session; the app keeps only the session token, in memory.
- **Android** (development build): the `android` target of the Leptoskit app collects location, app usage, and battery and thermal state behind a foreground service, and uploads them with write-only scopes ([below](#phone-location-app-usage-and-power), and [the phone guide](phone.md)). Health Connect data (heart rate, HRV, sleep, steps), motion and posture, notifications, and microphone segments are not collected by the app yet. It has not been measured on a device or released.
- **Wearables**: Bluetooth LE bridges and custom hardware send series batches under `sensor.*`.
- **Imports** (`quant_import`, run from the CLI): history the owner already has in other tools, so years before Quant existed still answer "where does my time go".

### Importing history

`quant import screenpipe [--db ~/.screenpipe/db.sqlite]` and `quant import activitywatch [--url http://localhost:5600|--file export.json]` pull existing history into the vault, once or with `--follow [--every 5m]` to keep pulling as new data arrives. `--since`/`--until` take the CLI's usual times (`-2h`, `yesterday`, RFC 3339), and `--dry-run` prints the records per stream and the span they cover without writing anything.

- **Screenpipe** is a local SQLite database, opened read-only (and immutable when its writer holds it locked). Its schema changes between versions, so tables and columns are detected at run time rather than assumed. Consecutive frames with the same app and window become one `apps.focus` span; a gap of more than twice the median frame interval, capped at 60 s, ends a span. Each frame's OCR text becomes a `screen.frame` record — the stream native capture already uses for OCR — with identical consecutive text deduplicated. Audio transcriptions become `audio.transcript` records.
- **ActivityWatch** is read from the aw-server REST API or an exported JSON file. `aw-watcher-window` events become `apps.focus` spans, `aw-watcher-afk` events become `device.state` idle and active spans, and `aw-watcher-web` events become `apps.focus` spans carrying the page URL. When a host also has a web watcher, the window watcher's events for that browser are skipped so focus time is not counted twice. Events from another host keep that hostname as their device label.

Everything goes through the normal signed ingest path: the importer is an owner-run client that pairs like a device and needs write scopes for the families it touches (`apps:write`, `screen:write`, `audio:write` for Screenpipe; `apps:write`, `device:write` for ActivityWatch). It never opens the vault database directly. Every record's id is a UUIDv7 derived deterministically from the source, the source event's id, and the event time — so re-running an import, with or without the per-source watermark that `--follow` keeps, stores each record once and never duplicates anything. Imported records carry `source = "screenpipe"` or `"activitywatch"`, their window titles go through the same sanitise path as captured text, and the same password-manager and private-window exclusions apply.

### Phone audio pipeline

`quant_collector::Spool::ambient_audio` accepts finalized AAC `.m4a` segments and
recorder measurements. The native recorder's default boundary is five minutes;
interruptions and stopping can produce shorter segments. A segment with zero
active milliseconds is discarded before anything is written to the encrypted
spool or uploaded. Activity means sound above the configured level, not speech.
Even a brief detected sound is retained. Stable ids derived from the device,
capture time, duration and content make crash recovery idempotent. The native
adapter must remove the plaintext file only after the spool write succeeds (or
returns that the segment is silent).

The signed uploader delivers these as `audio.ambient`. Inserting a segment and
its transcription job is one database transaction. A single low-priority daemon
worker transcribes pending segments on device, using the existing configured
Whisper model or Apple's on-device recognizer. Failures retry with exponential
backoff, from one minute to one hour, without blocking other segments. Jobs and
retry times survive restarts; offline uploads are processed even when their
capture time precedes already completed work.

Completion atomically stores an `audio.transcript` linked by `chunk_id`, marks
the source's `transcript_status`, and removes the job. Transcripts retain the
phone's device and capture span, enter full-text search immediately, and are
picked up by the existing embedding backfill when configured. They carry the
`others` sensitivity class, just like other microphone transcripts. Deleting a
source cancels its pending job: recognition already running cannot recreate
purged data. Empty recognition results are marked `no speech found` without a
search record.

Ambient raw audio defaults to seven days of retention through the stream spec's
`media_retention_hours`; the owner can override that spec. Transcript records
have no media attachment or automatic expiry, so deleting expired audio does
not delete searchable text. If recognition remains unavailable until raw audio
expires, the segment is marked `media expired` and cannot be transcribed. The
phone app does not record audio: its service and permission screen still need
to connect a native recorder to this pipeline; adding the pipeline alone does
not start recording.

### Phone resource budgets

The collector keeps status checks independent of backlog size: `pending()` asks
SQLite for counts and never decodes queued records or sensor payloads. New phone
records are capped at 64 KiB of encoded JSON, counted without creating a second
JSON buffer. Sensor producers must flush before 16,384 numeric values or offsets
(32 channels at most); media items are capped at 16 MiB. Normal five-minute
16 kHz / 24 kbps AAC segments are about 0.9 MB. Larger captures must be split at
the source, rather than silently dropped.

Uploads load at most 32 records or 8 sensor batches at once, with payload-size
checks before decoding even old spool entries. Only one media item is decrypted
and uploaded at a time; bounded reads reject oversized files before allocating
for their contents. These are allocation limits for individual operations, not
a measured whole-app RSS guarantee.

A default `Uploader::drain()` burst acknowledges at most 128 records, 32 sensor
batches and roughly 4 MiB of audio, with a ten-second deadline including network
waits. One larger media item may advance on its own so the byte budget cannot
starve a valid capture. Record acknowledgement is committed before another
media item is sent. Unacknowledged captures remain encrypted locally on timeout;
the cancelled HTTP connection driver is aborted so it cannot retain its socket
and buffers across retries. `drain_with_budget` can reduce these limits, including
a zero-duration pause while the phone is too hot. It cannot raise the ceilings.

The phone's service honors `Delivered::yielded`: it sends one budgeted burst,
then waits at least a minute before the next, however often it is woken, so it
never drains in a tight loop. Normal uploads can follow the
five-minute recorder boundary or a connectivity event. Motion samples should
be batched; use low-rate orientation or OS activity APIs, and increase sampling
only for a feature that needs it. The OS thermal and low-power signals still
come from the battery and thermal plugins through `PowerMonitor::budget()`
(see [Phone location, app usage and power](#phone-location-app-usage-and-power)).
Transcription and embeddings run on the owner's daemon, never in the phone
collector.

Physical-device profiling is still required for sustained battery, thermal and
RSS measurements; simulator tests cannot establish that a phone will stay cool.

### Phone location, app usage and power

The phone collector turns what four Leptoskit plugins report into vault
streams. The mobile host owns the plugins and the OS lifecycle; it hands each
reading to the `Spool`, which converts, validates and queues it for the signed
uploader. Every record id is derived from the device and the reading, and
series batches are stored by content hash, so a reading handed over twice
(after a crash, or an overlapping query) is stored once.

| Plugin | Spool method | Stream | Phone grant |
| --- | --- | --- | --- |
| `leptoskit_plugin_location` `next_updates` batches (continuous updates, iOS visits and significant changes) | `location_updates` | `location.fix` series: `lat`, `lon`, `accuracy`, and `altitude` when known | `location:write` |
| `leptoskit_plugin_app_usage` `sessions` | `app_sessions` | `apps.focus` spans with `app` (the launcher label), `bundle_id` (the package) and `source = "phone"` | `apps:write` |
| `leptoskit_plugin_app_usage` `events` | `usage_events` | `device.state`: `display_on`, `display_off`, `locked`, `unlocked`, `startup`, `shutdown` | `device:write` |
| `leptoskit_plugin_battery` and `leptoskit_plugin_thermal` snapshots | `PowerMonitor::observe` | `device.battery` level rows, and a `device.power` record whenever charging state, low-power mode or thermal state changes | `device:write` |

- **Location.** Fixes the platform marks as simulated (mock locations) and
  fixes with impossible coordinates or times are counted and dropped. A run
  of fixes without altitude is sent without that column rather than with a
  placeholder. Fixes the plugin's bounded native buffer discarded are shown
  on the `location` source status. `location.fix` carries the `location`
  sensitivity class, so a grant made with `--deny location` never sees it,
  in records, series, search, overview or context packets.
- **App usage.** Sessions are clipped to the queried range, so the host
  queries contiguous ranges from a watermark. On iOS the plugin reports only
  lock and unlock while the app runs, so iOS phones contribute
  `device.state` records and no `apps.focus` spans. App foreground events
  are not stored separately: sessions already cover them.
- **Power.** `device.battery` keeps one row per level change, in percent.
  `device.power` records say what changed in words ("Phone battery 80%,
  charging, low power mode on, thermal state fair.") and carry `charging`,
  `low_power`, `thermal`, `android_thermal` (Android's raw status) and
  `level`; summaries may group by `charging` and `thermal`. The thermal
  plugin needs Android 10, so older phones report power without a thermal
  state. `PowerMonitor::budget()` (or `UploadBudget::for_power`) pauses
  uploads while the phone is in a serious or critical thermal state, and
  cuts a burst to a quarter of the default in a fair state, in low-power
  mode, or under 20% charge on battery.

Pair a phone with `quant approve <code> --scope location:write --scope
apps:write --scope device:write` (plus `health:write`, `motion:write` and
`audio:write` for what else it captures). Write scopes never imply read
access.

The Android app starts these plugins. Its screen (`quant_app`'s `phone`
module) asks for location, background location, notifications and Usage
Access, starts the background service, and keeps `next_updates` and
`wait_for_change` waiting. A native service cannot call a Kotlin plugin, so
the screen hands every reply, undecoded, to the `quant-phone` service
(`quant_phone`), which owns the spool, the phone's identity and the uploads.
[Quant on a phone](phone.md) describes the pieces and their limits. The
plugins' Kotlin hosts have not been measured on a physical device, and there
is no iOS host.

### Rewind

The rolling buffer is a source like the others, designed around one number: the CPU it may spend while recording. Everything runs at utility priority on background threads. Each track (microphone, and system audio through a ScreenCaptureKit `SCStream` that excludes this process's own output) is downmixed, resampled to 16 kHz, measured by the same energy VAD the speech path uses, and encoded as mono 24 kbps AAC into 60-second `audio.buffer` chunks; a chunk's record carries `source`, `device`, `speech_ms`, `peak_dbfs` and its size. When both the `audio` source and `rewind` want the microphone they share one cpal stream rather than opening the device twice.

Saving a moment (`POST /v1/audio/save`, scope `audio:write`, or the owner route) asks the source to close the chunk still being recorded when the range reaches the present, joins the overlapping chunks into one `.m4a` — one decode-and-re-encode pass, decoded a chunk at a time — and stores it as an `audio.clip` whose media has no expiry. The response carries ids, spans, and byte counts, never audio, which is why write scope is enough. Only then is anything transcribed: once, on a low-priority thread, with whisper.cpp (when built with the feature and a model is set) or Apple's on-device `SFSpeechRecognizer`; the outcome lands in the clip's `transcript_status`.

The buffer's retention follows `rewind.hours` from the capture settings: the daemon keeps the `audio.buffer` stream spec's `media_retention_hours` in sync, so the sweep that deletes yesterday's chunks obeys the setting the owner chose.

## Derived memory

The daemon recomputes derived records every ten minutes (`quant_derive`). Derived records come from the nil device, carry a `derived_from` attribute, and have ids computed from what they summarise, so recomputing replaces rather than duplicates them.

- **Posture.** Each configured accelerometer is read in 5-second windows. The mean vector is gravity, so its angle to the sensor's "up" axis is the tilt; the RMS spread around it is movement. Movement above 0.8 m/s² is walking and above 3.5 m/s² running. Otherwise a torso sensor (chest strap, vest) reads upright under 35°, reclined under 60°, and lying beyond; a thigh sensor reads standing or sitting-or-lying; a loose sensor (a phone) only reads still. A torso and a thigh sensor together tell sitting from standing. Windows merge into `motion.posture` spans, and a blip shorter than 20 seconds between two spans of the same posture is absorbed.
- **Gait.** Walking bouts of 30 seconds or more become `motion.gait` records, one per bout. Peak detection on the acceleration magnitude with gravity filtered out sets the cadence — a thigh sensor peaks once per stride, so each of its peaks is two steps, while a torso or loose sensor peaks once per step — and autocorrelation at the dominant step lag says how regular the stride was. When cadence falls more than 10% below the day's earlier bouts, the bout where it dropped says so in its text and is marked `slowed`.
- **Daily summaries.** One `memory.daily` record per local day: screen time by app, steps, heart rate range with a resting estimate (the mean of the lowest tenth of minute averages), sleep by stage for the night ending that day (18:00 to 18:00), posture totals, and minutes of speech, with a one-paragraph text that search and agents can read.
- **Hourly episodes.** One `memory.episode` record per hour where anything happened: the top apps by focus time, the place, posture minutes, heart rate range and average, steps, and minutes of speech, plus a one-paragraph text an agent can read. Empty hours produce nothing.
- **Weekly reviews.** One `memory.weekly` record per local week (weeks start Monday, in the owner's time zone like the daily summaries), with the same Health and Location classes: average sleep and its change from the previous week, resting heart rate, steps, top apps, focus hours (focus spans of 25+ minutes), and plain correlations computed from the week and the four before it — sleep on nights with screen use past 23:00 versus other nights, resting heart rate versus the previous day's steps, and focus minutes by hour of day (the best two hours). Each finding states its numbers and how many days it rests on, and stays silent below five data points; the record carries the same numbers as structured data.

Purges take derived records with their sources: deleting records or series rows also deletes `memory.daily`, `memory.episode`, `memory.weekly`, `motion.posture`, and `motion.gait` records whose span overlaps the purged range, and any derived record whose `derived_from` names a purged stream or family; the matching derivation watermarks move back to the purge's start so the next run recomputes from what remains. Claims and embeddings follow through their cascades.

## Memory about the owner

Claims are what Quant believes about its owner — preferences, values, routines, facts, relationships, goals, and commitments — each with evidence pointing at the records it came from, so a claim is always traceable back to the life behind it.

- **Claims.** A claim is a natural-language statement (up to 500 characters) with a kind, an optional topic key such as `coffee.timing`, a confidence, sensitivity classes, and a list of evidence record ids. Every claim has an **authority**: `owner` (stated or corrected directly) outranks `owner_authored` (written by the owner), which outranks `observed` (derived by Quant), which outranks `inferred` (proposed by an agent and approved). A claim's **status** moves active → superseded by a newer owner statement on the same subject and topic (the owner's own earlier statements included), retracted by the owner, or — for proposals — held → approved or rejected.
- **Storage.** Claims live in their own STRICT tables; statements are indexed by FTS5 through triggers exactly like record text, and evidence rows cascade with their records. When a purge deletes records, observed and inferred claims whose evidence went with them are deleted too; claims the owner made are never deleted by a purge.
- **Serving.** Claims are addressed as the built-in stream `memory.profile`, so existing scopes apply. `memory.profile:summary` serves only active owner and owner-authored claims — "who I am, in my words" — with no evidence ids and no proposer. `memory.profile:read` and `:raw` add active inferred claims, confidence, and evidence ids, but an evidence id the caller could not read at read level is dropped, never revealed. `memory.profile:write` lets an agent propose claims: each proposal is stored held, with the proposer recorded, and every cited record must exist and be readable by that grant at read level — otherwise the whole request fails as not-found, never saying which. Held proposals are served to no one but the owner, and proposals are capped at 20 per grant per hour. Statements and topics go through the same sanitise-and-redact path as every other served text, claims with a denied class are never served, and every claim request is audited with its shape and counts.
- **The owner's side.** `quant tell "<statement>" --kind preference --topic coffee.timing` states a claim and supersedes anything lower-authority on the same subject and topic; `quant claims list|approve|reject|retract|correct` runs the review queue, and `quant profile` prints the owner's own words first, then the rest by confidence. All of it is owner-session work, so an agent with a shell cannot rewrite the profile.
- **Context packets.** `POST /v1/context` assembles a budgeted packet for one task (default 1,500 tokens, clamped 200–8,000, tokens estimated as characters ÷ 4): profile claims matching the task's terms first, then the latest `memory.daily` and the last 24 hours of `memory.episode` texts, then records matching the task through the ordinary search path. Sections are filled in that order until the budget is spent, each only where the grant allows it, with the same access checks, deny classes, redaction, lookback, and rate budgets as the underlying endpoints; the answer says which sections were cut for budget or withheld. The MCP server exposes this as `quant_context`, alongside `quant_profile` and `quant_propose` and a `quant://profile` resource, offered only to grants that can use them.

## Learning on the device

Everything learning-related is off until the owner configures it, and everything it calls is reachable only on this machine's own loopback. The settings live in the vault under `learn.config` (owner routes, audited, like `capture.config`): `embed {endpoint, model, dims}` for an OpenAI-compatible `/v1/embeddings` server (Ollama or a llama.cpp server, say), `extract {endpoint, model, calls_per_day, claims_per_run}` for an OpenAI-compatible `/v1/chat/completions` server, and the streams to learn from (by default records with text in `screen.*`, `audio.*` transcripts, and `memory.*` except `memory.profile`). Endpoints must be literal loopback addresses — `127.0.0.1` or `::1`, any port, plain http only — and hostnames are refused, so a DNS change or a proxy can never redirect vault text off the machine. `quant learn show|set …|off` configures it.

### Similarity search

Records and claims can carry embedding vectors (`embeddings` and `claim_embeddings` tables, cascading with their rows), stored per embedding **space**: a short hash of the model, its dimensions, and the chunking version, so switching models re-embeds into a fresh space instead of mixing vectors. Vectors are little-endian f32, L2-normalised, and are never served to clients. A backfill job on the derivation schedule embeds at most 200 un-embedded texts per run, oldest first; texts longer than the model's window are truncated (the chunking version records that policy), and an unavailable endpoint just stops the run, which resumes next time.

`POST /v1/query/search` and `quant_search` gain a `mode`: `keyword`, `similar`, or `hybrid` — hybrid by default when embeddings exist for the current space, keyword otherwise. Similarity search first narrows candidates in SQL by the caller's selector and time range (exactly the streams search may already see, denied classes included in that narrowing), then scores by cosine in Rust. Hybrid fuses the keyword and vector rankings with reciprocal rank fusion (k = 60). Results travel the existing path — redaction, budgets, audit — and context packets use hybrid for their records and claims sections when a space exists.

### Extraction, with grounding rules

After the day's `memory.daily` exists, a daily job sends the day's texts — the daily summary, its episodes, notes, and saved audio transcripts — to the extract model in small batches, asking for claims as strict JSON (`kind, subject, topic, statement, confidence, classes, evidence: [{record, quote}]`). Before anything is stored, deterministically:

- **Grounding.** Every quote must appear in its cited record's text after NFKC normalisation, case folding, and whitespace collapsing, as whole words, and every evidence id must be a record in the batch. A claim with any ungrounded quote is dropped whole.
- **No instructions.** A claim whose statement or quotes look like instructions to an AI — imperatives addressed to an assistant, "ignore previous", "system prompt", "you must", role markers, tool or markup syntax — is dropped, so captured text cannot plant orders in the owner's memory.
- **Held when sensitive.** Claims about identity, credentials, access, grants, money movement, or the owner's relationships to named people are always stored Held for the owner to approve.
- **Observed otherwise.** The rest stores with authority Observed: Active only when grounded in at least two distinct records from different days or different source streams, otherwise Held. An active claim with the same subject and topic gains the new evidence instead of a duplicate (corroboration, confidence rising by a bounded step); owner claims are never touched.
- **Budgets.** The settings' calls-per-day and claims-per-run are hard caps. Each call reports ok, empty, or unavailable; only ok and empty advance the day's watermark, so an outage never looks like "nothing was learned". Prompts and model output are never served to agents, and stored claims go through the usual sanitise-and-redact path. Every extraction run writes one audit entry, counts only.

## Roadmap

1. **The relay**, so phones, sensors, and agents reach the vault from anywhere while the relay sees only TLS ([hosting](hosting.md)).
2. **One leptoskit app everywhere:** take the Android collector from a development build to a measured release, add health, motion and audio to it on leptoskit's sensor and health plugins, and give iOS a host; Wear OS and watchOS for heart rate from the wrist.
3. **Desktop:** owner actions unlocked with Touch ID, and opening at login.
4. iOS collector (HealthKit, background modes).
5. Hosted vaults in attested enclaves ([hosting](hosting.md)).
