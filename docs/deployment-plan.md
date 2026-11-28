# Quant deployment plan

This is the runbook for releasing Quant. Follow the steps for the target being
deployed, keep the release evidence, and stop that target's rollout if its
verification fails. Update this document when deployment behavior changes.

All commands run from the repository root unless stated otherwise. Quant has
one `leptoskit.toml`, one Cargo workspace and one lockfile. Separate hosting
resources and release artifacts remain destinations of this one application.

## 1. Release scope and order

| Destination | Artifact and location | First release path |
| --- | --- | --- |
| `landing` | SSR server and browser bundle; `deploy/landing/` | Staging, then public website. No vault credentials or database. |
| `relay` | Relay server image; `deploy/relay/` | Staging, then an invite-gated pilot on one persistent instance. |
| `desktop` | Signed macOS app plus the `quant` CLI/daemon | Local install, owner pilot, then signed public distribution after packaging gates pass. |
| `desktop-preview` | Local UI with explicitly labelled, synthetic sample data | Design review and screenshots only; never distribute as the product. |
| `web` | Product browser frontend | Blocked by `targets.web.unavailable`; authenticated transport is unfinished. |
| `android` | Debug APK of the phone collector (incl. Health Connect, BLE, watch sync) | Development builds on the owner's own phone only ([guide](phone.md)). No release until the Android gates in section 10 pass. |
| `ios` | Unsigned device build of the phone collector (incl. HealthKit, BLE, WatchConnectivity) | Development builds in the simulator or on the owner's own iPhone only ([guide](phone.md)). No release until the iOS gates in section 10 pass. |
| `wearos` | Debug Wear OS 3+ watch APK (`platforms/wearos`, incl. Pixel Watch & Samsung Galaxy Watch; built in CI) | Development builds only ([wearables guide](wearables.md)). No release until watch hardware, battery, thermal, and store gates in section 10 pass. |
| `watchos` | Apple Watch app (`platforms/watchos`) | Development builds only ([wearables guide](wearables.md)). No release until watchOS hardware, HealthKit workout, battery, thermal, and store gates in section 10 pass. |
| `harmonyos` / `harmonyos-watch` | Experimental HarmonyOS phone/tablet and Huawei Watch HAP (`platforms/harmonyos`) | Experimental development builds only ([wearables guide](wearables.md)). No release until HarmonyOS SDK/device gates in section 10 pass. |

Deploy the landing site independently when ready. Prove the relay with a
synthetic vault and existing CLI clients before connecting an owner's vault.
Release the desktop to an owner pilot after its packaging and startup gates
pass. Mobile publication follows the platform gates in section 10.

The owner's machine runs the vault and capture daemon. Neither the landing
deployment nor the relay receives the owner's database, media, passphrase,
private keys, or decrypted requests. Hosted vaults remain a future design,
not part of this release. See [hosting](hosting.md).

## 2. Close the relevant release prerequisites

These are implementation or verification tasks, not capabilities this plan
claims are already present. Each affects only the destinations listed.

