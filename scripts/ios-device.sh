#!/usr/bin/env bash
# Build the Quant phone app and run it in an iOS simulator or on an iPhone.
#
#   scripts/ios-device.sh            build, install and open the app
#   scripts/ios-device.sh doctor     check the tools, the simulator or the iPhone
#   scripts/ios-device.sh help       every command
#
# It needs a Mac with Xcode. Without -s it uses the one booted simulator or
# the one attached iPhone, and boots an iPhone simulator when there is
# neither. An iPhone also needs QUANT_IOS_TEAM, the team the app is signed
# for. See docs/phone.md.
#
# Installing, opening, stopping, screenshots, recent logs and the gesture
# check go through `leptoskit device`, the same controller the Android script
# and agents use. simctl and devicectl are called directly only for what that
# controller has no step for: the clipboard, location, permissions, the
# installed state and following the log.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
BUNDLE="dev.leptoskit.quant"
PRODUCTS="$ROOT/.leptoskit/ios/build/Build/Products"
PORT="${QUANT_PORT:-7717}"
# Chosen by choose_device: ios-simulator or ios-device, and the UDID.
KIND=""
UDID="${QUANT_IOS_DEVICE:-}"

say() { printf '%s\n' "$*"; }
fail() {
	printf 'error: %s\n' "$*" >&2
	exit 1
}

