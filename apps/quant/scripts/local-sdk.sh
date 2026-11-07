#!/bin/sh
# Build against a local leptoskit checkout instead of the pinned commit.
#
#   LEPTOSKIT_SDK=~/src/leptoskit scripts/local-sdk.sh   # write .cargo/config.toml
#   scripts/local-sdk.sh --off                           # back to the pin
#
# It writes a Cargo `[patch]` for every leptoskit crate the app uses. Build
# the CLI from the same checkout (`cargo build -p leptoskit_cli` there), so
# its Swift and Kotlin host sources match the crates.
set -eu
cd "$(dirname "$0")/.."
config=.cargo/config.toml

if [ "${1:-}" = "--off" ]; then
	rm -f "$config"
	echo "using the pinned leptoskit commit"
	exit 0
fi

sdk=${LEPTOSKIT_SDK:?set LEPTOSKIT_SDK to a leptoskit checkout}
sdk=$(cd "$sdk" && pwd)
[ -f "$sdk/crates/leptoskit/Cargo.toml" ] || {
	echo "$sdk is not a leptoskit checkout" >&2
	exit 1
}

mkdir -p .cargo
{
	echo "# Written by scripts/local-sdk.sh; remove with --off."
	echo '[patch."https://github.com/leptoskit/leptoskit"]'
	for crate in leptoskit leptoskit_bridge leptoskit_bridge_ffi leptoskit_build \
		leptoskit_native leptoskit_plugin_menu leptoskit_plugin_tray leptoskit_testing leptoskit_wasm; do
		echo "$crate = { path = \"$sdk/crates/$crate\" }"
	done
} >"$config"
echo "using leptoskit from $sdk"