| Destination | Prerequisite | Evidence required before distribution |
| --- | --- | --- |
| All | Record an immutable candidate and test results. Current CI checks builds/tests; it does not publish a complete release. `ci`, `application-targets` and `containers` can be started by hand (`gh workflow run ci.yml --ref <ref>`, likewise `apps.yml` and `containers.yml`; `ci.yml` and `apps.yml` also accept `-f ref=<sha>` for an exact commit). That matters because merges made by `sync-leptoskit.yml` use the repository token, which starts no `push` run, so the newest `main` commit can have no recorded run of its own. | Exact Quant SHA, SDK SHA, tool versions, checks, artifact hashes and rollback references. |
| Containers | Verify both actual Docker builds. Both Dockerfiles copy `rust-toolchain.toml` and install that toolchain before installing the SDK CLI, pin the Rust and Debian base images by multi-architecture digest, verify the binaryen and esbuild downloads by SHA-256 (landing), and run as the numeric user `10001:10001`. `containers.yml` builds each image for `linux/amd64` and smoke-tests it read-only as that user, including a restart of the relay on the same volume. | A green `containers` run on the candidate commit, with pinned digests refreshed deliberately as their own change. |
| Containers | Set owned app names, region, domains, registry access and machine capacity. Both checked-in Fly app names are template values (`quant-landing` in `deploy/landing/fly.toml` and `relay` in `deploy/relay/fly.toml`), and neither file sets a primary region; the commands in sections 6 and 7 pass it explicitly. | Reviewed environment inventory and successful staging deployment. |
| Landing | Keep public claims aligned with what is released. The site states that the Mac app and the CLI are built from source, and that screenshots leave out known password managers and detected private windows rather than promising "never". | Reviewed landing copy and `/privacy` against released platforms and behavior before promotion (section 6). |
| Relay | Prove persistent migrations, consistent backups, restore and client reconnection. | Restore drill, previous image digest and schema compatibility result. |
| Desktop | Verify the separated daemon/UI login entries (resolved in code). The daemon's LaunchAgent label is `dev.leptoskit.quant.daemon`; the menu-bar app's login entry keeps the bundle identifier `dev.leptoskit.quant`. `quant service install` and `uninstall` move or remove an entry the old installer wrote under the old label, recognised by its `daemon` argument, and never touch the app's own entry. | On an installed build, toggling "Open Quant at login" leaves the daemon's entry alone (section 8). |
| Desktop | Package and update the daemon as well as the menu-bar app. The app is currently a client of a separately installed daemon. The `desktop` job of `application-targets` now uploads a development artifact named `quant-desktop-macos` (a tarball of `App.app` and the release `quant` binary, ad-hoc signed, not notarized); it is for the owner's own pilot, not a distribution package. | Fresh-machine installation, stable executable paths, daemon discovery and upgrade/uninstall tests. |
| Desktop | Produce a distribution signature. The SDK build currently applies an ad-hoc signature; `--release` alone is insufficient. | Developer ID signature, required entitlements, notarization and Gatekeeper checks. |
| Desktop | Verify capture consent for the executable that actually requests access. | Required purpose strings, permission denial/revocation tests, and approved capture from an installed build. The menu-bar host's current plist alone does not prove this. |
| Desktop | Synchronize package/app versions and architecture support. | Workspace/config version, plist version/build number, CLI version and native architecture recorded together. |
| Web/mobile | Complete the product web target (the only target marked `unavailable` in `leptoskit.toml`) and measure the mobile and watch development builds. `android`, `ios`, `wearos`, `watchos`, `harmonyos` and `harmonyos-watch` are selectable so that development builds can be made; none of them is released. | A working target plus the acceptance evidence in section 10; removing `unavailable` alone does not qualify, and a selectable target building does not either. |
| Android and Wear OS | Advance the SDK pin past the foreground-service crash fixed in https://github.com/leptoskit/leptoskit/pull/1027. With the SDK pinned before that fix, the Android app crashes about a second after launch on Android 14 and later because the SDK's background plugin restarts its foreground service without a service type (`InvalidForegroundServiceTypeException`), reproduced on Android 14 and Android 16 phones. | Before any Android or Wear OS build is handed to anyone, the SDK pin includes that fix and the app is seen to stay running on an Android 14 or later device with its collecting notification showing. |

Do not label a target production-ready while its prerequisites remain open.
A website release does not imply the desktop or mobile collectors are ready.

## 3. Prepare the environment and release record

Use separate staging and production hosting resources, volumes, credentials,
synthetic identities and DNS names. Staging must never mount a production
vault or replay an owner's captured data.

Before the first deployment, record these values in the operator's deployment
inventory. Environment variables below are names to populate, not existing
resources or credentials supplied by this repository.