usage() {
	cat <<EOF
Usage: scripts/ios-device.sh [-s UDID] [COMMAND]

Commands:
  run               Build, install and open the app (the default).
  build             Build the app for the chosen simulator or iPhone.
  install [APP]     Install the app just built, or the .app given.
  open              Open the installed app.
  stop              Stop the app.
  logs              Follow the app's log (simulator). Stop with Ctrl-C.
  recent [LINES]    Print the app's most recent log lines (simulator,
                    default 200).
  sanity            Install, open, and run a tap, long press, pan and
                    pinch through an Appium XCUITest session, then take a
                    screenshot.
  devices           List what \`leptoskit device\` can see.
  device ARGS...    Run any \`leptoskit device\` command against this
                    simulator or iPhone, such as:
                    device tap --app $BUNDLE --locator-text Connect
                    (steps that find a control need an Appium server).
  boot [UDID]       Boot a simulator: the one given, or an iPhone one.
  link [LINK]       Put a pairing link on the clipboard to paste into the
                    app. Without LINK, reads the output of \`quant link\`
                    from standard input.
  location LAT,LON  Move the simulator to a place, or \`location clear\`.
  grant             Allow location all the time in the simulator, without
                    tapping through the prompts. For testing.
  status            What is installed and running.
  screenshot [FILE] Save what the simulator shows (default quant-iphone.png).
  uninstall         Remove the app and everything it stored.
  doctor            Check the tools and the simulator or iPhone.

Options:
  -s UDID           The simulator or iPhone to use, as \`devices\` lists it.
                    QUANT_IOS_DEVICE does the same.

Environment:
  QUANT_IOS_TEAM    The Apple developer team ID an iPhone build is signed
                    for (Xcode, Settings, Accounts). Simulators need none.

Pair with a simulator, which shares this Mac's network:
  quant service install --tcp 127.0.0.1:$PORT
  quant link --host 127.0.0.1 | scripts/ios-device.sh link
EOF
}

need_xcode() {
	[ "$(uname -s)" = "Darwin" ] || fail "iOS builds need a Mac with Xcode."
	command -v xcodebuild >/dev/null 2>&1 || fail "xcodebuild was not found. Install Xcode from the App Store and run \`sudo xcode-select -s /Applications/Xcode.app\`."
}

# The Leptoskit revision the workspace pins, so the CLI matches the crates.
pinned_revision() {
	sed -n 's/^leptoskit = .*rev = "\([0-9a-f]\{40\}\)".*/\1/p' "$ROOT/Cargo.toml" | head -n 1
}

need_cli() {
	if ! command -v leptoskit >/dev/null 2>&1; then
		fail "The leptoskit CLI was not found. Install the pinned one:
  cargo install --git https://github.com/leptoskit/leptoskit --rev $(pinned_revision) --locked leptoskit_cli"
	fi
}

# Every simulator and iPhone \`leptoskit device list\` sees, one per line:
# kind, id, state and name, separated by tabs.
inventory() {
	need_cli
	(cd "$ROOT" && leptoskit device list --json) | python3 -c '
import json, sys
for device in json.load(sys.stdin).get("devices", []):
    if device["kind"] in ("ios_simulator", "ios_device"):
        print("\t".join([device["kind"], device["id"], device["state"], device["name"]]))
'
}

# Set KIND and UDID: the device -s named, else the one attached iPhone or
# booted simulator, else a newly booted iPhone simulator.
choose_device() {
	[ -z "$KIND" ] || return 0
	need_xcode
	local devices chosen
	devices="$(inventory)"
	if [ -n "$UDID" ]; then
		chosen="$(printf '%s\n' "$devices" | awk -F'\t' -v id="$UDID" '$2 == id')"
		[ -n "$chosen" ] || fail "No simulator or iPhone has the id $UDID. \`scripts/ios-device.sh devices\` lists them."
	else
		chosen="$(printf '%s\n' "$devices" | awk -F'\t' '$1 == "ios_device" || ($1 == "ios_simulator" && $3 == "Booted")')"
		if [ "$(printf '%s' "$chosen" | grep -c .)" -gt 1 ]; then
			printf '%s\n' "$chosen" | awk -F'\t' '{ print "  " $2 "  " $4 " (" $1 ", " $3 ")" }' >&2
			fail "More than one simulator or iPhone is ready. Choose one with -s UDID."
		fi
		if [ -z "$chosen" ]; then
			# The last iPhone simulator listed is on the newest runtime.
			chosen="$(printf '%s\n' "$devices" | awk -F'\t' '$1 == "ios_simulator" && $4 ~ /^iPhone/' | tail -n 1)"
			[ -n "$chosen" ] || fail "There is no iPhone simulator. Add one in Xcode (Window, Devices and Simulators), or attach an iPhone."
			say "Booting the $(printf '%s' "$chosen" | cut -f4) simulator."
			(cd "$ROOT" && leptoskit device boot "$(printf '%s' "$chosen" | cut -f2)") >/dev/null
			open -a Simulator >/dev/null 2>&1 || true
		fi
	fi
	UDID="$(printf '%s' "$chosen" | cut -f2)"
	case "$(printf '%s' "$chosen" | cut -f1)" in
		ios_simulator) KIND="ios-simulator" ;;
		*) KIND="ios-device" ;;
	esac
}

simulator() { [ "$KIND" = "ios-simulator" ]; }

need_simulator() {
	choose_device
	simulator || fail "$1 works only in a simulator. $2"
}

# Run one \`leptoskit device\` command against the chosen simulator or iPhone.
# The CLI writes what it captures under target/leptoskit-control and prints
# a JSON outcome, kept in OUTCOME.
control() {
	local action="$1"
	shift
	need_cli
	choose_device
	if ! OUTCOME="$(cd "$ROOT" && leptoskit device "$action" --kind "$KIND" --device "$UDID" "$@" 2>&1)"; then
		local error
		error="$(printf '%s\n' "$OUTCOME" | sed -n 's/^  "error": "\(.*\)",\{0,1\}$/\1/p' | head -n 1)"
		fail "${error:-$OUTCOME}"
	fi
}

# The file the last step saved, as an absolute path.
artifact() {
	local path
	path="$(printf '%s\n' "$OUTCOME" | sed -n 's/^ *"artifact": "\(.*\)",\{0,1\}$/\1/p' | head -n 1)"
	[ -n "$path" ] || fail "leptoskit device saved nothing: $OUTCOME"
	printf '%s\n' "$ROOT/$path"
}

built_app() {
	if simulator; then
		printf '%s\n' "$PRODUCTS/Debug-iphonesimulator/App.app"
	else
		printf '%s\n' "$PRODUCTS/Debug-iphoneos/App.app"
	fi
}

build() {
	choose_device
	command -v cargo >/dev/null 2>&1 || fail "cargo was not found. Install Rust from https://rustup.rs."
	need_cli
	local rust_target
	if ! simulator; then
		rust_target="aarch64-apple-ios"
	elif [ "$(uname -m)" = "x86_64" ]; then
		rust_target="x86_64-apple-ios"
	else
		rust_target="aarch64-apple-ios-sim"
	fi
	rustup target add "$rust_target" wasm32-unknown-unknown >/dev/null
	if simulator; then
		# Signed ad hoc, which simulators accept and the keychain needs.
		(cd "$ROOT" && leptoskit --app-target ios native build ios-simulator)
	else
		local team="${QUANT_IOS_TEAM:-}"
		[ -n "$team" ] || fail "Set QUANT_IOS_TEAM to the Apple developer team ID to sign for. Xcode shows it under Settings, Accounts; a free Apple account has one too."
		# Builds the Rust host and the guest, and writes the Xcode project.
		# Its own Xcode build is unsigned, which an iPhone refuses.
		(cd "$ROOT" && leptoskit --app-target ios build)
		local container=(-project .leptoskit/ios/Host.xcodeproj)
		[ -d "$ROOT/.leptoskit/ios/Host.xcworkspace" ] && container=(-workspace .leptoskit/ios/Host.xcworkspace)
		# A nix or devenv shell's LD is not a linker Xcode can drive.
		(cd "$ROOT" && env -u LD -u LDPLUSPLUS xcodebuild -quiet "${container[@]}" \
			-scheme App -configuration Debug -destination "id=$UDID" \
			-derivedDataPath .leptoskit/ios/build \
			-allowProvisioningUpdates -allowProvisioningDeviceRegistration \
			CODE_SIGN_STYLE=Automatic DEVELOPMENT_TEAM="$team" build)
	fi
	[ -d "$(built_app)" ] || fail "The build finished without writing $(built_app)."
	say "Built $(built_app)"
}

install() {
	choose_device
	local app="${1:-$(built_app)}"
	[ -e "$app" ] || fail "There is no app at $app. Run \`scripts/ios-device.sh build\` first."
	# An absolute path, because the controller runs from the repository root.
	app="$(cd "$(dirname "$app")" && pwd)/$(basename "$app")"
	control install --path "$app"
	say "Installed $app on $UDID."
}

open_app() {
	control open --app "$BUNDLE"
	say "Opened Quant on $UDID."
}

recent() {
	local lines="${1:-200}"
	need_simulator "Reading the log" "For an iPhone, open Console.app, choose the iPhone and search for App."
	# The controller reads the whole system log, so ask for more than is
	# wanted and keep the app's own lines. The executable is called App.
	control logs --tail 20000
	grep -E '[[:space:]]App\[' "$(artifact)" | tail -n "$lines" || say "Quant has logged nothing recently."
}

logs() {
	need_simulator "Following the log" "For an iPhone, open Console.app, choose the iPhone and search for App."
	xcrun simctl spawn "$UDID" log stream --style compact --predicate 'process == "App"'
}

sanity() {
	choose_device
	if [ -d "$(built_app)" ]; then
		control sanity --app "$BUNDLE" --path "$(built_app)"
	else
		control sanity --app "$BUNDLE"
	fi
	printf '%s\n' "$OUTCOME"
}

copy_link() {
	local link="${1:-}"
	if [ -z "$link" ]; then
		[ -t 0 ] && fail "Give the link, or pipe \`quant link\` into this command."
		link="$(grep -o 'quant://link[^[:space:]]*' | head -n 1 || true)"
	fi
	case "$link" in
		quant://link\?*) ;;
		*) fail "That is not a pairing link. It starts with quant://link?" ;;
	esac
	choose_device
	if simulator; then
		printf '%s' "$link" | xcrun simctl pbcopy "$UDID"
		say "The link is on the simulator's clipboard. Tap the link field, paste, then tap Connect."
	else
		printf '%s' "$link" | pbcopy
		say "The link is on this Mac's clipboard. With Handoff on for both, paste it into the link field on the iPhone, then tap Connect."
		say "The iPhone reaches the vault over the network, so make the link with your Mac's address (\`quant link\` without --host 127.0.0.1)."
	fi
}

move() {
	local place="${1:-}"
	need_simulator "Moving the phone" "An iPhone goes where you take it."
	case "$place" in
		clear)
			xcrun simctl location "$UDID" clear
			say "The simulator's location is no longer set."
			;;
		*,*)
			xcrun simctl location "$UDID" set "$place"
			say "The simulator is now at $place."
			;;
		*) fail "Give a place as LAT,LON, such as 51.5072,-0.1276, or \`clear\`." ;;
	esac
}

grant() {
	need_simulator "Granting from here" "On an iPhone, tap the buttons in the app; iOS asks you itself."
	xcrun simctl privacy "$UDID" grant location-always "$BUNDLE"
	say "Allowed location all the time. Reopen the app to see it."
}

status() {
	choose_device
	if simulator; then
		if ! xcrun simctl get_app_container "$UDID" "$BUNDLE" >/dev/null 2>&1; then
			say "Quant is not installed on $UDID."
			return
		fi
		say "Installed: $BUNDLE on $UDID"
		if xcrun simctl spawn "$UDID" launchctl list 2>/dev/null | grep -q "UIKitApplication:$BUNDLE"; then
			say "Running: yes"
		else
			say "Running: no"
		fi
		say "Data: $(xcrun simctl get_app_container "$UDID" "$BUNDLE" data)/Library/Application Support"
	else
		xcrun devicectl device info apps --device "$UDID" --bundle-id "$BUNDLE"
		xcrun devicectl device info processes --device "$UDID" | grep -E '/App\.app/' || say "Running: no"
	fi
}

screenshot() {
	local file="${1:-quant-iphone.png}"
	control screenshot --label quant-iphone
	cp "$(artifact)" "$file"
	say "Saved $file"
}

uninstall() {
	choose_device
	if simulator; then
		xcrun simctl uninstall "$UDID" "$BUNDLE"
	else
		xcrun devicectl device uninstall app --device "$UDID" "$BUNDLE"
	fi
	say "Removed Quant and what it stored from $UDID."
}

doctor() {
	local ok=0
	check() {
		if "$@" >/dev/null 2>&1; then
			say "ok       $CHECKING"
		else
			say "missing  $CHECKING"
			ok=1
		fi
	}
	CHECKING="a Mac" check test "$(uname -s)" = Darwin
	CHECKING="Xcode (App Store, then sudo xcode-select -s /Applications/Xcode.app)" check xcodebuild -version
	CHECKING="python3, which reads the device list (Xcode's command line tools)" check command -v python3
	CHECKING="cargo (https://rustup.rs)" check command -v cargo
	CHECKING="leptoskit CLI at $(pinned_revision | cut -c1-8) (cargo install --git https://github.com/leptoskit/leptoskit --rev $(pinned_revision) --locked leptoskit_cli)" check command -v leptoskit
	CHECKING="perl and make, to build OpenSSL for iOS" check sh -c 'command -v perl && command -v make'
	if [ "$ok" -ne 0 ]; then
		return "$ok"
	fi
	local devices
	devices="$(inventory)"
	if [ -z "$devices" ]; then
		say "missing  a simulator or an iPhone (Xcode, Window, Devices and Simulators)"
		return 1
	fi
	printf '%s\n' "$devices" | awk -F'\t' '$1 == "ios_device" || $3 == "Booted" { print "ok       " $4 " " $2 " (" $1 ", " $3 ")" }'
	if printf '%s\n' "$devices" | grep -q '^ios_device'; then
		CHECKING="QUANT_IOS_TEAM, the team an iPhone build is signed for" check test -n "${QUANT_IOS_TEAM:-}"
	fi
	return "$ok"
}

while getopts ":s:h" option; do
	case "$option" in
		s) UDID="$OPTARG" ;;
		h)
			usage
			exit 0
			;;
		*) fail "Unknown option -$OPTARG. Try \`scripts/ios-device.sh help\`." ;;
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
		control stop-app --identifier "$BUNDLE"
		say "Stopped Quant on $UDID."
		;;
	logs) logs ;;
	recent) recent "$@" ;;
	sanity) sanity ;;
	devices)
		need_cli
		(cd "$ROOT" && leptoskit device list)
		;;
	device)
		[ "$#" -gt 0 ] || fail "Give a \`leptoskit device\` command, such as: device source --app $BUNDLE"
		control "$@"
		printf '%s\n' "$OUTCOME"
		;;
	boot)
		need_cli
		if [ "$#" -gt 0 ]; then
			(cd "$ROOT" && leptoskit device boot "$1")
		else
			UDID=""
			choose_device
			say "Using $UDID."
		fi
		;;
	link) copy_link "$@" ;;
	location) move "$@" ;;
	grant) grant ;;
	status) status ;;
	screenshot) screenshot "$@" ;;
	uninstall) uninstall ;;
	doctor) doctor ;;
	help | --help) usage ;;
	*) fail "Unknown command $command. Try \`scripts/ios-device.sh help\`." ;;
esac
