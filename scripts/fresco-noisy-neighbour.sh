#!/bin/sh
# Noisy-neighbour gate (officina.md §10.5; fresco-f4-split.md §F4h, §F4j):
# a split-mode frescod with three clients. `atrium-noisy victim` animates a
# small window at 60 fps; then, while it keeps going, `atrium-noisy nodes`
# sends a million nodes and `atrium-noisy fills` sends 65,536 overlapping
# full-window translucent fills it redraws every frame. frescod logs, every
# STATS seconds, how often each window's drawings reach the screen
# (FRESCOD_FRAME_STATS). Runs on the HOST against a FreeBSD VM over ssh.
#
#   sh scripts/fresco-noisy-neighbour.sh            compute renderer, headless
#   RENDERER=gpu sh scripts/fresco-noisy-neighbour.sh   gpu renderer (gpusim / R9700)
#
# Passes when:
#  - the victim's drawings reach the screen at >= MIN_RATIO of its rate alone
#    (default 0.8), and its p99 gap is <= GAP_FACTOR x alone (default 2) plus
#    one frame, with both hogs running;
#  - the nodes hog was told EV_BUDGET_REFUSED (nodes) and is still connected
#    at the end (its renderer refused, it was not killed);
#  - with RENDERER=gpu, the fills hog was told budget::RASTER_TIME;
#  - frescod is still running.
#
# Builds first (x86_64-unknown-freebsd release, CROSS_RUSTFLAGS as
# cloud-vm.sh env): frescod (frescod, fresco-renderer), atrium-test-client
# (atrium-noisy). SSH_PORT (2222) and KEY pick the VM, as the other vm-*
# checks. Under TCG (the cloud VM) the rates are slow and noisy: compare
# the two phases only, never quote them as speeds, and give the hogs
# LOADED=160 so the nodes hog reaches its limit (about 3,000 nodes/s there).
set -u
ROOT=$(cd "$(dirname "$0")/.." && pwd)
T=${TARGET:-x86_64-unknown-freebsd}
R=release
KEY=${KEY:-$HOME/.ssh/fresco_bsd_ed25519}
PORT=${SSH_PORT:-2222}
SSHOPT="-i $KEY -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o LogLevel=ERROR"
g() { ssh $SSHOPT -o ConnectTimeout=30 -p "$PORT" root@localhost "$@"; }
put() { scp $SSHOPT -P "$PORT" -q "$1" "root@localhost:$2"; }
RENDERER=${RENDERER:-compute}
ALONE=${ALONE:-40}
LOADED=${LOADED:-80}
STATS=${STATS:-10}
MIN_RATIO=${MIN_RATIO:-0.8}
GAP_FACTOR=${GAP_FACTOR:-2}
OUT=${OUT:-/tmp/noisy-neighbour}
mkdir -p "$OUT"
D=/opt/nn
S=/tmp/nn
fails=0
ok() { echo "  ok: $*"; }
bad() { echo "  FAIL: $*"; fails=$((fails + 1)); }

g "pkill -x frescod; pkill -f atrium-noisy; sleep 1; rm -rf $S; mkdir -p $D $S; chmod 777 $S; true"
for b in frescod/target/$T/$R/frescod frescod/target/$T/$R/fresco-renderer atrium-test-client/target/$T/$R/atrium-noisy; do
    put "$ROOT/$b" "$D/" || { echo "missing $b: build it first"; exit 2; }
done

case "$RENDERER" in
    gpu) RENV="FRESCOD_RENDERER=gpu FRESCOD_FLIP_TIMEOUT_MS=${FLIP_MS:-900000}"; HEADLESS="" ;;
    *)   RENV="FRESCOD_RENDERER=compute"; HEADLESS="FRESCOD_HEADLESS_PNG=$S/screen" ;;
esac
g "cd $S && env RUST_LOG=${RUST_LOG:-warn} $RENV $HEADLESS FRESCOD_SPLIT=1 FRESCOD_RENDERER_LAUNCHER=unconfined \
    FRESCOD_RENDERER_BIN=$D/fresco-renderer FRESCOD_APP_REGISTRY=$S/none FRESCOD_FRAME_STATS=$STATS \
    FRESCOD_SOCK=$S/fresco.sock FRESCOD_WM_SOCK=$S/wm.sock daemon -f -o $S/frescod.log $D/frescod </dev/null"
