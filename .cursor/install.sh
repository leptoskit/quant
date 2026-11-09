#!/usr/bin/env bash
# Bootstrap a Linux Cloud Agent for the Quant workspace.
#
# Idempotent: safe to re-run after a snapshot or a branch switch. The desktop
# app (apps/quant) links macOS frameworks and is not built here. CI on
# macos-latest covers that target.
set -euo pipefail

# Vendored SQLCipher (rusqlite) compiles OpenSSL and needs a C toolchain.
# The default Cloud Agent image already has these; install them only if missing.
if ! command -v cc >/dev/null 2>&1 \
	|| ! command -v cmake >/dev/null 2>&1 \
	|| ! command -v pkg-config >/dev/null 2>&1 \
	|| ! command -v perl >/dev/null 2>&1; then
	sudo apt-get update
	sudo apt-get install -y --no-install-recommends \
		build-essential ca-certificates cmake pkg-config perl
fi

# rust-toolchain.toml pins the nightly, components, and wasm targets.
# `rustup show` installs that toolchain when it is not already present.
# /usr/local/cargo/bin is on the login-shell PATH of the default image.
rustup show active-toolchain

cargo fetch --locked
# Compile library, bins, and tests without running them, so a later agent
# is not waiting on a cold SQLCipher build. Re-running is a no-op when
# nothing changed.
cargo test --workspace --locked --no-run

# The relay is its own workspace and is not part of `cargo test --workspace`.
# Fetch it so an agent can build and test it without a cold git clone of
# the pinned Leptoskit revision.
if [[ -f apps/relay/Cargo.toml ]]; then
	(cd apps/relay && cargo fetch --locked)
fi
