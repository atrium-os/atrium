#!/bin/sh
# Fenestra VM gate for a host with monitors of its own, on the display path
# (docs/spec/fenestra.md §19b, "A host with monitors of its own"; frescod's
# viewed.rs and main.rs's display loop). The headless twin is
# scripts/fenestra-seat-vm.sh; here frescod scans out to gpusim's monitors.
#
# frescod (split mode, unconfined renderers) on /dev/atrium-display0 with
# forum-wm, a stub of ostiarius's lock socket, fenestrad, and a headless
# viewer asking for a 1600x900 output at 1.5. Checks: while viewed the
# desktop takes the viewer's output (log, forum-ctl), monitor 0 scans out
# frescod's viewed screen (field, words, Disconnect) and nothing of the
# desktop; a monitor plugged while viewed shows the viewed screen without
# the button and the desktop stays at the viewer's size; Return at the seat
# ends the session, frescod asks the lock socket, keeps the viewed screen
# until the lock file says the seat is locked, and only then gives the
# desktop back to both monitors; frescod and forum-wm live, no panic.
#
# Needs the Mac VM booted with gpusim, the viewer's port forwarded and
# hw.pci.honor_msi_blacklist=0, and the atrium_gpu_amd modules attached
# (/dev/atrium-display0):
#   NETDEV_EXTRA=hostfwd=udp:127.0.0.1:24300-:24300 sh scripts/run-vm.sh --gpusim
#   FEN_KMODS=DIR sh scripts/fenestra-display-vm.sh   # DIR: the three .ko
# FEN_KMODS loads atrium_gpu_amd_pci, _display and atrium_gpu_amd from DIR
# and force-attaches the gpusim device when the display is not there yet;
# the modules must be built against the guest's running kernel. The viewer
# runs on the Mac, minting over ssh into the guest and connecting through
# the UDP forward.
# Evidence (screendumps, logs) goes to $EVID, default
# ~/atrium-vm/evidence/fenestra-display-<date>.
set -u
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
. "$ROOT/scripts/lib/gate.sh"
gate_begin fenestra-display 15

KEY="$HOME/.ssh/fresco_bsd_ed25519"
SSHOPT="-i $KEY -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o LogLevel=ERROR"
P="${VM_SSH_PORT:-2222}"
g() { ssh $SSHOPT -o ConnectTimeout=30 -p "$P" root@localhost "$@"; }
put() { scp $SSHOPT -P "$P" -q "$1" "root@localhost:$2"; }
get() { scp $SSHOPT -P "$P" -q "root@localhost:$1" "$2"; }
MON="${VM_MON:-$HOME/atrium-vm/mon.sock}"
PORT=24300
EVID="${EVID:-$HOME/atrium-vm/evidence/fenestra-display-$(date +%Y%m%d-%H%M)}"
mkdir -p "$EVID" || gate_abort "cannot make $EVID"
TARGET=aarch64-unknown-freebsd
D=/opt/fenestra-display
G=/tmp/fendisp
DEV=dev.atrium_gpu_amd_display.0

QARGS=$(ps -axww -o command | grep '[q]emu-system')
echo "$QARGS" | grep -q "device gpusim" || gate_abort "no QEMU with gpusim (run-vm.sh --gpusim)"
echo "$QARGS" | grep -q "hostfwd=udp:127.0.0.1:$PORT-:$PORT" || gate_abort "the VM has no UDP $PORT forward"
[ -S "$MON" ] || gate_abort "no QEMU monitor at $MON"
g true || gate_abort "the VM does not answer ssh"

echo "=== build"
S="$HOME/atrium-vm/sysroot"
export CC_aarch64_unknown_freebsd=clang CFLAGS_aarch64_unknown_freebsd="--target=$TARGET --sysroot=$S"
export AR_aarch64_unknown_freebsd="$(command -v llvm-ar || echo "$(brew --prefix llvm 2>/dev/null)/bin/llvm-ar")"
xb() { (cd "$ROOT/$1" && cargo build -q --release --target "$TARGET" $2) || gate_abort "$1 did not cross-build"; }
xb frescod "--bin frescod --bin fresco-renderer"
xb fenestrad ""
xb fenestra-session "--bin fenestra-session"
xb forum-wm "--bin forum-wm"
xb forum-ctl ""
(cd "$ROOT/fenestra-view" && cargo build -q --release) || gate_abort "fenestra-view did not build"
VIEW="$ROOT/fenestra-view/target/release/fenestra-view"
T="$TARGET/release"
BINS="$ROOT/frescod/target/$T/frescod $ROOT/frescod/target/$T/fresco-renderer $ROOT/fenestrad/target/$T/fenestrad
    $ROOT/fenestrad/target/$T/fenestra-shell $ROOT/fenestra-session/target/$T/fenestra-session
    $ROOT/forum-wm/target/$T/forum-wm $ROOT/forum-ctl/target/$T/forum-ctl"

