# One Quant application

`leptoskit.toml` owns the project. The root `Cargo.toml` lists all packages,
including the native runner, and Cargo resolves them into one `Cargo.lock`.
Individual crates have manifests but never independent workspaces or lockfiles.

```text
quant/
  leptoskit.toml        identity, frontends, named platform/deployment targets
  Cargo.toml           one workspace and shared SDK revision
  Cargo.lock
  crates/
    quant_app/         product UI: the desktop window and the phone screen
    quant_model/       UI/service contracts
    quant_service/     resident desktop service and daemon client
    quant_phone/       phone service: encrypted spool, identity and uploads
    quant_landing_server/   public SSR binary (package: landing)
    landing_app/       public file routes, components and stores
    landing_backend/  public health/RPC endpoints
    landing_shared/   public contracts
    quant_relay_server/     blind relay binary (package: relay)
    relay_db/          relay metadata and migrations
    quant_*/           capture, vault, authorization, CLI, MCP and collector
  platforms/macos/     app-owned Swift lifecycle and Rust runner
  platforms/android/   Gradle project, manifest and the Rust host's plugin list
  assets/app/          product assets
  assets/landing/      public assets
  web/landing/         public web entry files
  deploy/landing/      public deployment container and Fly configuration
  deploy/relay/        relay deployment container and Fly configuration
  tests/landing/       hydrated browser checks
  scripts/android-device.sh   build, install and drive the app on a USB phone
  .leptoskit/          generated host/plugin state (ignored)
  dist/<target>/       isolated web/server output (ignored)
```

A frontend selects UI code, routes, render mode and assets. A named target
selects its frontend, platform, server package, listener, database and sidecars.
The desktop's native service starts before a window exists. The public landing
frontend cannot start vault services implicitly: sidecars must be listed for
each target. The relay has its own database and forwards end-to-end encrypted
traffic; sharing a build workspace does not give it the vault's keys.

Build and deploy commands run at the root with `--app-target desktop`,
`--app-target landing`, `--app-target relay` or `--app-target android`.
Deployment files use that same root context. No credentials are stored in the
target definitions.

The `android` target builds the phone collector: `quant_app` with its `android`
feature is the screen, `quant_phone` is its native service, and
`platforms/android` is the host. It is a development build, described in
[Quant on a phone](phone.md).

`web`, `ios` and `harmonyos` reserve the intended destinations in the same
system. Selection currently fails with an explanation because the product web
frontend has no authenticated transport and those platforms have no host.
Declaring a target does not enable background permissions or claim continuous
capture support on a platform.

Run `python3 scripts/check-project.py` for structural checks. Core tests use
the workspace's default members; target CI selects explicit packages and
builds with the matching SDK CLI. The SDK update script updates one manifest,
both container CLI pins and the single lockfile.

Release preparation, deployment, verification and rollback follow the
[deployment plan](deployment-plan.md).