i=0; while [ $i -lt 90 ] && ! g "test -S $S/fresco.sock"; do sleep 2; i=$((i+1)); done
g "test -S $S/fresco.sock" || { echo "frescod did not start"; g "tail -n 30 $S/frescod.log"; exit 1; }

total=$((ALONE + LOADED + 20))
echo "== victim alone (${ALONE} s)"
g "cd $S && daemon -f -o $S/victim.log $D/atrium-noisy $S/fresco.sock victim $total </dev/null"
sleep "$ALONE"
g "cp $S/frescod.log $S/alone.log"
echo "== victim with the nodes and fills hogs (${LOADED} s)"
g "cd $S && daemon -f -o $S/nodes.log $D/atrium-noisy $S/fresco.sock nodes $((LOADED + 10)) </dev/null"
g "cd $S && daemon -f -o $S/fills.log $D/atrium-noisy $S/fresco.sock fills $((LOADED + 10)) </dev/null"
sleep "$LOADED"
g "cp $S/frescod.log $S/loaded.log"
sleep 20
for f in frescod alone loaded victim nodes fills; do
    g "cat $S/$f.log" > "$OUT/$f.log" 2>/dev/null
done

# The victim's stats lines: "window N "noisy-victim": R new/s gap p99 G ms ...".
victim() { # log, first line to read from -> "rate p99" averaged over its lines
    tail -n +"$2" "$1" | grep 'frescod: stats:' | grep -o '"noisy-victim": [0-9.]* new/s gap p99 [0-9.]* ms' \
        | awk '{r += $2; g += $6; n++} END { if (n) printf "%.2f %.2f %d\n", r / n, g / n, n; else print "0 0 0" }'
}
skip_alone=$(grep -c 'frescod: stats:' "$OUT/alone.log")
# Leave out the first stats line of each phase (the victim starting, the hogs uploading).
a=$(grep -n 'frescod: stats:' "$OUT/alone.log" | sed -n 2p | cut -d: -f1)
l=$(grep -n 'frescod: stats:' "$OUT/loaded.log" | sed -n "$((skip_alone + 2))p" | cut -d: -f1)
set -- $(head -n "$(wc -l < "$OUT/alone.log")" "$OUT/alone.log" > "$OUT/alone.only"; victim "$OUT/alone.only" "${a:-1}")
r0=$1 g0=$2 n0=$3
set -- $(victim "$OUT/loaded.log" "${l:-1}")
r1=$1 g1=$2 n1=$3
echo "  victim alone:  $r0 new/s, gap p99 $g0 ms ($n0 samples)"
echo "  victim loaded: $r1 new/s, gap p99 $g1 ms ($n1 samples)"
[ "$n0" -gt 0 ] && [ "$n1" -gt 0 ] || bad "no victim stats (is FRESCOD_FRAME_STATS in this frescod?)"
awk -v a="$r0" -v b="$r1" -v m="$MIN_RATIO" 'BEGIN { exit !(a > 0 && b >= a * m) }' \
    && ok "victim kept its rate ($r1 vs $r0 new/s, min ratio $MIN_RATIO)" || bad "victim rate fell: $r1 vs $r0 new/s"
awk -v a="$g0" -v b="$g1" -v f="$GAP_FACTOR" -v r="$r0" 'BEGIN { exit !(b <= a * f + (r > 0 ? 1000 / r : 0)) }' \
    && ok "victim gaps stayed short (p99 $g1 vs $g0 ms)" || bad "victim gaps grew: p99 $g1 vs $g0 ms"
grep -q 'resource 1 ' "$OUT/nodes.log" && ok "nodes hog was told it is over its node budget" || bad "nodes hog never told (budget)"
grep -q 'still connected' "$OUT/nodes.log" && ok "nodes hog still connected at the end" || bad "nodes hog lost its connection"
grep -q 'still connected' "$OUT/fills.log" && ok "fills hog still connected at the end" || bad "fills hog lost its connection"
if [ "$RENDERER" = gpu ]; then
    grep -q 'resource 5 ' "$OUT/fills.log" && ok "fills hog was told its frames cost too much (RASTER_TIME)" || bad "fills hog never refused for raster cost"
fi
g "pgrep -x frescod >/dev/null" && ok "frescod still running" || bad "frescod died"
grep -E 'panicked|killed|renderer.*exit' "$OUT/frescod.log" | head -5
g "pkill -f atrium-noisy; pkill -x frescod; true"
echo "noisy-neighbour ($RENDERER): $fails failed"
[ "$fails" -eq 0 ]