| Value | Purpose |
| --- | --- |
| `QUANT_ENV` | `staging` or `production`. |
| `QUANT_FLY_ORG`, `QUANT_REGION` | Hosting account and explicit primary region. |
| `QUANT_LANDING_APP`, `QUANT_RELAY_APP` | Distinct, owned Fly app names for this environment. |
| `QUANT_SITE_ORIGIN`, `QUANT_RELAY_ORIGIN` | HTTPS origins used for smoke tests. Clients use a `wss://` relay URL. |
| `QUANT_REGISTRY` | Container repository prefix accessible by the deployment platform. |
| `QUANT_RELAY_VOLUME_GB` | Capacity chosen for relay metadata and backup headroom. |
| `QUANT_LANDING_IMAGE`, `QUANT_RELAY_IMAGE` | Tested immutable image references, including `@sha256:...`. |
| Previous image/package references | Artifacts retained for rollback. |
| Release operator and incident contact | Who deploys, observes the pilot and performs recovery. |

Keep hosting tokens and Apple signing/notarization credentials in the operator's
keychain or deployment secret store, scoped to their destination. Release
evidence contains credential names, not values. Pairing links and invite codes
also stay out of public logs and release notes.

For every candidate, keep a private release record containing:

- Quant commit/tag, version, SDK revision and lockfile hash.
- Rust, Leptoskit CLI, Binaryen, Node, Xcode and hosting CLI versions used.
- Target/platform, container architecture or supported Mac architecture.
- Check results for that exact revision, artifact checksums/image digests,
  and signing/notarization results where applicable.
- Migration identifiers, pre-deploy backup references, restore-test result,
  previous artifacts and schema compatibility.
- Deployment timestamps, environment/resource identifiers, smoke-test results,
  observation results and the decision to promote or roll back.

## 4. Freeze and validate the candidate

### Update before freezing

During candidate preparation, update to the latest Leptoskit main revision
(also automated every 30 minutes by
[sync-leptoskit.yml](../.github/workflows/sync-leptoskit.yml), which opens a PR,
runs [ci.yml](../.github/workflows/ci.yml),
[apps.yml](../.github/workflows/apps.yml) and
[containers.yml](../.github/workflows/containers.yml), and squash-merges only
when every check passes). Two caveats apply to that bot: its merge uses the repository
token and starts no `push` run on `main` (see section 2 for running workflows by
hand), and the `pull_request` runs GitHub creates for the bot's pull request
wait for approval and are marked failed when the bot merges, with no jobs having
run. The verification that counts is the `verify-ci`, `verify-apps` and
`verify-containers` jobs inside the sync run.

To update by hand:

```sh
python3 scripts/update-leptoskit.py --no-check
python3 scripts/check-project.py
```

Review and commit the root manifest, lockfile and both Docker CLI pins together.
Then freeze the Quant SHA. Reuse those pinned inputs through staging and
production; do not resolve a newer SDK or regenerate the lockfile on the
production host. Any source or dependency change creates a new candidate.

Read the SDK pin and install its matching CLI on the build machine:

```sh
QUANT_SDK_REV=$(python3 -c 'import pathlib,tomllib; print(tomllib.loads(pathlib.Path("Cargo.toml").read_text())["workspace"]["dependencies"]["leptoskit"]["rev"])')
cargo install --git https://github.com/leptoskit/leptoskit \
  --rev "$QUANT_SDK_REV" --locked leptoskit_cli
```

Use the repository's `rust-toolchain.toml`, not an unrelated system toolchain.
Build from a clean checkout without local SDK overrides. Run independent
platform builds in separate jobs; use bounded build concurrency rather than
building every SDK/platform feature together on the owner's machine.

### Required evidence

```sh
git status --short
python3 scripts/check-project.py
cargo metadata --locked --no-deps --format-version 1 > /tmp/quant-release-metadata.json
cargo fmt --all --check
leptoskit targets
```

