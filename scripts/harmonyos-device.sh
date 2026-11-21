#!/bin/sh
# Build and install Quant on a Huawei Watch or HarmonyOS phone via hdc.
#
#   scripts/harmonyos-device.sh doctor   # check OHOS_NDK_HOME, hvigorw, hdc
#   scripts/harmonyos-device.sh build    # build Huawei Watch HAP target
#   scripts/harmonyos-device.sh          # build and install via hdc
set -eu
cd "$(dirname "$0")/.."

cmd=${1:-run}
target=${QUANT_HARMONYOS_TARGET:-harmonyos-watch}
hap="platforms/harmonyos/entry/build/default/outputs/default/entry-default-signed.hap"

case "$cmd" in
	doctor)
		echo "OHOS_NDK_HOME=${OHOS_NDK_HOME:-<unset>}"
		command -v hvigorw >/dev/null 2>&1 && command -v hvigorw || echo "hvigorw not on PATH"
		command -v hdc >/dev/null 2>&1 && hdc list targets || echo "hdc not on PATH"
		;;
	build)
		rustup target add wasm32-unknown-unknown aarch64-unknown-linux-ohos >/dev/null 2>&1 || true
		leptoskit --app-target "$target" build
		;;
	install)
		path=${2:-$hap}
		hdc ${QUANT_HDC_TARGET:+-t "$QUANT_HDC_TARGET"} install -r "$path"
		;;
	run)
		"$0" build
		"$0" install "$hap"
		;;
	*)
		echo "Usage: $0 [doctor|build|install [HAP]|run]" >&2
		exit 1
		;;
esac
