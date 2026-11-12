#!/bin/sh
# Pergola's controls, Canvas and DocumentView on a live headless frescod in
# the FreeBSD dev VM; the composited frame comes back as a PNG.
#
#   sh scripts/cloud-vm.sh up
#   sh scripts/pergola-showcase-vm.sh [light|dark|dark-dialog|light-dialog] [out.png]
set -eu
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
MODE="${1:-light}"
OUT="${2:-$ROOT/scratch/pergola-showcase-$MODE.png}"
set -- env
. "$ROOT/scripts/cloud-vm.sh" >/dev/null
set --
KEY="$HOME/.ssh/fresco_bsd_ed25519"
SSHOPT="-i $KEY -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o LogLevel=ERROR"
g() { ssh $SSHOPT -o ConnectTimeout=30 -p 2222 root@localhost "$@"; }
put() { scp $SSHOPT -P 2222 -q "$1" "root@localhost:$2"; }
get() { scp $SSHOPT -P 2222 -q "root@localhost:$1" "$2"; }

echo "=== cross-build"
xb() { (cd "$ROOT/$1" && RUSTFLAGS="$CROSS_RUSTFLAGS" cargo build -q --release --target "$TARGET" $2); }
xb frescod "--bin frescod"
xb pergola-document "--example showcase"

D=/opt/pergola-showcase
g "pkill -x frescod; pkill -x showcase; sleep 1; rm -rf $D /tmp/ps; mkdir -p $D /tmp/ps; chmod 777 /tmp/ps; true"
for b in "$ROOT/frescod/target/$TARGET/release/frescod" "$ROOT/pergola-document/target/$TARGET/release/examples/showcase"; do
    put "$b" "$D/"
    [ "$(sha256sum "$b" | cut -d' ' -f1)" = "$(g "sha256 -q $D/$(basename "$b")")" ] || { echo "ABORT: hash mismatch for $(basename "$b")"; exit 1; }
done

# frescod's server-side text (Text nodes) reads the system faces from here.
g "mkdir -p /usr/local/share/fonts/ibm-plex"
for f in "$ROOT"/fonts/ibm-plex/*.ttf "$ROOT"/fonts/phosphor/*.ttf; do
    g "test -f /usr/local/share/fonts/ibm-plex/$(basename "$f")" || put "$f" /usr/local/share/fonts/ibm-plex/
done

echo "=== run"
g "cd /tmp/ps && env FRESCOD_SPLIT=0 FRESCOD_HEADLESS_PNG=/tmp/ps/screen FRESCOD_SOCK=/tmp/ps/fresco.sock \
    FRESCOD_WM_SOCK=/tmp/ps/wm.sock FRESCOD_INPUT_SOCK=/tmp/ps/inject.sock daemon -f -o /tmp/ps/frescod.log $D/frescod </dev/null"
i=0; while [ $i -lt 60 ] && ! g "test -S /tmp/ps/fresco.sock"; do sleep 2; i=$((i+1)); done
arg="$(echo "$MODE" | tr - " ")"
S=${SECS:-90}
g "cd /tmp/ps && env FRESCO_SOCKET=/tmp/ps/fresco.sock SHOWCASE_SECS=$S daemon -f -o /tmp/ps/showcase.log $D/showcase $arg </dev/null"
# CLICKS="x,y x,y": presses through frescod's input injector once the
# window is up (a menu opens only from a press: frescod's pop-up rule).
if [ -n "${CLICKS:-}" ]; then
    sleep ${CLICK_AFTER:-40}
    for c in $CLICKS; do
        x=${c%,*}; y=${c#*,}
        g "printf 'MOVE $x $y\\nBTN 1 1\\n' | nc -N -U /tmp/ps/inject.sock; sleep 1; printf 'BTN 1 0\\n' | nc -N -U /tmp/ps/inject.sock"
        sleep 8
    done
    sleep 15
    S=0
fi
# The frame while the window is still up (frescod rewrites the PNG on damage).
[ "$S" -gt 15 ] && sleep $((S - 15))
g "cp /tmp/ps/screen.png /tmp/ps/shot.png; ls -la /tmp/ps/; tail -5 /tmp/ps/frescod.log"
mkdir -p "$(dirname "$OUT")"
get /tmp/ps/shot.png "$OUT" && echo "=== $OUT"
sleep 5
g "cat /tmp/ps/showcase.log"