For shared core changes, require the core Clippy/tests on macOS and Linux as
defined in [ci.yml](../.github/workflows/ci.yml). For each shipping target,
require its package checks, tests, Leptoskit lint and production build from
[apps.yml](../.github/workflows/apps.yml), and for container targets require the
image build and read-only smoke test from
[containers.yml](../.github/workflows/containers.yml). Run the landing's
hydrated browser regression checks against the production bundle, not just an
SSR response. Test SQLCipher and migrations when the SDK or SQLite dependencies
change.

Use results for the exact candidate from CI or equivalent local checks. The
project permits local validation when CI is backlogged; waiting for every CI
job is not mandatory. Record which checks ran and where. An authorized
`--no-verify` push skips a Git hook, not this release evidence. A queued check
is not a passing check, and a documentation-only change does not need a full
native rebuild.

The shipping-target commands are:

```sh
leptoskit --app-target landing lint --no-clippy --deny-warnings
leptoskit --app-target landing build --release
leptoskit --app-target relay lint --no-clippy --deny-warnings
leptoskit --app-target relay build --release
leptoskit --app-target desktop lint --no-clippy --deny-warnings
leptoskit --app-target desktop build --release
cargo build --locked --release -p quant_cli --bin quant
```

Run the desktop build on a supported Mac. The `desktop` job of
`application-targets` also runs
`cargo build --locked --release -p quant_cli --bin quant`, so that command no
longer needs a separate local run when the candidate has a green
`application-targets` run. Package/architecture checks and container builds are
additional gates; a local Rust build does not substitute for them.

## 5. Build artifacts once

Use the repository root as both containers' build context. Initially build
Linux `amd64`, matching the existing Linux CI path; qualify any additional
architecture separately. After closing the container prerequisites in section
2, build and push candidate images to the configured registry:

```sh
QUANT_RELEASE_SHA=$(git rev-parse HEAD)
docker buildx build --platform linux/amd64 \
  --file deploy/landing/Dockerfile \
  --tag "$QUANT_REGISTRY/landing:$QUANT_RELEASE_SHA" \
  --metadata-file /tmp/quant-landing-image.json --push .
docker buildx build --platform linux/amd64 \
  --file deploy/relay/Dockerfile \
  --tag "$QUANT_REGISTRY/relay:$QUANT_RELEASE_SHA" \
  --metadata-file /tmp/quant-relay-image.json --push .
```