echo "=== stage"
g "pkill -x fenestrad; pkill -x fenestra-session; pkill -x forum-wm; pkill -x frescod; pkill -f 'nc -N -lU $G'; sleep 1; rm -rf $D $G; mkdir -p $D $G; true"
for b in $BINS; do
    put "$b" "$D/" || gate_abort "copy $(basename "$b")"
    [ "$(shasum -a 256 "$b" | cut -d' ' -f1)" = "$(g "sha256 -q $D/$(basename "$b")")" ] || gate_abort "hash mismatch for $(basename "$b")"
done
g "mkdir -p /usr/local/share/fonts/ibm-plex"
for f in "$ROOT"/fonts/ibm-plex/*.ttf "$ROOT"/fonts/phosphor/*.ttf; do
    g "test -f /usr/local/share/fonts/ibm-plex/$(basename "$f")" || put "$f" /usr/local/share/fonts/ibm-plex/
done

echo "=== display"
# GPU fences complete by MSI-X; without it frescod's frame waits time out
# (atrium_gpu_amd_pci "MSI-X unavailable — children poll"). QEMU's arm64
# virt host bridge is on FreeBSD's MSI blacklist, so the guest must boot
# with hw.pci.honor_msi_blacklist=0 (loader.conf, or nextboot -e once).
[ "$(g "sysctl -n hw.pci.honor_msi_blacklist")" = 0 ] \
    || gate_abort "the guest honours the MSI blacklist: boot it with hw.pci.honor_msi_blacklist=0 (nextboot -e hw.pci.honor_msi_blacklist=0)"
if ! g "test -c /dev/atrium-display0"; then
    [ -n "${FEN_KMODS:-}" ] || gate_abort "no /dev/atrium-display0 in the guest (FEN_KMODS=DIR with the three modules)"
    g "mkdir -p /root/fendisp-kmod"
    for k in atrium_gpu_amd_pci atrium_gpu_amd_display atrium_gpu_amd; do
        put "$(find "$FEN_KMODS" -name "$k.ko" | head -1)" /root/fendisp-kmod/ || gate_abort "copy $k.ko"
    done
    g "cd /root/fendisp-kmod && { kldstat -q -n atrium_gpu_amd_pci.ko || kldload ./atrium_gpu_amd_pci.ko; } && { pciconf -l | grep -q '^atrium_gpu_amd_pci' || devctl set driver -f \$(pciconf -l | sed -n 's/^[^@]*@\\(pci[0-9]*:[0-9]*:[0-9]*:[0-9]*\\).*vendor=0x1002 device=0x7550.*/\\1/p') atrium_gpu_amd_pci; } && sleep 3 && { kldstat -q -n atrium_gpu_amd_display.ko || kldload ./atrium_gpu_amd_display.ko; } && { kldstat -q -n atrium_gpu_amd.ko || kldload ./atrium_gpu_amd.ko; }" \
        || gate_abort "the GPU modules did not load"
    g "test -c /dev/atrium-display0" || { g "dmesg | tail -20"; gate_abort "no /dev/atrium-display0 after loading"; }
fi
# One desktop monitor on output 0 and nothing on output 1; the display free.
g "sysctl $DEV.lease='' >/dev/null; sysctl $DEV.sim_plug_at=1:-1 >/dev/null; sysctl $DEV.sim_plug=3 >/dev/null" || gate_abort "set gpusim's monitors"
sleep 3

# ostiarius's lock socket, stubbed: it answers ok once, then writes the lock
# file naming the seat's user, as ostiarius would, 8 s later, so the gate
# can see frescod hold the viewed screen until the seat is locked.
echo "=== host"
STUB=$(mktemp)
cat >"$STUB" <<STUBEOF
printf '{"status":"ok","active":"root"}\n' | nc -N -lU $G/lock.sock >$G/lock-req.txt
date +%s >$G/lock-asked
sleep 8
echo root >$G/session-locked
date +%s >$G/lock-written
STUBEOF
put "$STUB" "$G/lockstub.sh"; rm -f "$STUB"
g "echo root >$G/seat; daemon -f sh $G/lockstub.sh"
g "cd $G && env FRESCOD_SPLIT=1 FRESCOD_RENDERER_LAUNCHER=unconfined FRESCOD_RENDERER_BIN=$D/fresco-renderer \
    FRESCOD_SOCK=$G/fresco.sock FRESCOD_WM_SOCK=$G/wm.sock FRESCOD_FENESTRA_SOCK=$G/fenestra.sock \
    FRESCOD_SEAT=$G/seat FRESCOD_LOCK_SOCK=$G/lock.sock FRESCOD_INPUT_SOCK=$G/input.sock \
    daemon -f -o $G/frescod.log $D/frescod </dev/null"
