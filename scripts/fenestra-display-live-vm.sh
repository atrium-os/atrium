#!/bin/sh
# Fenestra VM gate for a live seat on the display path (docs/spec/fenestra.md
# §11.4, decision 3 opt-out; frescod's viewed.rs live indicator and the span
# direct-scanout guard in main.rs). The headless twin is
# scripts/fenestra-live-vm.sh; the blanked seat on the display path is
# scripts/fenestra-display-vm.sh, whose setup this follows.
#
# frescod (split mode, unconfined renderers, FRESCOD_FENESTRA_LIVE=1) on
# /dev/atrium-display0 with three 1080p gpusim monitors, forum-wm, fenestrad
# and atrium-span-probe zoomed over a span group of the three (gap 40).
# Checks: with no session the span's members scan the window out directly
# (positive control) and show it, no indicator; while a viewer is on frescod
# stops direct scanout and composes, and every member shows frescod's
# indicator at its strip's right end, the red "viewed from ..." pill and a
# blue Disconnect (the composed sRGB colours of the chrome's linear ones);
# a click on monitor 0's Disconnect through frescod's input ends the
# session, the indicator is gone and direct scanout comes back; frescod and
# forum-wm live, no panic.
#
# Needs the Mac VM booted with gpusim, the viewer's port forwarded and
# hw.pci.honor_msi_blacklist=0 (see scripts/fenestra-display-vm.sh):
#   NETDEV_EXTRA=hostfwd=udp:127.0.0.1:24300-:24300 sh scripts/run-vm.sh --gpusim
#   FEN_KMODS=DIR sh scripts/fenestra-display-live-vm.sh   # DIR: the three .ko
# Evidence (screendumps, logs) goes to $EVID, default
# ~/atrium-vm/evidence/fenestra-live-direct-<date>.
set -u
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
. "$ROOT/scripts/lib/gate.sh"
gate_begin fenestra-display-live 16

KEY="$HOME/.ssh/fresco_bsd_ed25519"
SSHOPT="-i $KEY -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o LogLevel=ERROR"
P="${VM_SSH_PORT:-2222}"
g() { ssh $SSHOPT -o ConnectTimeout=30 -p "$P" root@localhost "$@"; }
put() { scp $SSHOPT -P "$P" -q "$1" "root@localhost:$2"; }
get() { scp $SSHOPT -P "$P" -q "root@localhost:$1" "$2"; }
MON="${VM_MON:-$HOME/atrium-vm/mon.sock}"
PORT=24300
EVID="${EVID:-$HOME/atrium-vm/evidence/fenestra-live-direct-$(date +%Y%m%d-%H%M)}"
mkdir -p "$EVID" || gate_abort "cannot make $EVID"
TARGET=aarch64-unknown-freebsd
D=/opt/fenestra-display
G=/tmp/fenlive
DEV=dev.atrium_gpu_amd_display.0
# Where the span's members start on the desktop (and in the window): three
# 1920 px monitors, 40 px bezels.
OFFS=0,1960,3920

QARGS=$(ps -axww -o command | grep '[q]emu-system')
echo "$QARGS" | grep -q "device gpusim" || gate_abort "no QEMU with gpusim (run-vm.sh --gpusim)"
echo "$QARGS" | grep -q "hostfwd=udp:127.0.0.1:$PORT-:$PORT" || gate_abort "the VM has no UDP $PORT forward"
[ -S "$MON" ] || gate_abort "no QEMU monitor at $MON"
g true || gate_abort "the VM does not answer ssh"

