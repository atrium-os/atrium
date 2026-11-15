#!/bin/sh
# Presenting (Officina O6d, docs/spec/officina.md §16.33) on a headless
# two-monitor frescod in the FreeBSD dev VM: atrium-slides presents a deck
# that uses every transition and build, pressing Next by itself, with the
# slideshow on one monitor and the presenter view on the other. frescod
# composes at 60 Hz while an animation runs; each finished animation logs
# the frames composed and the worst gap between them (the smoothness gate:
# no gap over one 60 Hz period, 16.7 ms plus slack, on real timing).
#
#   sh scripts/cloud-vm.sh up
#   sh scripts/slides-present-vm.sh [outdir]
#   OPEN=deck.oslides sh scripts/slides-present-vm.sh   # another deck (default: the show_deck example's)
#   PRESS_MS=4000                                      # time between presses
#   EVERY=1                                            # also keep every animation frame
#   FRESCOD_ENV="FRESCOD_RENDER_TIMES=1"               # more environment for frescod
#
# Under TCG (the cloud container) the gaps measure the emulator, not the
# product: run it on the Mac (HVF) for the 60 Hz figure.
set -eu
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
OUTDIR="${1:-$ROOT/scratch/present}"
PRESS_MS="${PRESS_MS:-4000}"
set -- env
. "$ROOT/scripts/cloud-vm.sh" >/dev/null
set --
KEY="$HOME/.ssh/fresco_bsd_ed25519"
SSHOPT="-i $KEY -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o LogLevel=ERROR"
g() { ssh $SSHOPT -o ConnectTimeout=30 -p 2222 root@localhost "$@"; }
put() { scp $SSHOPT -P 2222 -q "$1" "root@localhost:$2"; }
get() { scp $SSHOPT -P 2222 -q "root@localhost:$1" "$2"; }

mkdir -p "$OUTDIR"
if [ -z "${OPEN:-}" ]; then
    OPEN="$OUTDIR/show.oslides"
    (cd "$ROOT/atrium-slides" && cargo run -q --example show_deck -- "$OPEN")
fi

echo "=== cross-build"
xb() { (cd "$ROOT/$1" && RUSTFLAGS="$CROSS_RUSTFLAGS" cargo build -q --release --target "$TARGET" $2); }
xb frescod "--bin frescod"
xb atrium-slides "--bin atrium-slides"

D=/opt/atrium-slides
g "pkill -x frescod; pkill -x atrium-slides; sleep 1; rm -rf $D /tmp/sh; mkdir -p $D /tmp/sh/shots; chmod 777 /tmp/sh; true"
for b in "$ROOT/frescod/target/$TARGET/release/frescod" "$ROOT/atrium-slides/target/$TARGET/release/atrium-slides"; do
    put "$b" "$D/"
    [ "$(sha256sum "$b" | cut -d' ' -f1)" = "$(g "sha256 -q $D/$(basename "$b")")" ] || { echo "ABORT: hash mismatch for $(basename "$b")"; exit 1; }
done
g "mkdir -p /usr/local/share/fonts/ibm-plex"
for f in "$ROOT"/fonts/ibm-plex/*.ttf "$ROOT"/fonts/phosphor/*.ttf; do
    g "test -f /usr/local/share/fonts/ibm-plex/$(basename "$f")" || put "$f" /usr/local/share/fonts/ibm-plex/
done

SHOW="/tmp/sh/$(basename "$OPEN")"
put "$OPEN" "$SHOW"
g "cd /tmp/sh && env FRESCOD_SPLIT=0 FRESCOD_HEADLESS_PNG=/tmp/sh/screen FRESCOD_HEADLESS_OUTPUTS=1280x720,1280x720 FRESCOD_HEADLESS_HZ=60 ${EVERY:+FRESCOD_HEADLESS_EVERY_FRAME=1} ${FRESCOD_ENV:-} \
    FRESCOD_SOCK=/tmp/sh/fresco.sock FRESCOD_WM_SOCK=/tmp/sh/wm.sock FRESCOD_INPUT_SOCK=/tmp/sh/inject.sock daemon -f -o /tmp/sh/frescod.log $D/frescod </dev/null"
i=0; while [ $i -lt 90 ] && ! g "test -S /tmp/sh/fresco.sock"; do sleep 2; i=$((i+1)); done
g "test -S /tmp/sh/fresco.sock" || { echo "frescod did not listen"; g "tail -20 /tmp/sh/frescod.log"; exit 1; }

echo "=== presenting $(basename "$OPEN"), Next every $PRESS_MS ms"
g "cd /tmp/sh && env HOME=/tmp/sh FRESCO_SOCKET=/tmp/sh/fresco.sock ATRIUM_SLIDES_PRESENT=1 ATRIUM_SLIDES_PRESS_MS=$PRESS_MS ATRIUM_SLIDES_ANIM_LOG=1 \
    daemon -f -o /tmp/sh/app.log $D/atrium-slides $SHOW </dev/null"
# A copy of the screen every second while the show runs.
g "n=0; while [ \$n -lt ${SHOTS:-90} ] && pgrep -x atrium-slides >/dev/null; do sleep 1; n=\$((n+1)); \
    [ -f /tmp/sh/screen.png ] && cp /tmp/sh/screen.png /tmp/sh/shots/\$(printf %03d \$n).png; done; true"
g "pkill -x atrium-slides; sleep 1; true"
echo "=== app log"
g "cat /tmp/sh/app.log" | tee "$OUTDIR/app.log"
g "cat /tmp/sh/frescod.log" > "$OUTDIR/frescod.log"; g "tail -5 /tmp/sh/frescod.log; pkill -x frescod; true"
rm -f "$OUTDIR"/shot-*.png
for f in $(g "cd /tmp/sh/shots && ls | awk 'NR%3==0'"); do get "/tmp/sh/shots/$f" "$OUTDIR/shot-$f"; done
if [ -n "${EVERY:-}" ]; then
    rm -f "$OUTDIR"/frame-*.png
    for f in $(g "cd /tmp/sh && ls screen-*.png 2>/dev/null"); do get "/tmp/sh/$f" "$OUTDIR/frame-${f#screen-}"; done
fi
echo "shots: $(ls "$OUTDIR" | grep -c shot-), animation frames: $(ls "$OUTDIR" | grep -c frame-) in $OUTDIR"
