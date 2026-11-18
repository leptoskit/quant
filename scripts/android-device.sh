#!/usr/bin/env bash
# Build the Quant phone app and run it on an Android phone attached over USB.
#
#   scripts/android-device.sh            build, install and open the app
#   scripts/android-device.sh doctor     check the tools and the phone
#   scripts/android-device.sh help       every command
#
# The phone needs USB debugging switched on (Settings, Developer options) and
# this computer allowed when the phone asks. See docs/phone.md.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
PACKAGE="dev.leptoskit.quant"
ACTIVITY="$PACKAGE/.MainActivity"
APK="$ROOT/platforms/android/app/build/outputs/apk/debug/app-debug.apk"
RUST_TARGET="aarch64-linux-android"
PORT="${QUANT_PORT:-7717}"

say() { printf '%s\n' "$*"; }
fail() {
	printf 'error: %s\n' "$*" >&2
	exit 1
}

usage() {
	cat <<EOF
Usage: scripts/android-device.sh [-s SERIAL] [COMMAND]

Commands:
  run               Build, install and open the app (the default).
  build             Build the debug APK.
  install [APK]     Install the APK just built, or the one given, such as the
                    quant-android-debug artifact from CI.
  open              Open the installed app.
  stop              Stop the app.
  logs              Follow the app's log. Stop with Ctrl-C.
  usb [PORT]        Let the phone reach this computer's daemon through the
                    cable, at 127.0.0.1:PORT on the phone (default $PORT).
  link [LINK]       Type a pairing link into the field in focus on the phone.
                    Tap the link field first. Without LINK, reads the output
                    of \`quant link\` from standard input.
  grant             Grant location, notifications and Usage Access from here,
                    without tapping through the phone's prompts. For testing.
  status            What is installed, running and granted.
  screenshot [FILE] Save what the phone shows (default quant-phone.png).
  uninstall         Remove the app and everything it stored.
  doctor            Check the tools and the phone.

Options:
  -s SERIAL         The phone to use when more than one is attached. The
                    ANDROID_SERIAL environment variable does the same.

Pair over the cable, with no shared Wi-Fi:
  quant service install --tcp 0.0.0.0:$PORT
  scripts/android-device.sh usb
  quant link --host 127.0.0.1 | scripts/android-device.sh link
EOF
}

# Find the Android SDK where Android Studio puts it, unless the environment
# already says.
find_sdk() {
	if [ -n "${ANDROID_HOME:-}" ] || [ -n "${ANDROID_SDK_ROOT:-}" ]; then
		return
	fi
	local candidate
	for candidate in "$HOME/Library/Android/sdk" "$HOME/Android/Sdk"; do
		if [ -d "$candidate" ]; then
			export ANDROID_HOME="$candidate"
			return
		fi
	done
}

find_adb() {
	if command -v adb >/dev/null 2>&1; then
		ADB="adb"
		return
	fi
	local sdk="${ANDROID_HOME:-${ANDROID_SDK_ROOT:-}}"
	if [ -n "$sdk" ] && [ -x "$sdk/platform-tools/adb" ]; then
		ADB="$sdk/platform-tools/adb"
		return
	fi
	fail "adb was not found. Install Android Studio or the platform tools (https://developer.android.com/tools/releases/platform-tools) and set ANDROID_HOME."
}

# Make sure exactly one phone is chosen and it has authorised this computer.
need_device() {
	find_adb
	local devices
	devices="$("$ADB" devices | awk 'NR > 1 && NF >= 2 { print $1 "\t" $2 }')"
	if [ -z "$devices" ]; then
		fail "No phone is attached. Plug it in, switch on USB debugging, and allow this computer on the phone."
	fi
	if [ -z "${ANDROID_SERIAL:-}" ]; then
		if [ "$(printf '%s\n' "$devices" | wc -l | tr -d ' ')" -gt 1 ]; then
			printf '%s\n' "$devices" >&2
			fail "More than one device is attached. Choose one with -s SERIAL."
		fi
		ANDROID_SERIAL="$(printf '%s\n' "$devices" | cut -f1)"
		export ANDROID_SERIAL
	fi
	local state
	state="$(printf '%s\n' "$devices" | awk -F'\t' -v serial="$ANDROID_SERIAL" '$1 == serial { print $2 }')"
	case "$state" in
		device) ;;
		unauthorized) fail "The phone has not allowed this computer. Unlock it and accept the USB debugging prompt." ;;
		"") fail "No attached device has the serial $ANDROID_SERIAL." ;;
		*) fail "The phone is $state, not ready." ;;
	esac
}

# The Leptoskit revision the workspace pins, so the CLI matches the crates.
pinned_revision() {
	sed -n 's/^leptoskit = .*rev = "\([0-9a-f]\{40\}\)".*/\1/p' "$ROOT/Cargo.toml" | head -n 1
}

need_build_tools() {
	find_sdk
	command -v cargo >/dev/null 2>&1 || fail "cargo was not found. Install Rust from https://rustup.rs."
	if ! command -v leptoskit >/dev/null 2>&1; then
		fail "The leptoskit CLI was not found. Install the pinned one:
  cargo install --git https://github.com/leptoskit/leptoskit --rev $(pinned_revision) --locked leptoskit_cli"
	fi
	if [ -z "${ANDROID_HOME:-}${ANDROID_SDK_ROOT:-}${ANDROID_NDK_HOME:-}" ]; then
		fail "The Android SDK was not found. Install it with Android Studio, add an NDK (SDK Manager, SDK Tools, NDK), and set ANDROID_HOME."
	fi
}

build() {
	need_build_tools
	(
		cd "$ROOT"
		rustup target add "$RUST_TARGET" wasm32-unknown-unknown >/dev/null
		leptoskit --app-target android build
	)
	[ -f "$APK" ] || fail "The build finished without writing $APK."
	say "Built $APK"
}

install() {
	local apk="${1:-$APK}"
	[ -f "$apk" ] || fail "There is no APK at $apk. Run \`scripts/android-device.sh build\` first."
	need_device
	local abi
	abi="$("$ADB" shell getprop ro.product.cpu.abi | tr -d '\r')"
	if [ "$abi" != "arm64-v8a" ]; then
		say "warning: this device is $abi and the app is built for arm64-v8a; it may not install."
	fi
	"$ADB" install -r "$apk"
}

open_app() {
	need_device
	"$ADB" shell am start -n "$ACTIVITY" >/dev/null
	say "Opened Quant on $ANDROID_SERIAL."
}

logs() {
	need_device
	local pid
	pid="$("$ADB" shell pidof -s "$PACKAGE" | tr -d '\r' || true)"
	[ -n "$pid" ] || fail "Quant is not running. Open it with \`scripts/android-device.sh open\`."
	"$ADB" logcat -v time --pid="$pid"
}

usb() {
	local port="${1:-$PORT}"
	need_device
	"$ADB" reverse "tcp:$port" "tcp:$port" >/dev/null
	say "The phone now reaches this computer's port $port at 127.0.0.1:$port, for as long as the cable stays in."
	say "Pair with: quant link --host 127.0.0.1 --port $port | scripts/android-device.sh link"
}

type_link() {
	local link="${1:-}"
	if [ -z "$link" ]; then
		[ -t 0 ] && fail "Give the link, or pipe \`quant link\` into this command."
		link="$(grep -o 'quant://link[^[:space:]]*' | head -n 1 || true)"
	fi
	case "$link" in
		quant://link\?*) ;;
		*) fail "That is not a pairing link. It starts with quant://link?" ;;
	esac
	case "$link" in
		*\'*) fail "The link holds a quote character and can't be typed safely." ;;
	esac
	need_device
	# The phone's shell would read & and ? itself, so the link goes in quotes.
	"$ADB" shell "input text '$link'"
	say "Typed the link. Tap Connect on the phone."
}

grant() {
	need_device
	local permission
	for permission in ACCESS_COARSE_LOCATION ACCESS_FINE_LOCATION ACCESS_BACKGROUND_LOCATION POST_NOTIFICATIONS; do
		"$ADB" shell pm grant "$PACKAGE" "android.permission.$permission" 2>/dev/null ||
			say "note: $permission could not be granted from here on this Android version."
	done
	"$ADB" shell appops set "$PACKAGE" GET_USAGE_STATS allow ||
		say "note: Usage Access could not be granted from here."
	"$ADB" shell dumpsys deviceidle whitelist "+$PACKAGE" >/dev/null ||
		say "note: the battery exemption could not be granted from here."
	say "Granted location, notifications, Usage Access and the battery exemption. Reopen the app to see them."
}

status() {
	need_device
	local version pid
	version="$("$ADB" shell dumpsys package "$PACKAGE" | sed -n 's/^ *versionName=//p' | head -n 1 | tr -d '\r')"
	if [ -z "$version" ]; then
		say "Quant is not installed on $ANDROID_SERIAL."
		return
	fi
	say "Installed: $PACKAGE $version on $ANDROID_SERIAL"
	pid="$("$ADB" shell pidof -s "$PACKAGE" | tr -d '\r' || true)"
	say "Running: ${pid:-no}"
	say "Granted permissions:"
	"$ADB" shell dumpsys package "$PACKAGE" | tr -d '\r' |
		sed -n 's/^ *\(android\.permission\.[A-Z_]*\): granted=true.*/  \1/p' | sort -u
	say "Usage Access: $("$ADB" shell appops get "$PACKAGE" GET_USAGE_STATS | tr -d '\r' | head -n 1)"
	say "Background service:"
	"$ADB" shell dumpsys activity services "$PACKAGE" | tr -d '\r' |
		sed -n 's/^ *\* ServiceRecord{[^ ]* [^ ]* \([^}]*\)}.*/  \1/p; s/^ *\(isForeground=[^ ]*\).*/  \1/p' | head -n 6
}

screenshot() {
	local file="${1:-quant-phone.png}"
	need_device
	"$ADB" exec-out screencap -p >"$file"
	say "Saved $file"
}

doctor() {
	local ok=0
	find_sdk
	check() {
		if "$@" >/dev/null 2>&1; then
			say "ok       $CHECKING"
		else
			say "missing  $CHECKING"
			ok=1
		fi
	}
	CHECKING="cargo (https://rustup.rs)" check command -v cargo
	CHECKING="leptoskit CLI at $(pinned_revision | cut -c1-8) (cargo install --git https://github.com/leptoskit/leptoskit --rev $(pinned_revision) --locked leptoskit_cli)" check command -v leptoskit
	CHECKING="Android SDK (ANDROID_HOME=${ANDROID_HOME:-${ANDROID_SDK_ROOT:-unset}})" check test -n "${ANDROID_HOME:-${ANDROID_SDK_ROOT:-}}"
	CHECKING="Android NDK (Android Studio, SDK Manager, SDK Tools, NDK)" check test -n "${ANDROID_NDK_HOME:-$(ls -d "${ANDROID_HOME:-${ANDROID_SDK_ROOT:-/nonexistent}}"/ndk/* 2>/dev/null | head -n 1)}"
	CHECKING="Java 17 or later, for Gradle" check command -v java
	CHECKING="perl and make, to build OpenSSL for the phone" check sh -c 'command -v perl && command -v make'
	CHECKING="adb" check sh -c 'command -v adb || test -x "${ANDROID_HOME:-${ANDROID_SDK_ROOT:-/nonexistent}}/platform-tools/adb"'
	if (need_device) >/dev/null 2>&1; then
		need_device
		say "ok       phone $ANDROID_SERIAL ($("$ADB" shell getprop ro.product.model | tr -d '\r'), Android $("$ADB" shell getprop ro.build.version.release | tr -d '\r'), $("$ADB" shell getprop ro.product.cpu.abi | tr -d '\r'))"
	else
		say "missing  a phone attached with USB debugging allowed"
		ok=1
	fi
	return "$ok"
}

while getopts ":s:h" option; do
	case "$option" in
		s) export ANDROID_SERIAL="$OPTARG" ;;
		h)
			usage
			exit 0
			;;
		*) fail "Unknown option -$OPTARG. Try \`scripts/android-device.sh help\`." ;;
	esac
done
shift $((OPTIND - 1))

command="${1:-run}"
[ "$#" -gt 0 ] && shift

case "$command" in
	run)
		build
		install
		open_app
		;;
	build) build ;;
	install) install "$@" ;;
	open) open_app ;;
	stop)
		need_device
		"$ADB" shell am force-stop "$PACKAGE"
		;;
	logs) logs ;;
	usb) usb "$@" ;;
	link) type_link "$@" ;;
	grant) grant ;;
	status) status ;;
	screenshot) screenshot "$@" ;;
	uninstall)
		need_device
		"$ADB" uninstall "$PACKAGE"
		;;
	doctor) doctor ;;
	help | --help) usage ;;
	*) fail "Unknown command $command. Try \`scripts/android-device.sh help\`." ;;
esac
