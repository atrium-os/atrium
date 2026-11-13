#!/bin/sh
# atrium-activation-probe, with its island checks (navigator-ui.md §14
# step 6; RUNBOOK §13.6d), against the
# current frescod in the dev VM, in split mode, slot and island checks only (PROBE_SLOTS_ONLY).
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
KEY="$HOME/.ssh/fresco_bsd_ed25519"
SSHOPT="-i $KEY -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o LogLevel=ERROR"
g() { ssh $SSHOPT -o ConnectTimeout=60 -p 2222 root@localhost "$@"; }
put() { scp $SSHOPT -P 2222 -q "$1" "root@localhost:$2"; }
R=aarch64-unknown-freebsd/release
D=/opt/island-probe; S=/tmp/ip
g "pkill -x frescod; pkill -x forum-wm; pkill -x navigator-ui; pkill -x limend-dev; pkill -x navigatord; sleep 1; mkdir -p $D /var/run/atrium; true"
for b in "$ROOT/frescod/target/$R/frescod" "$ROOT/frescod/target/$R/fresco-renderer" "$ROOT/atrium-test-client/target/$R/atrium-activation-probe"; do
    h=$(sha256sum "$b" | cut -d' ' -f1)
    put "$b" "$D/"
    [ "$h" = "$(g "sha256 -q $D/$(basename "$b")")" ] || { echo "ABORT: hash mismatch for $(basename "$b")"; exit 1; }
done
rc=0
for mode in split; do
    echo "=== $mode"
    SPLIT=""; [ $mode = split ] && SPLIT="FRESCOD_SPLIT=1 FRESCOD_RENDERER=compute FRESCOD_RENDERER_LAUNCHER=unconfined FRESCOD_RENDERER_BIN=$D/fresco-renderer"
    g "pkill -x frescod; sleep 1; rm -rf $S; mkdir -p $S; cd $S && env $SPLIT FRESCOD_APP_REGISTRY=$S/none FRESCOD_HEADLESS_PNG=$S/screen FRESCOD_SOCK=$S/fresco.sock FRESCOD_WM_SOCK=$S/wm.sock \
        FRESCOD_INPUT_SOCK=$S/inject.sock FRESCOD_CHROME_SOCK=$S/chrome.sock FRESCOD_SLOT_UIDS=0 daemon -f -o $S/frescod.log $D/frescod </dev/null"
    i=0; while [ $i -lt 60 ] && ! g "test -S $S/fresco.sock && test -S /var/run/atrium/fresco-activation.sock"; do sleep 2; i=$((i+1)); done
    g "cd $S && PROBE_SLOTS_ONLY=1 timeout 900 $D/atrium-activation-probe $S/fresco.sock $S/wm.sock $S/inject.sock /var/run/atrium/fresco-activation.sock" > ${OUT:-/tmp}/probe-$mode.log 2>&1 || rc=1
    grep -iE "island|cover|FAIL|all passed" ${OUT:-/tmp}/probe-$mode.log
done
g "pkill -x frescod; true"
exit $rc