echo "=== build"
S="$HOME/atrium-vm/sysroot"
export CC_aarch64_unknown_freebsd=clang CFLAGS_aarch64_unknown_freebsd="--target=$TARGET --sysroot=$S"
export AR_aarch64_unknown_freebsd="$(command -v llvm-ar || echo "$(brew --prefix llvm 2>/dev/null)/bin/llvm-ar")"
BLOG="$EVID/build.log"
xb() { (cd "$ROOT/$1" && cargo build -q --release --target "$TARGET" $2) >>"$BLOG" 2>&1 || { tail -20 "$BLOG"; gate_abort "$1 did not cross-build"; }; }
xb frescod "--bin frescod --bin fresco-renderer"
xb fenestrad ""
xb fenestra-session "--bin fenestra-session"
xb forum-wm "--bin forum-wm"
xb forum-ctl ""
xb atrium-test-client "--bin atrium-span-probe"
(cd "$ROOT/fenestra-view" && cargo build -q --release) >>"$BLOG" 2>&1 || gate_abort "fenestra-view did not build"
VIEW="$ROOT/fenestra-view/target/release/fenestra-view"
T="$TARGET/release"
BINS="$ROOT/frescod/target/$T/frescod $ROOT/frescod/target/$T/fresco-renderer $ROOT/fenestrad/target/$T/fenestrad
    $ROOT/fenestrad/target/$T/fenestra-shell $ROOT/fenestra-session/target/$T/fenestra-session
    $ROOT/forum-wm/target/$T/forum-wm $ROOT/forum-ctl/target/$T/forum-ctl
    $ROOT/atrium-test-client/target/$T/atrium-span-probe"

echo "=== stage"
g "pkill -x atrium-span-probe; pkill -x fenestrad; pkill -x fenestra-session; pkill -x forum-wm; pkill -x frescod; sleep 1; rm -rf $D $G; mkdir -p $D $G; true"
for b in $BINS; do
    put "$b" "$D/" || gate_abort "copy $(basename "$b")"
    [ "$(shasum -a 256 "$b" | cut -d' ' -f1)" = "$(g "sha256 -q $D/$(basename "$b")")" ] || gate_abort "hash mismatch for $(basename "$b")"
