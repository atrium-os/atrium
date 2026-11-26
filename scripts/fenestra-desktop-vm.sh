#!/bin/sh
# Fenestra R2 VM gate, first part (docs/spec/fenestra.md §3.2, §19 R2): a
# whole-desktop session from a real frescod in the cloud VM to fenestra-view
# --headless in the container, over QUIC on a forwarded UDP port.
#
# In the guest: frescod in split mode (each client in its own renderer
# process, here unconfined) with the fenestra link on, an app (Officina's
# atrium-sheets on a small sheet), fenestrad (no network) and its session. The
# renderers write the scene streams; the broker relays them; fenestra-session
# sends them on without parsing them. The viewer draws each window and
# composes the desktop from the host's layout.
#
# Checks: the viewer gets windows and a layout; every window's interior on
# the viewer's desktop is pixel-equal to frescod's own screen, after a click
# and typing sent from the viewer changed it, and again after more typing
# sent once the viewer had reconnected; an idle screen
# sends no scene bytes; a reconnect resumes (the session's log: nothing
# resent whole); no stream errors; the session outlives its viewer.
#
#   sh scripts/cloud-vm.sh up      (with NETDEV_EXTRA=hostfwd=udp:127.0.0.1:24300-:24300)
#   sh scripts/fenestra-desktop-vm.sh
set -u
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
. "$ROOT/scripts/lib/gate.sh"
gate_begin fenestra-desktop 12
set -- env
. "$ROOT/scripts/cloud-vm.sh" >/dev/null
set --
KEY="$HOME/.ssh/fresco_bsd_ed25519"
SSHOPT="-i $KEY -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o LogLevel=ERROR"
g() { ssh $SSHOPT -o ConnectTimeout=30 -p "${VM_SSH_PORT:-2222}" root@localhost "$@"; }
put() { scp $SSHOPT -P "${VM_SSH_PORT:-2222}" -q "$1" "root@localhost:$2"; }
get() { scp $SSHOPT -P "${VM_SSH_PORT:-2222}" -q "root@localhost:$1" "$2"; }
PORT="${FENESTRA_PORT:-24300}"
pgrep -fa qemu-system | grep -q "hostfwd=udp:127.0.0.1:$PORT-:$PORT" || gate_abort "the VM has no UDP $PORT forward (NETDEV_EXTRA=hostfwd=udp:127.0.0.1:$PORT-:$PORT sh scripts/cloud-vm.sh up)"
g true || gate_abort "the VM does not answer ssh"

echo "=== build"
S="$HOME/atrium-vm/sysroot"
export CC_aarch64_unknown_freebsd=clang CFLAGS_aarch64_unknown_freebsd="--target=aarch64-unknown-freebsd --sysroot=$S"
export AR_aarch64_unknown_freebsd="$(command -v llvm-ar || ls /usr/bin/llvm-ar* 2>/dev/null | head -1)"
xb() { (cd "$ROOT/$1" && RUSTFLAGS="$CROSS_RUSTFLAGS" cargo build -q --release --target "$TARGET" $2) || gate_abort "$1 did not cross-build"; }
xb frescod "--bin frescod --bin fresco-renderer"
xb fenestrad ""
xb fenestra-session "--bin fenestra-session"
xb atrium-sheets "--bin atrium-sheets"
(cd "$ROOT/fenestra-view" && cargo build -q --release) || gate_abort "fenestra-view did not build"
VIEW="$ROOT/fenestra-view/target/release/fenestra-view"
T="$TARGET/release"
BINS="$ROOT/frescod/target/$T/frescod $ROOT/frescod/target/$T/fresco-renderer $ROOT/fenestrad/target/$T/fenestrad $ROOT/fenestra-session/target/$T/fenestra-session $ROOT/atrium-sheets/target/$T/atrium-sheets"

D=/opt/fenestra-desk
G=/tmp/fendesk
g "pkill -x fenestrad; pkill -x fenestra-session; pkill -x atrium-sheets; pkill -x frescod; sleep 1; rm -rf $D $G; mkdir -p $D $G/mint; chmod 700 $G/mint; true"
for b in $BINS; do
    put "$b" "$D/"
    [ "$(sha256sum "$b" | cut -d' ' -f1)" = "$(g "sha256 -q $D/$(basename "$b")")" ] || gate_abort "hash mismatch for $(basename "$b")"