Record each registry digest from the build metadata, verify the pushed image,
and populate the immutable image variables from it. Preserve the metadata in
the release record. Promote these exact images; do not rebuild a tag for
production. Docker documents these build, platform, push and metadata options
in its [Buildx reference](https://docs.docker.com/reference/cli/docker/buildx/build/).

Run both images in staging with the intended runtime restrictions: unprivileged
user, read-only application filesystem, temporary scratch space, and only the
relay's `/data` mount writable. Verify UID 10001 can write its mounted database.
The landing container has no persistent data mount. Keep `.dockerignore`
exclusions for vaults, databases, local environments and build artifacts.

The macOS artifact comes from `.leptoskit/macos/App.app`; the CLI/daemon comes
from the candidate's native Cargo build. Copy them into a versioned packaging
directory before signing. Preserve the final signed package and its checksums;
the build directory is not a distribution channel.

## 6. Deploy and verify the landing site

The existing Fly files are the reference hosting path. Fly app names here are
deployment resources, not additional Leptoskit applications or workspaces.
Use explicit `--app` and `--config` arguments to avoid the template names or
an accidental environment selection.

For a new environment only, create the owned landing app:

```sh
fly apps create "$QUANT_LANDING_APP" --org "$QUANT_FLY_ORG"
```

Configure the environment's DNS/certificate and registry access, then deploy
the tested image:

```sh
fly deploy --config deploy/landing/fly.toml \
  --app "$QUANT_LANDING_APP" --primary-region "$QUANT_REGION" \
  --image "$QUANT_LANDING_IMAGE" --ha=false
fly status --app "$QUANT_LANDING_APP"
fly checks list --app "$QUANT_LANDING_APP"
curl --fail --show-error --silent "$QUANT_SITE_ORIGIN/healthz"
curl --fail --show-error --silent "$QUANT_SITE_ORIGIN/readyz"
```

Fly supports deployment from an existing image and explicit configuration;
`--ha=false` avoids automatically adding spare Machines for this initial
single-instance rollout. See [fly deploy](https://docs.fly.io/flyctl/cmd/fly_deploy).

Confirm `/`, `/privacy` and an unknown route return the expected content/status,
assets load, hydration completes, and theme/menu/FAQ/memory interactions work
at desktop and phone widths. Run `npm run test:browser` with `QUANT_SITE_URL`
set to this environment's site origin. Check TLS, redirects and absence of
vault/service routes or credentials in public assets.

Promote the same digest to production using production inventory values.
Keep downloads or capture claims limited to platforms actually released.
An unhealthy deployment or broken hydrated page is a rollback condition even
if `/healthz` responds successfully.

## 7. Deploy and verify the relay

### Persistent state and first deployment

For a new environment only:

```sh
fly apps create "$QUANT_RELAY_APP" --org "$QUANT_FLY_ORG"
fly volumes create data --app "$QUANT_RELAY_APP" \
  --region "$QUANT_REGION" --size "$QUANT_RELAY_VOLUME_GB"
```

Record the volume ID. Keep one active relay instance with one database volume
in that region. Quant's current relay has process-local connection routing;
adding replicas does not create shared routing or database replication.
Accept a maintenance interruption for pilot upgrades. Fly volumes attach to
one Machine and do not replicate automatically; see
[volume behavior](https://docs.fly.io/volumes/overview).

Use `/data/relay.sqlite3`, as configured by the container's `DATABASE_URL`.
Before upgrading an existing relay, take and verify the backup described in
section 9. The server applies migrations during startup, before readiness.
No separate migration job is currently configured.

```sh
fly deploy --config deploy/relay/fly.toml \
  --app "$QUANT_RELAY_APP" --primary-region "$QUANT_REGION" \
  --image "$QUANT_RELAY_IMAGE" --strategy immediate --ha=false
fly status --app "$QUANT_RELAY_APP"
fly checks list --app "$QUANT_RELAY_APP"
curl --fail --show-error --silent "$QUANT_RELAY_ORIGIN/healthz"
curl --fail --show-error --silent "$QUANT_RELAY_ORIGIN/readyz"
```

Verify exactly one serving instance and the intended volume before enabling
client traffic. Use `wss://`, preserve the external host through any proxy,
and keep automatic idle shutdown disabled. Keep proxy timeouts compatible
with the daemon's 30-second ping interval.

Bootstrap invite gating on the persistent database before opening registration
to general traffic. Run the image's `/app/server invite add` through a private
operator console; the first invite enables gating. Keep the initial endpoint
restricted to the pilot until this is done. Store the generated code privately;
do not embed it in the image or a public release record.

### End-to-end acceptance

Use a synthetic daemon and a second client, then repeat for the owner pilot:

1. Configure the daemon with `quant relay set wss://<relay-host>` and a pilot
   invite where required. Confirm `quant relay status` and outbound connection.
2. Pair and approve the client with limited scopes, lookback and expiry.
3. Make the direct path unreachable and prove the request actually used the
   relay; a healthy direct request is not a relay test.
4. Verify certificate-pin mismatch rejection, unapproved-client rejection,
   insufficient-scope rejection, a successful permitted request and its audit
   entry. Verify revoked grants stop working.
5. Restart the relay. Confirm existing sockets reconnect and registrations,
   invite state and usage counters survive. An empty database is a failed
   persistence test, not a successful restart.
6. Exercise configured limits and an interrupted upload/retry without duplicate
   ingestion or unbounded backlog. Check logs contain no payloads or keys.

Retain evidence from [relay tests](../crates/quant_relay_server/tests/relay.rs)
and the deployed smoke test. Health checks alone cannot prove encrypted routing.
Finish staging acceptance before using the same image in the production pilot.

The Compose and systemd files in `deploy/relay/` are alternative supervisors.
Select one hosting path per environment; preserve the same persistence, backup,
TLS, health and rollback requirements when using either alternative.

## 8. Package and release macOS

### Distribution package

Close the desktop prerequisites in section 2 first. Preserve the bundle ID
`dev.leptoskit.quant` and the signing team across updates unless an identity
migration has been designed and tested.

Package the menu-bar app and CLI/daemon together with a documented, stable
installation layout. Configure daemon startup only after the owner initializes
or selects a vault. Preserve `QUANT_HOME`, client configuration and keychain
identity on upgrades; never run `quant init` as an upgrade step. An installation
without a keychain-stored vault key must support the documented interactive
unlock path rather than launching an unattended process that cannot unlock.

Use Developer ID distribution, hardened runtime and the reviewed entitlements.
Sign nested executable code before the outer bundle and verify every shipped
executable. Do not substitute a blanket signing command for checking the
package's actual contents. Apple describes these requirements in
[Developer ID distribution](https://developer.apple.com/developer-id/).

After signing, submit a packaging copy with a preconfigured keychain profile:

```sh
xcrun notarytool submit "$QUANT_NOTARIZATION_ARCHIVE" \
  --keychain-profile "$QUANT_NOTARY_PROFILE" --wait
xcrun stapler staple "$QUANT_SIGNED_APP"
xcrun stapler validate "$QUANT_SIGNED_APP"
codesign --verify --strict --verbose=2 "$QUANT_SIGNED_APP"
spctl --assess --type execute --verbose=2 "$QUANT_SIGNED_APP"
```

Require an accepted notarization result and inspect its log. Staple the app,
then recreate a final ZIP if ZIP distribution is used; the ZIP submitted for
notarization is not automatically the final download. Record checksums after
this step. Apple documents the sequence in its
[notarization workflow](https://developer.apple.com/documentation/security/customizing-the-notarization-workflow).

### Installed-build acceptance

Test the downloaded package on a clean supported Mac, not only on the build
machine. The current app plist declares macOS 14 minimum support; either prove
that version or adjust the declaration before distribution. Advertise only
architectures whose final packages were tested.

- First launch, existing-vault discovery, interactive unlock and restricted
  pairing work. Touch ID works with the distribution signature, and declined
  authentication leaves a usable passphrase path.
- The daemon and UI each start correctly after login; toggling UI autostart
  does not alter the daemon. Closing the window hides it and keeps its guest
  loaded while the menu-bar service remains available. Quitting UI and stopping
  capture have clearly tested behavior.
- Capture consent, denial, revocation, pause/resume, screen lock, sleep/wake
  and offline recovery behave correctly. Test microphone/screen permissions
  on the actual installed executable identities.
- A previous-version vault upgrades without losing records, media, grants,
  client identities or audit continuity. Uninstall preserves owner data unless
  deletion is separately requested through the existing owner-controlled path.
- Measure UI-only idle use separately from capture/transcription. Record CPU,
  resident memory, wakeups, disk growth and thermal state over at least one
  hour, including repeated window open/close. Look for sustained CPU activity
  at idle and memory growth across repeated window open and close. Set and
  record measured budgets before the pilot; do not invent a claim of negligible
  resource use.

Run a 24-hour owner pilot covering normal use and a relay outage before widening
distribution. Collect operational metrics without captured content. Publish
the final signed package, checksums, version/compatibility notes and known
limitations only after acceptance. Code push remains disabled in the root
configuration; this plan does not introduce a second update channel.

## 9. Backup, rollback and operation

### Backups and restore drills

There is no `quant backup` or `quant restore` command today. Use a documented,
tested storage-copy procedure and keep its exact steps with the release record.

For a vault, stop the daemon and other writers cleanly, then copy the complete
vault directory, including `vault.json`, `quant.db`, sealed `media/` and any
remaining SQLite WAL/journal files as one consistent set. Preserve required
client configuration and identities privately. Protect the backup with owner
access controls and encryption; keep passphrase recovery material separately.
Never put an owner's vault or keychain export in a release artifact.
Exclude ephemeral Unix sockets from the archive; restore persistent files with
their owner-only permissions intact.

For the relay, stop writers for a consistent database/volume backup, preserve
any WAL/journal files, and save it outside the active volume. Record snapshot
IDs and image/schema versions. Automatic Fly snapshots are supplementary,
not the only backup; see [snapshot recovery](https://docs.fly.io/volumes/snapshots).

Take backups before every state-changing upgrade and schedule regular backups
between releases. Choose retention, maximum acceptable data loss and recovery
time for the pilot, then measure them in a restore drill. Verify backups on
an isolated copy: unlock the vault, query known records and verify the audit
chain; for relay state, verify registrations/invites/counters and reconnect a
synthetic client. Keep the original copy untouched until recovery succeeds.

### Rollback decisions

| Failure | Action |
| --- | --- |
| Landing health, assets or hydration regression | Redeploy the previous image digest with the same environment configuration; rerun route and browser checks. |
| Relay binary failure, schema remains backward compatible | Redeploy the previous digest with `--strategy immediate --ha=false`; preserve the volume and verify client reconnection. |
| Relay migration is incompatible with the old binary | Stop writers, restore the verified pre-upgrade backup to a recovery volume, then run the matching old image. Account for post-backup registrations and counters before reopening traffic. |
| Desktop regression with compatible vault format | Stop the affected processes, reinstall the prior signed package and retain the existing data/keychain configuration. |
| Desktop migration is incompatible | Preserve the failed state for recovery, restore a consistent vault copy and use its matching prior package. Do not run an old daemon against an untested newer schema. |
| Key exposure or authorization regression | Stop the affected access path and follow an incident recovery procedure before ordinary rollout resumes. An image rollback alone does not revoke leaked credentials. |

Migrations run on relay startup and vault open. Do not apply down migrations
automatically or assume an old image can open newer state. Where restore loses
newer data, retain the post-failure copy and record that loss/reconciliation.

### Observation and handover

During rollout, watch readiness, error rates, relay reconnects, resource use,
disk/volume headroom and capture/spool recovery. A synthetic end-to-end probe
should exercise the relay; site uptime alone is insufficient. Logs and alerts
must exclude transcripts, screenshots, raw audio, signing material, pairing
links and invite codes.

Before handover, identify who receives alerts and prove they can access the
previous artifacts and recover a backup. Record the deployed digests/package
hashes and outcome after the observation window. Stop promotion or roll back
on data-integrity failures, unauthorized reads, repeated crashes, failure to
reconnect, or sustained resource growth beyond the measured release budget.

## 10. Gates for future product web and mobile releases

Do not deploy unreleased web or mobile targets. Only `web` is marked
`unavailable` in `leptoskit.toml`; `android`, `ios`, `wearos`, `watchos`,
`harmonyos` and `harmonyos-watch` are selectable so that development builds can
be made, which is not a release, and none of them has the device evidence below.
A first release of each platform requires:

| Platform | Required implementation and acceptance |
| --- | --- |
| Product web | Authenticated transport and owner sessions, scoped/rate-limited API access, route/asset isolation from landing, browser session security, and proof that the relay/public site cannot access vault keys. |
| Android | Native host, collector and encrypted bounded spool; permission/foreground lifecycle; continuous-audio chunking with silent chunks suppressed; location, motion, steps, thermal/usage integrations as actually supported; app-usage access through the platform's explicit settings flow. Administrative privileges must never stand in for specific consent or unsupported capture capabilities. |
| iOS | Native host and permitted background collection, explicit consent and visible capture state, encrypted bounded spool, interruption/restart handling, and evidence of what continues when backgrounded/locked. Document unsupported data sources instead of promising Android-equivalent capture. |
| Wear OS (`wearos`) | Wear OS 3+ device verification (Pixel Watch and Samsung Galaxy Watch), Health Services / optional Samsung Health Sensor SDK permission flows, Tile/Complication updates, companion sync fallback, and real-device battery/thermal measurements. |
| watchOS (`watchos`) | Apple Watch hardware verification, `HealthKit` workout/passive delivery, `WatchConnectivity` companion sync, `WidgetKit` complication timelines, and real-device battery/thermal measurements. |
| HarmonyOS (`harmonyos`, `harmonyos-watch`) | Qualified SDK/host/toolchain, Huawei Watch Sensor/Health/WearEngine hardware verification, signing and device tests. Keep its experimental status explicit until those pass. |

For Android and iOS, require emulator/simulator regression tests and real-device
permission, background, battery and thermal measurements. Emulators do not
establish battery-life or overheating behavior. Test silence, five-minute
audio boundaries, upload retry/deduplication, bounded disk/memory, low storage,
low battery, network loss and revocation of each capture permission. Confirm
captured data is not exported to public infrastructure or unapproved models.

Also exercise an external `quant://link` on both platforms, cold and while
running: Quant must open with a reviewable Connect card, send nothing until
Connect is tapped, and never replace a paired vault merely because a link
arrived. Pasting the link must remain available as the fallback, and the
receiving platform must not request camera permission.

Produce signed platform packages with version/build numbers, privacy metadata
and an internal-testing distribution first. Record supported OS versions and
device classes, migration behavior and a recovery strategy; mobile storefronts
may require a replacement release rather than an immediate downgrade. Verify
current platform/store requirements when that release is implemented.

## 11. Deployment completion checklist

- [ ] Scope, environment and exact release revision recorded.
- [ ] Target prerequisites closed; tests refer to that revision.
- [ ] SDK/lockfile/toolchain and artifact checksums/digests recorded.
- [ ] State backed up; restore and downgrade compatibility demonstrated.
- [ ] Staging runs the actual artifacts and passes target acceptance.
- [ ] Production receives the same tested artifacts and intended configuration.
- [ ] Health, end-to-end behavior, permissions and resource use verified.
- [ ] Previous artifacts retained; operator can execute the recovery steps.
- [ ] Public claims/downloads match released platform capabilities.
- [ ] Observation outcome and operational handover recorded.

Before automating publication, add a release workflow that implements this
sequence: explicit target/environment selection, candidate validation, artifact
build and retention, staging verification, promotion by digest/package hash,
and an evidence record. Reuse the current target checks; do not conflate a
successful CI run with an already deployed release.

## 12. LeptosKit Cloud

LeptosKit Cloud is the SDK's managed hosting. It is not publicly live, and
nothing in this plan uses it yet; Fly remains the reference path.

Only `landing` and `relay` could ever run there. The vault, the daemon and the
capture pipeline stay on the owner's machine, as with any other host. Both
servers already meet Cloud's runtime contract as the pinned SDK defines it:
`/healthz` and `/readyz` (the relay target declares `probes = true`), graceful
shutdown inside the grace period, a read-only root filesystem, port 3000, and a
numeric non-root user.

What blocks using it is in the SDK at the current pin, not in Quant:

- `[cloud]` is a single top-level table and `lk cloud` and `lk deploy` ignore
  `--app-target`, so one `leptoskit.toml` can describe only one hosted app, and
  Quant has two.
- The Cloud builder only looks for a `Dockerfile` at the repository root, and
  Quant's are under `deploy/`.

Gates before adopting LeptosKit Cloud once those SDK limits are lifted:

- Do not add `[cloud].project` to `leptoskit.toml` before then, because it makes
  plain `lk deploy` ship to Cloud.
- Run the relay as exactly one always-on instance (`min = 1`, `max = 1`,
  `sleep = "never"`), because its connection routing is in process memory.
- Provide another route for `relay invite add` before using invite gating there,
  because Cloud has no way to run a one-off command in a container.
- Verify on the live service that UID 10001 can write the mounted `/data` volume
  and that a `wss://` connection survives more than two minutes idle through its
  proxy.
