#!/bin/sh
# Invoked by crypta-gp-packet-check.py --vm. Uses an already running guest.
# A test jail and native tun are sufficient for this packet gate. The
# harness drives the engine's control and Arca descriptors.
set -eu
ROOT=$(cd "$(dirname "$0")/.." && pwd)
case "$1" in
    arm64) target=aarch64-unknown-freebsd; headers="$ROOT/sysroot" ;;
    amd64) target=x86_64-unknown-freebsd; headers="$ROOT/sysroot-amd64" ;;
    *) exit 2 ;;
esac
port=$2
pin=$3
# Scratch worktrees use the sysroot paths from .cargo/config.toml.
if [ ! -d "$headers/usr/include" ]; then
    headers="$HOME/src/bsd/$(basename "$headers")"
fi
(
    cd "$ROOT/crypta"
    tool_suffix=$(printf %s "$target" | tr - _)
    llvm=${CRYPTA_LLVM_BIN:-/opt/homebrew/opt/llvm/bin}
    env "CC_$tool_suffix=$llvm/clang" "AR_$tool_suffix=$llvm/llvm-ar" \
        "CFLAGS_$tool_suffix=--sysroot=$headers" \
        cargo build --offline --target "$target" -p crypta-engine --bin atrium-crypta-ac --example ac_harness
)
V="$ROOT/scripts/vssh"
guest=$(sh "$V" 'mktemp -d /tmp/crypta-gp-XXXXXXXX')
case "$guest" in
    /tmp/crypta-gp-????????) ;;
    *) echo "FreeBSD guest did not return a temporary directory" >&2; exit 1 ;;
esac
cleanup() {
    sh "$V" "rm -f '$guest/atrium-crypta-ac' '$guest/ac_harness'; rmdir '$guest'" || true
}
trap cleanup EXIT
bin="$ROOT/crypta/target/$target/debug"
scp -i "$HOME/.ssh/fresco_bsd_ed25519" -P 2222 -o StrictHostKeyChecking=no \
    -o UserKnownHostsFile=/dev/null -o LogLevel=ERROR \
    "$bin/atrium-crypta-ac" "$bin/examples/ac_harness" "root@localhost:$guest/"
sh "$V" "sh -s -- '$guest' '$port' '$pin'" <<'GUEST'
set -eu
guest=$1
port=$2
pin=$3
name=${guest##*/}
jail_id=
tun=
cleanup() {
    [ -z "$jail_id" ] || jail -r "$jail_id"
    [ -z "$tun" ] || ifconfig "$tun" destroy
}
trap cleanup EXIT
chmod 755 "$guest"
tun=$(ifconfig tun create)
ifconfig "$tun" inet 10.77.0.2 10.77.0.1 netmask 255.255.255.255 up
jail_id=$(jail -i -c "name=$name" path=/ "host.hostname=$name" ip4=inherit ip6=disable persist)
HARNESS_REBIND=1 "$guest/ac_harness" "$guest/atrium-crypta-ac" 10.0.2.2 "10.0.2.2:$port" /dev/null \
    --gp --pin "$pin" --native-tun "/dev/$tun" --jid "$jail_id" --ping 10.77.0.1 \
    username=alice 'password=@held:a&+ b'
GUEST