done
g "mkdir -p /usr/local/share/fonts/ibm-plex"
for f in "$ROOT"/fonts/ibm-plex/*.ttf "$ROOT"/fonts/phosphor/*.ttf; do
    g "test -f /usr/local/share/fonts/ibm-plex/$(basename "$f")" || put "$f" /usr/local/share/fonts/ibm-plex/
done

echo "=== display"
# GPU fences complete by MSI-X (see fenestra-display-vm.sh).
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
# Three 1080p monitors of one mode: the desktop one (3), its twin (5) and
# the 60 Hz VRR model (6); nothing on output 3; the display free.
g "sysctl $DEV.lease='' >/dev/null; sysctl $DEV.sim_plug=3 >/dev/null; sysctl $DEV.sim_plug_at=1:5 >/dev/null; sysctl $DEV.sim_plug_at=2:6 >/dev/null; sysctl $DEV.sim_plug_at=3:-1 >/dev/null" \
    || gate_abort "set gpusim's monitors"
sleep 3

echo "=== host"
g "echo root >$G/seat"
g "cd $G && env FRESCOD_SPLIT=1 FRESCOD_RENDERER_LAUNCHER=unconfined FRESCOD_RENDERER_BIN=$D/fresco-renderer \
    FRESCOD_SOCK=$G/fresco.sock FRESCOD_WM_SOCK=$G/wm.sock FRESCOD_FENESTRA_SOCK=$G/fenestra.sock \
    FRESCOD_SEAT=$G/seat FRESCOD_FENESTRA_LIVE=1 FRESCOD_INPUT_SOCK=$G/input.sock \
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
check "forum-ctl shows the three monitors at 1920x1080" 3 "$(grep '^output ' "$EVID/outputs-before.txt" | grep -c 1920x1080)"

# Each gpusim pipe that is scanning out, as screendumps on the host.
dump() {
    for n in 0 1 2 3; do
        rm -f "$EVID/$1-pipe$n.ppm"
        printf 'screendump %s gpusim0 %s\n' "$EVID/$1-pipe$n.ppm" "$n" | nc -w 2 -U "$MON" >/dev/null 2>&1
    done
    sleep 2
}
# The members a phase's dumps show the probe on, continuous, nothing over
# the top band's middle (span-probe-check.py).
probe_ok() {
    python3 -P "$ROOT/scripts/span-probe-check.py" "$OFFS" "$EVID/$1"-pipe*.ppm >"$EVID/$1-probe.txt" 2>&1 && echo yes || echo no
}
# The live indicator on each dump that shows a member: for each, the window
# offset it shows, the red and blue pills' pixel counts in the top 24 rows,
# whether blue ends at the strip's right end with red left of it, and the
# Disconnect's centre in that monitor's pixels. One line per member, sorted
# by offset.
pills() { python3 -P - "$OFFS" "$EVID/$1"-pipe*.ppm <<'EOF'
import sys
from PIL import Image
offs = [int(o) for o in sys.argv[1].split(',')]
near = lambda p, c: all(abs(a - b) <= 3 for a, b in zip(p, c))
RED, BLUE = (237, 108, 124), (108, 196, 255)
out = []
for path in sys.argv[2:]:
    try:
        im = Image.open(path).convert("RGB")
    except Exception:
        continue
    w, h = im.size
    if w < 64:
        continue
    px = im.load()
    r = px[4, h // 2]
    if not all(2 <= v % 16 <= 14 for v in r):
        continue
    first = (r[0] // 16) | (r[1] // 16) << 4 | (r[2] // 16) << 8
    off = next((o for o in offs if o // 8 == first), None)
    if off is None:
        continue
    reds = [(x, y) for y in range(min(h, 24)) for x in range(w) if near(px[x, y], RED)]
    blues = [(x, y) for y in range(min(h, 24)) for x in range(w) if near(px[x, y], BLUE)]
    end = "no"
    centre = "-"
    if reds and blues:
        bx0, bx1 = min(p[0] for p in blues), max(p[0] for p in blues)
        rx1 = max(p[0] for p in reds)
        if bx1 >= w - 40 and rx1 < bx0 and min(p[0] for p in reds) > w // 2:
            end = "yes"
        by = [p[1] for p in blues]
        centre = f"{(bx0 + bx1) // 2},{(min(by) + max(by)) // 2}"
    out.append((off, f"{off} red {len(reds)} blue {len(blues)} end {end} at {centre} ({path.rsplit('/', 1)[-1]})"))
for _, l in sorted(out):
    print(l)
EOF
}
# "yes" when the pills lines ($1) name all three members with both pills
# at the right end; "none" when all three show neither pill.
pills_on() { echo "$1" | awk -v n=3 '{ if ($3 > 300 && $5 > 300 && $7 == "yes") y++; if ($3 == 0 && $5 == 0) z++; c++ }
    END { if (c == n && y == n) print "yes"; else if (c == n && z == n) print "none"; else print "no (" c " members, " y " with pills)" }'; }
# The line number of frescod's first log line matching $1 after line $2
# (0: none).
after() { g "awk -v s=$2 'NR > s && /$1/ { print NR; exit }' $G/frescod.log" | grep . || echo 0; }
lines() { g "wc -l <$G/frescod.log" | tr -d ' '; }

echo "=== span: the probe zoomed over outputs 0 1 2"
g "cd $G && daemon -f -o $G/probe.log $D/atrium-span-probe $G/fresco.sock </dev/null"
sleep 15
GAME=$(g "$CTL surfaces" | sed 's/^\*/ /' | awk '$2=="Document"{id=$1} END{print id}')
echo "  probe window: ${GAME:-none}"
[ -n "$GAME" ] || { g "cat $G/probe.log; $CTL surfaces"; gate_abort "no probe window"; }
g "$CTL span 0 1 2 gap 40" >/dev/null || bad "span group refused"
g "$CTL focus $GAME; sleep 3; $CTL zoom" >/dev/null
sleep 30
g "$CTL spans; $CTL outputs" >"$EVID/spans.txt" 2>&1
L0=$(lines)
dump direct
check "no session: the members scan the probe out directly" yes \
    "$(g "grep 'frescod: span: ' $G/frescod.log | tail -1" | grep -q 'span: direct scanout' && echo yes || echo no)"
check "no session: each member shows its tile of the probe, nothing over it" yes "$(probe_ok direct)"
PD=$(pills direct); echo "$PD" | sed 's/^/  /'
check "no session: no indicator on any member" none "$(pills_on "$PD")"

echo "=== viewer (on the host, 120 s)"
g "cd $G && daemon -f -o $G/fenestrad.log $D/fenestrad --frescod $G/fenestra.sock --session $D/fenestra-session \
    --mint-sock $G/mint.sock --seat $G/seat --bind 0.0.0.0:$PORT </dev/null"