i=0; while [ $i -lt 60 ] && ! g "test -S $G/fresco.sock && test -S $G/fenestra.sock"; do sleep 2; i=$((i+1)); done
g "test -S $G/fenestra.sock" || { g "tail -20 $G/frescod.log"; gate_abort "frescod did not open the fenestra link"; }
g "chmod 666 $G/fresco.sock $G/wm.sock; cd $G && env FRESCO_SOCKET=$G/fresco.sock FRESCO_WM_SOCKET=$G/wm.sock FORUM_CTL_SOCKET=$G/ctl.sock \
    FORUM_DISPLAYS=$G/forum-displays.state daemon -f -o $G/wm.log $D/forum-wm </dev/null"
i=0; while [ $i -lt 30 ] && ! g "test -S $G/ctl.sock"; do sleep 1; i=$((i+1)); done
g "pgrep -x forum-wm >/dev/null" || { g "tail -20 $G/wm.log"; gate_abort "forum-wm did not start"; }
CTL="env FORUM_CTL_SOCKET=$G/ctl.sock $D/forum-ctl"
sleep 10
g "$CTL outputs" >"$EVID/outputs-before.txt" 2>&1
g "cd $G && daemon -f -o $G/fenestrad.log $D/fenestrad --frescod $G/fenestra.sock --session $D/fenestra-session \
    --mint-sock $G/mint.sock --seat $G/seat --bind 0.0.0.0:$PORT </dev/null"
i=0; while [ $i -lt 30 ] && ! g "test -S $G/mint.sock"; do sleep 1; i=$((i+1)); done
g "test -S $G/mint.sock" || { g "cat $G/fenestrad.log"; gate_abort "fenestrad did not open its mint socket"; }

# Each gpusim pipe that is scanning out, as screendumps on the host.
dump() {
    for n in 0 1 2 3; do
        rm -f "$EVID/$1-pipe$n.ppm"
        printf 'screendump %s gpusim0 %s\n' "$EVID/$1-pipe$n.ppm" "$n" | nc -w 2 -U "$MON" >/dev/null 2>&1
    done
    sleep 2
}
# What a screendump shows: frescod's viewed field, its words, its button.
look() { python3 -P - "$1" <<'EOF'
import sys
from PIL import Image
try:
    im = Image.open(sys.argv[1]).convert("RGB")
except Exception as e:
    print(f"none ({e.__class__.__name__})"); sys.exit()
d = im.getdata(); n = len(d)
field = sum(1 for p in d if p == (15, 18, 23)) / n
text = sum(1 for p in d if min(p) > 200)
button = sum(1 for p in d if p == (38, 140, 255))
print(f"{im.size[0]}x{im.size[1]} field {field:.3f} text {text} button {button}")
EOF
}
viewed_ok() { echo "$1" | awk '{ f = $3; t = $5; b = $7; if (f > 0.9 && t > 300) print (b > 500 ? "button" : (b == 0 ? "nobutton" : "some")); else print "no" }'; }

echo "=== viewer (on the host, 120 s)"
"$VIEW" --headless --ssh "ssh $SSHOPT -n -o ConnectTimeout=30 -p $P root@localhost env PATH=$D:/bin:/usr/bin FENESTRA_MINT_SOCK=$G/mint.sock" \
    --out "$EVID/view" --connect "127.0.0.1:$PORT" --output 1600x900@1.5 --run-secs 120 >"$EVID/view.log" 2>&1 &
VP=$!
sleep 25
g "$CTL outputs" >"$EVID/outputs-viewed.txt" 2>&1
dump viewed
V0=$(look "$EVID/viewed-pipe0.ppm")
echo "  monitor 0 while viewed: $V0"
check "while viewed frescod gives the desktop the viewer's output" yes \
    "$(g "grep -q \"viewed; the desktop takes the viewer's output: 1600x900\" $G/frescod.log" && echo yes || echo no)"
