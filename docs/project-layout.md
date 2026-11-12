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
    quant_app/         product UI, currently using the native service bridge
    quant_model/       UI/service contracts
    quant_service/     resident desktop service and daemon client
    quant_landing_server/   public SSR binary (package: landing)
    landing_app/       public file routes, components and stores
    landing_backend/  public health/RPC endpoints
    landing_shared/   public contracts
    quant_relay_server/     blind relay binary (package: relay)
    relay_db/          relay metadata and migrations
    quant_*/           capture, vault, authorization, CLI, MCP and collector
  platforms/macos/     app-owned Swift lifecycle and Rust runner
  assets/app/          product assets
  assets/landing/      public assets
  web/landing/         public web entry files
  deploy/landing/      public deployment container and Fly configuration
  deploy/relay/        relay deployment container and Fly configuration
  tests/landing/       hydrated browser checks
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
`--app-target landing` or `--app-target relay`. Deployment files use that same
root context. No credentials are stored in the target definitions.

`web`, `android`, `ios` and `harmonyos` reserve the intended destinations in the
same system. Selection currently fails with an explanation because the
existing product UI has a native desktop bridge and the mobile collector
lifecycle is not integrated. The collector/spool library already remains a
workspace member. Declaring a target does not enable background permissions
or claim continuous capture support on a platform.

Run `python3 scripts/check-project.py` for structural checks. Core tests use
the workspace's default members; target CI selects explicit packages and
builds with the matching SDK CLI. The SDK update script updates one manifest,
both container CLI pins and the single lockfile.
