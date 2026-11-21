#!/bin/sh
# Build and install Quant on an Apple Watch or watchOS Simulator (macOS + Xcode).
#
#   scripts/watchos-device.sh doctor    # check Xcode, watchOS SDK, simulators
#   scripts/watchos-device.sh build     # prepare and build watchOS host
#   scripts/watchos-device.sh           # build and open in watchOS Simulator
set -eu
cd "$(dirname "$0")/.."

cmd=${1:-run}
app_id="dev.leptoskit.quant.watchkitapp"

case "$cmd" in
	doctor)
		xcrun --sdk watchos --show-sdk-path
		xcrun simctl list devices available | grep -i "Apple Watch" || true
		command -v leptoskit >/dev/null 2>&1 && leptoskit --version || echo "leptoskit CLI not on PATH"
		;;
	build)
		rustup target add wasm32-unknown-unknown >/dev/null 2>&1 || true
		leptoskit --app-target watchos build
		;;
	run)
		"$0" build
		echo "Install the built watchOS bundle via Xcode or xcrun simctl install booted <path> ($app_id)."
		;;
	*)
		echo "Usage: $0 [doctor|build|run]" >&2
		exit 1
		;;
esac
