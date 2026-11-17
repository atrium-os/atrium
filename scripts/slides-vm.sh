#!/bin/sh
# atrium-slides live on a headless frescod in the FreeBSD dev VM (Officina O6b,
# docs/spec/officina.md §16.35): a screenshot of a deck.
#
#   sh scripts/cloud-vm.sh up
#   sh scripts/slides-vm.sh [out.png]
#   OPEN=scratch/sample.oslides sh scripts/slides-vm.sh   # the file to show (default: that one)
#   SLIDE=2                                               # start on that slide
#   ALSO="a.oslides b.oslides"                           # more files copied next to it (into /tmp/sh)
#
# Under TCG (the cloud container) timings are not product numbers.
set -eu
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
OUT="${1:-$ROOT/scratch/atrium-slides.png}"
OPEN="${OPEN:-$ROOT/scratch/sample.oslides}"
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
xb atrium-slides "--bin atrium-slides"

D=/opt/atrium-slides
g "pkill -x frescod; pkill -x atrium-slides; sleep 1; rm -rf $D /tmp/sh; mkdir -p $D /tmp/sh; chmod 777 /tmp/sh; true"
for b in "$ROOT/frescod/target/$TARGET/release/frescod" "$ROOT/atrium-slides/target/$TARGET/release/atrium-slides"; do
    put "$b" "$D/"
    [ "$(sha256sum "$b" | cut -d' ' -f1)" = "$(g "sha256 -q $D/$(basename "$b")")" ] || { echo "ABORT: hash mismatch for $(basename "$b")"; exit 1; }
done
g "mkdir -p /usr/local/share/fonts/ibm-plex"
for f in "$ROOT"/fonts/ibm-plex/*.ttf "$ROOT"/fonts/phosphor/*.ttf; do
    g "test -f /usr/local/share/fonts/ibm-plex/$(basename "$f")" || put "$f" /usr/local/share/fonts/ibm-plex/
done
. "$ROOT/scripts/vm-fonts.sh"; vm_install_fonts

start_frescod() {
    g "cd /tmp/sh && env FRESCOD_SPLIT=0 FRESCOD_HEADLESS_PNG=/tmp/sh/screen FRESCOD_SOCK=/tmp/sh/fresco.sock \
        FRESCOD_WM_SOCK=/tmp/sh/wm.sock FRESCOD_INPUT_SOCK=/tmp/sh/inject.sock daemon -f -o /tmp/sh/frescod.log $D/frescod </dev/null"
    i=0; while [ $i -lt 90 ] && ! g "test -S /tmp/sh/fresco.sock"; do sleep 2; i=$((i+1)); done
    g "test -S /tmp/sh/fresco.sock" || { echo "frescod did not listen"; g "tail -20 /tmp/sh/frescod.log"; exit 1; }
}

SHOW="/tmp/sh/$(basename "$OPEN")"
put "$OPEN" "$SHOW"
for f in ${ALSO:-}; do put "$f" "/tmp/sh/$(basename "$f")"; done
echo "=== the app on $(basename "$OPEN")"
start_frescod
g "cd /tmp/sh && env HOME=/tmp/sh FRESCO_SOCKET=/tmp/sh/fresco.sock ${SLIDE:+ATRIUM_SLIDES_SLIDE=$SLIDE} daemon -f -o /tmp/sh/app.log $D/atrium-slides $SHOW </dev/null"
sleep ${SHOT_AFTER:-60}
mkdir -p "$(dirname "$OUT")"
g "ls -t /tmp/sh/screen* 2>/dev/null | head -1" | { read -r f && [ -n "$f" ] && get "$f" "$OUT" && echo "screenshot: $OUT"; } || echo "no screenshot"
g "pkill -x atrium-slides; sleep 1; cat /tmp/sh/app.log; pkill -x frescod; true"