check "forum-ctl shows the one remote output at 1600x900" "1 yes" \
    "$(grep -c '^output ' "$EVID/outputs-viewed.txt") $(grep '^output ' "$EVID/outputs-viewed.txt" | grep -q 1600x900 && echo yes || echo no)"
check "monitor 0 scans out the viewed screen with its Disconnect, not the desktop" button "$(viewed_ok "$V0")"

echo "=== hotplug output 1 while viewed"
g "sysctl $DEV.sim_plug_at=1:5 >/dev/null"
sleep 25
g "$CTL outputs" >"$EVID/outputs-plugged.txt" 2>&1
dump plugged
for n in 0 1 2 3; do echo "  pipe $n: $(look "$EVID/plugged-pipe$n.ppm")"; done
check "the desktop stays at the viewer's output after the hotplug" "0 1 yes" \
    "$(g "grep -c \"the desktop is the monitors' again\" $G/frescod.log") $(grep -c '^output ' "$EVID/outputs-plugged.txt") $(grep '^output ' "$EVID/outputs-plugged.txt" | grep -q 1600x900 && echo yes || echo no)"
# The new monitor's pipe: the one lit now that was not before, or else any
# other lit pipe than monitor 0's.
NEW=""
for n in 1 2 3 0; do
    l=$(look "$EVID/plugged-pipe$n.ppm")
    [ "$n" != 0 ] && [ "$(viewed_ok "$l")" != no ] && { NEW="$n: $l"; break; }
done
check "the plugged monitor shows the viewed screen's field and words, no button" nobutton "$(viewed_ok "${NEW#*: }")"
check "monitor 0 still shows the viewed screen with its button" button "$(viewed_ok "$(look "$EVID/plugged-pipe0.ppm")")"

echo "=== Disconnect: Return at the seat"
g "printf 'KEY 40 1\nKEY 40 0\n' | nc -N -U $G/input.sock"
sleep 4
g "$CTL outputs" >"$EVID/outputs-locking.txt" 2>&1
dump locking
check "frescod asked the lock socket to lock the seat" '{"op":"lock"}' "$(g "cat $G/lock-req.txt" | tr -d '\n ')"
check "before the lock file is written the desktop is still the viewer's" "no 1 yes" \
    "$(g "test -e $G/session-locked && echo yes || echo no") $(grep -c '^output ' "$EVID/outputs-locking.txt") $(grep '^output ' "$EVID/outputs-locking.txt" | grep -q 1600x900 && echo yes || echo no)"
check "monitor 0 keeps the viewed screen (locking) until then" nobutton "$(viewed_ok "$(look "$EVID/locking-pipe0.ppm")")"
sleep 16
g "$CTL outputs" >"$EVID/outputs-locked.txt" 2>&1
dump locked
check "the desktop went back to the monitors only once the seat was locked" yes \
    "$(g "cat $G/frescod.log" | awk '/viewed: the session ended; locking the seat/ { e = NR } /viewed: the seat is locked/ { l = NR } /the desktop is the monitors. again/ { m = NR } END { print (e && l && m && e < l && l <= m) ? "yes" : "no (" e " " l " " m ")" }')"
check "forum-ctl shows both monitors again" 2 "$(grep -c '^output ' "$EVID/outputs-locked.txt")"
check "monitor 0 no longer shows the viewed screen" no "$(viewed_ok "$(look "$EVID/locked-pipe0.ppm")")"

g "pgrep -x frescod >/dev/null" && ok "frescod alive" || bad "frescod DEAD"
g "pgrep -x forum-wm >/dev/null" && ok "forum-wm alive" || bad "forum-wm DEAD"
check "no panic in frescod's log" 0 "$(g "grep -c -E 'panicked|RUST_BACKTRACE' $G/frescod.log")"

kill $VP 2>/dev/null; wait $VP 2>/dev/null
g "pkill -x fenestrad; pkill -x fenestra-session; pkill -x forum-wm; pkill -x frescod; pkill -f 'nc -N -lU $G'; sysctl $DEV.sim_plug_at=1:-1 >/dev/null; true"
for f in frescod.log wm.log fenestrad.log lock-req.txt; do get "$G/$f" "$EVID/$f"; done
g "grep -E 'viewed|desktop|monitor|output|hotplug|lock|inject|panic' $G/frescod.log" >"$EVID/frescod-viewed.txt" 2>&1
for f in "$EVID"/*.ppm; do
    [ -s "$f" ] && python3 -P -c 'import sys; from PIL import Image; Image.open(sys.argv[1]).save(sys.argv[1][:-4] + ".png")' "$f" 2>/dev/null && rm -f "$f"
done
echo "  evidence in $EVID"
gate_end
