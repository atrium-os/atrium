#!/bin/sh
# atrium-sheets live on a headless frescod in the FreeBSD dev VM (Officina O2,
# docs/spec/officina.md §16.2): a screenshot of a sheet, then the scroll gate on a
# 1,048,576-row sheet (`atrium-sheets --bench`).
#
#   sh scripts/cloud-vm.sh up
#   sh scripts/sheets-vm.sh [out.png]
#   BENCH="all 1048576 8 60" sh scripts/sheets-vm.sh    # the bench's arguments
#   OPEN=scratch/styled.osheet BENCH=none sh scripts/sheets-vm.sh   # look at a file, no bench
#
# Under TCG (the cloud container) frame times are not product numbers: this run is
# functional (the app draws, scrolls and reports). Real timing: the same steps on the
# Mac's HVF VM (scripts/run-vm.sh), or bare metal.
set -eu
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
OUT="${1:-$ROOT/scratch/atrium-sheets.png}"
BENCH="${BENCH:-all 1048576 8 60}"
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
xb atrium-sheets "--bin atrium-sheets"

D=/opt/atrium-sheets
g "pkill -x frescod; pkill -x atrium-sheets; sleep 1; rm -rf $D /tmp/sh; mkdir -p $D /tmp/sh; chmod 777 /tmp/sh; true"
for b in "$ROOT/frescod/target/$TARGET/release/frescod" "$ROOT/atrium-sheets/target/$TARGET/release/atrium-sheets"; do
    put "$b" "$D/"
    [ "$(sha256sum "$b" | cut -d' ' -f1)" = "$(g "sha256 -q $D/$(basename "$b")")" ] || { echo "ABORT: hash mismatch for $(basename "$b")"; exit 1; }
done
g "mkdir -p /usr/local/share/fonts/ibm-plex"
for f in "$ROOT"/fonts/ibm-plex/*.ttf "$ROOT"/fonts/phosphor/*.ttf; do
    g "test -f /usr/local/share/fonts/ibm-plex/$(basename "$f")" || put "$f" /usr/local/share/fonts/ibm-plex/
done

# A small sheet to look at: numbers, text, formulas, an error.
CSV=$(mktemp)
{
    echo "Region,Q1,Q2,Q3,Q4,Total,Growth"
    i=2
    for r in North South East West Central Islands Coast Hills; do
        echo "$r,$((i*1130+17)),$((i*1207+3)),$((i*990+411)),$((i*1402+9)),=SUM(B$i:E$i),=E$i/B$i-1"
        i=$((i+1))
    done
    echo "All,=SUM(B2:B9),=SUM(C2:C9),=SUM(D2:D9),=SUM(E2:E9),=SUM(F2:F9),=1/0"
} > "$CSV"
put "$CSV" /tmp/sh/sales.csv
rm -f "$CSV"
# OPEN=path: open that file instead (an .osheet, say; .xlsx needs the converter).
SHOW=/tmp/sh/sales.csv
if [ -n "${OPEN:-}" ]; then
    SHOW="/tmp/sh/$(basename "$OPEN")"
    put "$OPEN" "$SHOW"
fi

start_frescod() {
    g "cd /tmp/sh && env FRESCOD_SPLIT=0 FRESCOD_HEADLESS_PNG=/tmp/sh/screen FRESCOD_SOCK=/tmp/sh/fresco.sock \
        FRESCOD_WM_SOCK=/tmp/sh/wm.sock FRESCOD_INPUT_SOCK=/tmp/sh/inject.sock daemon -f -o /tmp/sh/frescod.log $D/frescod </dev/null"
    i=0; while [ $i -lt 90 ] && ! g "test -S /tmp/sh/fresco.sock"; do sleep 2; i=$((i+1)); done
    g "test -S /tmp/sh/fresco.sock" || { echo "frescod did not listen"; g "tail -20 /tmp/sh/frescod.log"; exit 1; }
}

echo "=== the app on a CSV (formulas read as text by CSV; the app shows them as typed)"
start_frescod
g "cd /tmp/sh && env HOME=/tmp/sh FRESCO_SOCKET=/tmp/sh/fresco.sock daemon -f -o /tmp/sh/app.log $D/atrium-sheets $SHOW </dev/null"
sleep ${SHOT_AFTER:-60}
mkdir -p "$(dirname "$OUT")"
g "ls -t /tmp/sh/screen* 2>/dev/null | head -1" | { read -r f && [ -n "$f" ] && get "$f" "$OUT" && echo "screenshot: $OUT"; } || echo "no screenshot"
g "pkill -x atrium-sheets; sleep 1; cat /tmp/sh/app.log"

[ "$BENCH" = none ] && { g "pkill -x frescod; true"; exit 0; }
echo "=== scroll gate: atrium-sheets --bench $BENCH"
g "cd /tmp/sh && env HOME=/tmp/sh FRESCO_SOCKET=/tmp/sh/fresco.sock $D/atrium-sheets --bench $BENCH" || echo "(bench exit $?)"
g "pkill -x frescod; true"
echo "=== frescod log tail"
g "tail -5 /tmp/sh/frescod.log"