done
g "mkdir -p /usr/local/share/fonts/ibm-plex"
for f in "$ROOT"/fonts/ibm-plex/*.ttf "$ROOT"/fonts/phosphor/*.ttf; do
    g "test -f /usr/local/share/fonts/ibm-plex/$(basename "$f")" || put "$f" /usr/local/share/fonts/ibm-plex/
done

echo "=== host"
g "cd $G && env FRESCOD_SPLIT=1 FRESCOD_RENDERER_LAUNCHER=unconfined FRESCOD_RENDERER_BIN=$D/fresco-renderer \
    FRESCOD_HEADLESS_PNG=$G/screen FRESCOD_SOCK=$G/fresco.sock FRESCOD_WM_SOCK=$G/wm.sock \
    FRESCOD_FENESTRA_SOCK=$G/fenestra.sock daemon -f -o $G/frescod.log $D/frescod </dev/null"
i=0; while [ $i -lt 90 ] && ! g "test -S $G/fresco.sock && test -S $G/fenestra.sock"; do sleep 2; i=$((i+1)); done
g "test -S $G/fenestra.sock" || { g "tail -20 $G/frescod.log"; gate_abort "frescod did not open the fenestra link"; }
g "chmod 666 $G/fresco.sock"
CSV=$(mktemp)
{ echo "Region,Q1,Q2,Q3,Q4,Total"; i=2
  for r in North South East West Central Islands Coast Hills; do
      echo "$r,$((i*1130+17)),$((i*1207+3)),$((i*990+411)),$((i*1402+9)),=SUM(B$i:E$i)"; i=$((i+1)); done; } >"$CSV"
put "$CSV" "$G/sales.csv"; rm -f "$CSV"
g "cd $G && env HOME=$G FRESCO_SOCKET=$G/fresco.sock daemon -f -o $G/app.log $D/atrium-sheets $G/sales.csv </dev/null"
sleep "${WAIT:-40}"

echo "=== session"
W=$(mktemp -d "${TMPDIR:-/tmp}/fenestra-desktop.XXXXXX")
mkdir -p "$W/mint"
RUN="${RUN_SECS:-150}"
# The viewer's input, seconds from its start (it connects within ~10 s): a
# click on a cell and "123" Enter; it reconnects at 70 s; then right arrow
# and "45" Enter; nothing from 120 s.
cat >"$W/input.txt" <<'EOF'
45 MOVE 300 200
45.2 BTN 1 1
45.3 BTN 1 0
48 KEY 30 1
48.1 KEY 30 0
49 KEY 31 1
49.1 KEY 31 0
50 KEY 32 1
50.1 KEY 32 0
51 KEY 40 1
51.1 KEY 40 0
85 KEY 79 1
85.1 KEY 79 0
86 KEY 33 1
86.1 KEY 33 0
87 KEY 34 1
87.1 KEY 34 0
88 KEY 40 1
88.1 KEY 40 0
EOF
"$VIEW" --headless --input "$W/input.txt" --mint-dir "$W/mint" --out "$W/view" --run-secs "$RUN" --connect "127.0.0.1:$PORT" \
    --reconnect-after 70 --idle-after 120 >"$W/view.log" 2>&1 &
VP=$!
i=0; while [ $i -lt 50 ] && [ ! -s "$W/mint/viewer.pub" ]; do sleep 0.2; i=$((i+1)); done
put "$W/mint/viewer.pub" "$G/mint/viewer.pub" || gate_abort "could not give the guest the viewer key"
g "cd $G && daemon -f -o $G/fenestrad.log $D/fenestrad --frescod $G/fenestra.sock --session $D/fenestra-session --mint-dir $G/mint --bind 0.0.0.0:$PORT </dev/null"
i=0; while [ $i -lt 60 ] && ! g "test -s $G/mint/mint"; do sleep 1; i=$((i+1)); done
get "$G/mint/mint" "$W/mint/mint.tmp" && mv "$W/mint/mint.tmp" "$W/mint/mint" || gate_abort "no mint from the session"

sleep 15
g "cp $G/screen.png $G/before.png"
sleep 50
g "cp $G/screen.png $G/middle.png"
sleep 45
g "cp $G/screen.png $G/after.png"
wait $VP
get "$G/after.png" "$W/host.png"
get "$G/before.png" "$W/host-before.png"
get "$G/middle.png" "$W/host-middle.png"
g "cat $G/fenestrad.log" >"$W/fenestrad.log" 2>&1
g "tail -50 $G/frescod.log" >"$W/frescod.log" 2>&1
cat "$W/view.log"
V="$W/view/viewer.txt"
cat "$V"

nw=$(grep -c '^window ' "$V")
check "the viewer drew windows" yes "$([ "$nw" -gt 0 ] && echo yes || echo "no ($nw)")"
check "the viewer has the host's layout" yes "$(grep -q '^desktop ' "$V" && echo yes || echo no)"
check "no stream errors" 0 "$(awk '$1 == "errors" { print $2 }' "$V")"
check "nothing sent while idle" 0 "$(awk '$1 == "idle_bytes" { print $2 }' "$V")"
# Each placed window's interior equal on both screens. Left out: 4 px at its
# edges (frescod's focus ring) and the top STRIP px of the screen (frescod's
# strip): trusted chrome frescod draws over every window, not streamed yet.
STRIP=$(sed -n 's/^pub const STRIP_PT: *[a-z0-9]* *= *\([0-9]*\).*/\1/p' "$ROOT/frescod/src/chrome.rs")
[ -n "$STRIP" ] || gate_abort "no STRIP_PT in frescod/src/chrome.rs"
compare() { python3 -I - "$1" "$W/view/desktop.png" "$V" "$STRIP" <<'EOF'
import sys
from PIL import Image
h = Image.open(sys.argv[1]).convert("RGB")
v = Image.open(sys.argv[2]).convert("RGB")
placed = [l.split()[1:] for l in open(sys.argv[3]) if l.startswith("placed ")]
if h.size != v.size or not placed:
    print(f"sizes {h.size} {v.size}, {len(placed)} placed"); sys.exit()
bad = []
for p in placed:
    i, x, y, w, ht = map(int, p)
    box = (max(x + 4, 0), max(y + 4, int(sys.argv[4])), min(x + w - 4, h.size[0]), min(y + ht - 4, h.size[1]))
    if box[2] <= box[0] or box[3] <= box[1]:
        continue
    a, b = h.crop(box).tobytes(), v.crop(box).tobytes()
    n = sum(1 for k in range(0, len(a), 3) if a[k:k+3] != b[k:k+3])
    if n:
        bad.append(f"window {i}: {n} of {len(a)//3} px differ")
print("equal" if not bad else "; ".join(bad))
EOF
}
cmp=$(compare "$W/host.png")
check "every window equal to frescod's own screen" equal "$cmp"
# The positive control: the screen before the last typing is not what the
# viewer ended with, so the comparison sees a typed cell.
check "the typing reached the viewer (its desktop differs from the screen before it)" yes "$(compare "$W/host-middle.png" | grep -q differ && echo yes || echo no)"
check "the viewer's input changed the host's screen" yes "$(cmp -s "$W/host-middle.png" "$W/host-before.png" && echo no || echo yes)"
check "input sent after the reconnect changed it again" yes "$(cmp -s "$W/host.png" "$W/host-middle.png" && echo no || echo yes)"
ins=$(grep 'viewer [0-9] gone' "$W/fenestrad.log" | sed 's/.*inputs \([0-9]*\) dropped \([0-9]*\).*/\1:\2/' | tr '\n' ' ')
check "the session passed on every input event, on both connections, none dropped" "11:0 8:0 " "$ins"
grep 'fenestra-session' "$W/fenestrad.log" | tail -5
check "the viewer reconnected" yes "$(grep -c 'connected to' "$W/view.log" | awk '{ print ($1 >= 2) ? "yes" : "no (" $1 ")" }')"
r2=$(grep 'viewer 2 gone' "$W/fenestrad.log" | sed 's/.*resumed \([0-9]*\) resent \([0-9]*\).*/\1 \2/')
case "$r2" in
    "") bad "the second connection resumed: no record of it in the session's log" ;;
    "0 "*) bad "the second connection resumed nothing ($r2)" ;;
    *" 0") ok "the second connection resumed every held window (resumed/resent: $r2)" ;;
    *) bad "held windows were sent whole again (resumed/resent: $r2)" ;;
esac
check "the session is still up after its viewer left" yes "$(g "pgrep -x fenestra-session >/dev/null" && echo yes || echo "no: $(tail -2 "$W/fenestrad.log")")"
g "pkill -x fenestrad; pkill -x fenestra-session; pkill -x atrium-sheets; pkill -x frescod; true"
echo "  evidence in $W"
gate_end
