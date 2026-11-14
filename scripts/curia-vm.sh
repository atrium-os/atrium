#!/bin/sh
# Curia's Network page live (docs/spec/curia.md §5 C1) in the FreeBSD VM,
# against real Wi-Fi: atrium-net and aura-mlme/aura-keys as the Aura gate
# leaves them, atrium-netconf, headless frescod + forum-wm + forum-bar, and
# Curia as `appui`. Frames go to scratch/curia-*.png.
#
#   sh scripts/aura-sim-gate.sh vms && sh scripts/aura-sim-gate.sh bar   # Wi-Fi and the UI up
#   sh scripts/curia-vm.sh shot                 # start Curia, one frame
#   sh scripts/curia-vm.sh click X Y [name]     # a click, then a frame
#   sh scripts/curia-vm.sh type TEXT [name]     # keys (a-z, space, Enter as \n), then a frame
#   sh scripts/curia-vm.sh frame [name]
#   sh scripts/curia-vm.sh status               # atrium-netctl status + list
set -eu
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
T=aarch64-unknown-freebsd
S="$HOME/atrium-vm/sysroot"
V="$ROOT/scripts/vssh"
K="-i $HOME/.ssh/fresco_bsd_ed25519 -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o LogLevel=ERROR"
Sx=/tmp/st
D=/opt/bar-net
O="$ROOT/scratch"; mkdir -p "$O"
X="-C link-arg=--sysroot=$S -C link-arg=-L$S/lib -C link-arg=-L$S/usr/lib -C link-arg=--dynamic-linker=/libexec/ld-elf.so.1 -C link-arg=$S/usr/lib/Scrt1.o -C link-arg=$S/usr/lib/crti.o -C link-arg=$S/usr/lib/crtbeginS.o -C link-arg=-lc -C link-arg=$S/usr/lib/crtendS.o -C link-arg=$S/usr/lib/crtn.o"
inj() { "$V" "printf '$1\n' | nc -N -U $Sx/inject.sock"; }
frame() {
    sleep "${GAP:-20}"
    "$V" "cp $Sx/screen.png $Sx/f.png"
    scp $K -P 2222 -q root@localhost:$Sx/f.png "$O/curia-$1.png"
    echo "frame: $O/curia-$1.png"
}
case "${1:-}" in
shot)
    (cd "$ROOT/curia" && RUSTFLAGS="$X" cargo build -q --release --target "$T" --bin curia)
    "$V" "pkill -x curia; true"
    scp $K -P 2222 -q "$ROOT/curia/target/$T/release/curia" "root@localhost:$D/"
    E2="FRESCO_SOCKET=$Sx/fresco.sock FRESCO_WM_SOCKET=$Sx/wm.sock FORUM_CTL_SOCKET=$Sx/ctl.sock"
    "$V" "cd $Sx && env $E2 CURIA_NET_DIR=/tmp/aura/sockets daemon -f -u appui -o $Sx/curia.log $D/curia </dev/null"
    GAP=${GAP:-60} frame start ;;
click)
    inj "MOVE $2 $3"; sleep 2; inj "BTN 1 1"; sleep 1; inj "BTN 1 0"
    frame "${4:-click}" ;;
type)
    s=$2; out=""
    while [ -n "$s" ]; do
        c=$(printf '%s' "$s" | cut -c1); s=$(printf '%s' "$s" | cut -c2-)
        case "$c" in
            [a-z]) u=$(( $(printf '%d' "'$c") - 97 + 4 )) ;;
            " ") u=44 ;;
            "|") u=40 ;;   # Enter
            *) echo "type: only a-z, space and | (Enter)"; exit 2 ;;
        esac
        out="${out}KEY $u 1\nKEY $u 0\n"
    done
    "$V" "printf '$out' | nc -N -U $Sx/inject.sock"
    frame "${3:-typed}" ;;
frame)
    GAP=${GAP:-2} frame "${2:-now}" ;;
status)
    "$V" "/tmp/aura/atrium-netctl --sock /tmp/aura/net.sock status; /tmp/aura/atrium-netctl --sock /tmp/aura/net.sock list; tail -n 5 $Sx/curia.log" ;;
*)
    sed -n 2,14p "$0"; exit 2 ;;
esac