i=0; while [ $i -lt 30 ] && ! g "test -S $G/mint.sock"; do sleep 1; i=$((i+1)); done
g "test -S $G/mint.sock" || { g "cat $G/fenestrad.log"; gate_abort "fenestrad did not open its mint socket"; }
"$VIEW" --headless --ssh "ssh $SSHOPT -n -o ConnectTimeout=30 -p $P root@localhost env PATH=$D:/bin:/usr/bin FENESTRA_MINT_SOCK=$G/mint.sock" \
    --out "$EVID/view" --connect "127.0.0.1:$PORT" --output 1600x900@1.5 --run-secs 120 >"$EVID/view.log" 2>&1 &
VP=$!
sleep 25
dump viewed
ON=$(after "viewed: live seat, indicator on" "$L0")
check "the live indicator came on" yes "$([ "$ON" -gt 0 ] && echo yes || echo no)"
check "while viewed frescod composes the span (no direct scanout after the indicator)" "yes 0" \
    "$([ "$(after 'span: composed' "$L0")" -gt 0 ] && echo yes || echo no) $(g "awk -v s=$L0 'NR > s && /span: direct scanout/' $G/frescod.log | wc -l" | tr -d ' ')"
PV=$(pills viewed); echo "$PV" | sed 's/^/  /'
check "every member shows the red viewed pill and the blue Disconnect at its strip's right end" yes "$(pills_on "$PV")"
check "the desktop stays the monitors' (live seat)" 3 "$(g "$CTL outputs" | grep '^output ' | grep -c 1920x1080)"

echo "=== Disconnect: a click on monitor 0's indicator"
B=$(echo "$PV" | awk '$1 == 0 { split($9, a, ","); print a[1], a[2] }')
echo "  Disconnect at desktop ${B:-none}"
[ -n "$B" ] && g "printf 'MOVE $B\nBTN 1 1\nBTN 1 0\n' | nc -N -U $G/input.sock"
sleep 10
L1=$(lines)
dump after
check "the click ended the session" "yes yes gone" \
    "$(g "grep -q 'Disconnect pressed on the live indicator' $G/frescod.log" && echo yes || echo no) $(g "grep -q 'disconnected at the host' $G/fenestrad.log" && echo yes || echo no) $(g "pgrep -x fenestra-session >/dev/null" && echo running || echo gone)"
OFF=$(after "viewed: live seat, indicator off" "$ON")
check "the indicator went off" yes "$([ "$OFF" -gt 0 ] && echo yes || echo no)"
PA=$(pills after); echo "$PA" | sed 's/^/  /'
check "no indicator on any member after" none "$(pills_on "$PA")"
check "direct scanout came back after the indicator went off" yes \
    "$([ "$OFF" -gt 0 ] && [ "$(after 'span: direct scanout' "$OFF")" -gt 0 ] && g "grep 'frescod: span: ' $G/frescod.log | tail -1" | grep -q 'direct scanout' && echo yes || echo no)"
check "each member shows its tile of the probe again, nothing over it" yes "$(probe_ok after)"

g "pgrep -x frescod >/dev/null" && ok "frescod alive" || bad "frescod DEAD"
g "pgrep -x forum-wm >/dev/null" && ok "forum-wm alive" || bad "forum-wm DEAD"
check "no panic in frescod's log" 0 "$(g "grep -c -E 'panicked|RUST_BACKTRACE' $G/frescod.log")"

kill $VP 2>/dev/null; wait $VP 2>/dev/null
g "pkill -x atrium-span-probe; pkill -x fenestrad; pkill -x fenestra-session; pkill -x forum-wm; pkill -x frescod; sysctl $DEV.sim_plug_at=1:-1 >/dev/null; sysctl $DEV.sim_plug_at=2:-1 >/dev/null; true"
for f in frescod.log wm.log fenestrad.log probe.log; do get "$G/$f" "$EVID/$f"; done
grep -E 'viewed|span|Disconnect|panic' "$EVID/frescod.log" >"$EVID/frescod-live.txt" 2>&1
for f in "$EVID"/*.ppm; do
    [ -s "$f" ] && python3 -P -c 'import sys; from PIL import Image; Image.open(sys.argv[1]).save(sys.argv[1][:-4] + ".png")' "$f" 2>/dev/null && rm -f "$f"
done
echo "  evidence in $EVID"
gate_end
