# Quant desktop app

A menu-bar remote control for `quant daemon`. The daemon does all the capturing; this app shows what it is doing, saves the last few minutes of the rolling audio buffer, pauses and resumes capture, and opens a small window to approve agents and read the audit log. It is built with [leptoskit](https://github.com/leptoskit/leptoskit), and the same app will gain an Android target (the always-on collector).

It is meant to cost nothing while it sits in the menu bar: no Dock icon, no window until you open one, and a status check every 60 seconds (every 10 while the menu or window is open), with the thread asleep in between.

## What it does

The menu-bar menu:

- A status line, such as "Recording · 4 sources", "Paused until 15:30", or "Waiting for approval · K7QD-M2XA".
- A warning for each macOS permission a source is waiting for, which opens the right pane of System Settings.
- **Save the last 5, 15, 30, or 60 minutes** of the rolling audio buffer. When nothing was recorded in that window, it says so.
- **Pause for 30 minutes**, **1 hour**, or **until resumed**; or **Resume**.
- **Open Quant…** and **Quit Quant**.

The window has four sections: **Status** (the daemon's version and each capture source), **Saved moments** (play and rename), **Agents** (approve a pending pairing with the scopes and lookback you choose, deny it, or revoke a grant), and **Audit** (the last 50 entries and whether the hash chain verifies).

## Security

- The app is its own client: profile `desktop`, kind `device`, asking once for `audio:write audio:read audio:raw`. It pairs the first time it reaches the daemon and shows the pairing code. Approve it with `quant approve <code>`, or unlock the window and approve "Quant desktop" under Agents.
- Approving, denying, revoking, pausing, resuming, and the audit log need an owner session. The window asks for the passphrase in a secure field and sends it to the daemon once; the app keeps only the 15-minute session token, in memory, and never writes the passphrase anywhere. Quitting ends the session.
- The status line uses `GET /v1/capture/status`, which any approved client may call: which sources run, whether capture is paused, and which permission a stopped source needs. Nothing captured is in it.
- Listing saved moments is an audited read, so the app does it when the window opens and after a save, not on every status check.

## How it is put together

| Path                  | What it is                                                                                                                                                   |
| --------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------ |
| `quant_service`       | Native Rust: the worker thread with the signed `quant_client` and the owner session, the menu-bar item (`leptoskit_plugin_tray`), and the window's service. |
| `quant_app`           | The window, a leptoskit Wasm guest written with builder calls. It only talks to `quant_service`.                                                             |
| `quant_model`         | What the window and the service say to each other.                                                                                                           |
| `macos/`              | The app-owned macOS host: `App.swift` and `Info.plist` (`LSUIElement`).                                                                                       |
| `assets/tray/icon.png` | The menu-bar icon: a black-on-transparent template image, 36 × 36 pixels (18 points at 2×). Replace this file to change it.                                  |

The host starts the resident core in `quant_service` at launch: a worker thread that talks to the daemon, and the status item. The window's guest is created the first time the window opens and reaches the core through a leptoskit app service (`rust-service-factory`), so an app that stays in the menu bar never loads it.

## Build and run

You need a Mac with Xcode, the toolchain in the repository's `rust-toolchain.toml` (it includes the Wasm target), and the leptoskit CLI built from the same leptoskit commit the app pins:

```sh
cargo install --git https://github.com/leptoskit/leptoskit --rev <the rev in Cargo.toml> leptoskit_cli
cd apps/quant
leptoskit build --platform macos            # or --release
open .leptoskit/macos/App.app
```

leptoskit is not on crates.io, so `Cargo.toml` pins a commit of the public repository. To build against a local checkout instead, point `LEPTOSKIT_SDK` at it and run the script, which writes a Cargo `[patch]` to `.cargo/config.toml` (ignored by git); `--off` removes it:

```sh
LEPTOSKIT_SDK=~/src/leptoskit scripts/local-sdk.sh
scripts/local-sdk.sh --off
```

The app finds the daemon the way the CLI does: `QUANT_ADDR`, or the socket of `QUANT_HOME` (default `~/Library/Application Support/Quant`), with its identity in `QUANT_CONFIG` (default `~/Library/Application Support/Quant/clients`). Run the executable directly to point it at a scratch vault:

```sh
QUANT_HOME=/tmp/q/vault QUANT_CONFIG=/tmp/q/clients .leptoskit/macos/App.app/Contents/MacOS/App
```

### Open at login

The Status section has an "Open Quant at login" checkbox, which registers the app with `SMAppService.mainApp` and shows what macOS reports back. A local build is signed ad hoc; macOS may refuse it, or list it under System Settings → General → Login Items for you to allow. A signed copy in `/Applications` registers normally.

### Snapshots

Setting `QUANT_SNAPSHOT=<directory>` makes the app draw its own menu-bar menu and window into PNGs there, without needing Screen Recording permission. `QUANT_SNAPSHOT_STEPS` scripts what it does first (see `Snapshots` in `macos/App.swift`). It is a development aid and does nothing unless set.

## Tests

```sh
cargo test --workspace     # wording, time windows, scope choice, the menu, and the window's sections
```
