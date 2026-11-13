#!/bin/sh
# Sigillum S5 P4 (docs/spec/sigillum.md §16.5): the vendor PKCS#11 module under
# sigillum-p11, against the real token, on a Linux amd64 guest it is passed through to
# (qemu: -device qemu-xhci -device usb-host,vendorid=0x338c,productid=0x0032). Never in CI.
#
#   sh scripts/sigillum-p4.sh build                  # sigillum-p11 + sigillum-p11-usb, focal libstdc++
#   sh scripts/sigillum-p4.sh list MODULE.so         # the token's certificates (no PIN)
#   sh scripts/sigillum-p4.sh sign MODULE.so KEYID ecdsa|pkcs1|pss
#                                                    # one signature; the PIN is typed at the prompt
#
# MODULE.so is libInnaITPKCS11Driver.so from the user's innait-dsc-driver 4.0.7.1 package,
# checked against its sha256 (SIGILLUM_P4_MODULE_SHA256) before it is opened.
set -eu
R=$(cd "$(dirname "$0")/.." && pwd)
H="$R/hospes"
LSC="$HOME/.cache/hospes-focal-libstdcxx/root/usr/lib/x86_64-linux-gnu/libstdc++.so.6.0.28"
HOST="$H/target/x86_64-unknown-linux-gnu/release/sigillum-p11"
USB="$H/target/release/sigillum-p11-usb"

check_module() {
    want="${SIGILLUM_P4_MODULE_SHA256:-}"
    [ -n "$want" ] || { echo "set SIGILLUM_P4_MODULE_SHA256 to the module's known sha256" >&2; exit 2; }
    got=$(sha256sum < "$1" | cut -d' ' -f1)
    [ "$got" = "$want" ] || { echo "module sha256 $got is not the expected one" >&2; exit 1; }
}

run_usb() {
    # pcscd would hold the CCID interface.
    if command -v systemctl >/dev/null && systemctl is-active -q pcscd.socket 2>/dev/null; then
        sudo systemctl stop pcscd.socket pcscd.service
    fi
    # usbfs nodes are root's; only the bridge needs it, the host drops nothing it has.
    sudo "$USB" "$HOST" "$@"
}

case "${1:-}" in
    build)
        sh "$R/scripts/hospes-build.sh" focal >/dev/null
        sh "$R/scripts/hospes-build.sh" p11 x86_64-unknown-linux-gnu >/dev/null
        (cd "$H" && cargo build --release -p sigillum-p11-usb >&2)
        echo "built: $HOST $USB" ;;
    list) check_module "$2"; run_usb "$2" "$LSC" list ;;
    sign) check_module "$2"; run_usb "$2" "$LSC" sign "$3" "$4" ;;
    *) sed -n '2,13p' "$0"; exit 2 ;;
esac
