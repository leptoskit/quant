#!/bin/sh
# Build, install, and launch Quant on a Wear OS watch (Pixel Watch, Samsung
# Galaxy Watch / One UI Watch, TicWatch, or Wear OS emulator) over ADB.
#
#   scripts/wearos-device.sh doctor     # check tools and attached watches
#   scripts/wearos-device.sh build      # build the Wear OS APK
#   scripts/wearos-device.sh            # build, install, and open on watch
set -eu
cd "$(dirname "$0")/.."

cmd=${1:-run}
apk="platforms/wearos/app/build/outputs/apk/debug/app-debug.apk"
app_id="dev.leptoskit.quant"

find_adb() {
	if command -v adb >/dev/null 2>&1; then
		command -v adb
	elif [ -n "${ANDROID_HOME:-}" ] && [ -x "$ANDROID_HOME/platform-tools/adb" ]; then
		echo "$ANDROID_HOME/platform-tools/adb"
	else
		echo "adb not found; install Android SDK platform-tools or set ANDROID_HOME" >&2
		exit 1
	fi
}

case "$cmd" in
	doctor)
		adb=$(find_adb)
		echo "adb: $adb"
		"$adb" devices -l
		command -v leptoskit >/dev/null 2>&1 && leptoskit --version || echo "leptoskit CLI not on PATH"
		;;
	build)
		rustup target add wasm32-unknown-unknown aarch64-linux-android >/dev/null 2>&1 || true
		leptoskit --app-target wearos build
		;;
	install)
		adb=$(find_adb)
		path=${2:-$apk}
		"$adb" ${QUANT_WEAROS_SERIAL:+-s "$QUANT_WEAROS_SERIAL"} install -r "$path"
		;;
	open)
		adb=$(find_adb)
		"$adb" ${QUANT_WEAROS_SERIAL:+-s "$QUANT_WEAROS_SERIAL"} shell am start \
			-n "$app_id/dev.leptoskit.quant.wearos.MainActivity"
		;;
	run)
		"$0" build
		"$0" install "$apk"
		"$0" open
		;;
	*)
		echo "Usage: $0 [doctor|build|install [APK]|open|run]" >&2
		exit 1
		;;
esac
