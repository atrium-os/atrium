#!/bin/sh
# Pop-up window check (docs/spec/fresco-popups.md §5 step 2). Runs on the
# HOST of the cloud amd64 VM (scripts/cloud-vm.sh up). Runs a headless
# frescod with the input injector, direct and then split (an unconfined
# fresco-renderer), and atrium-popup-probe against each: open, keys, a
# press inside, the flip at the screen edge, a submenu, dismissal by an
# outside press (swallowed), a refused anchor, a tooltip.
#
#   sh scripts/vm-popup-check.sh FRESCOD FRESCO_RENDERER POPUP_PROBE
#
# The probe's second client plays the bar: frescod grants it the Chrome
# role (FRESCOD_CHROME_UIDS=0, root) and the probe checks it got it.
#
# (x86_64-unknown-freebsd release builds.) Ends "popup-check: N failed".
set -u
FD=$1 RD=$2 PROBE=$3
ROOT=$(cd "$(dirname "$0")/.." && pwd)
K="-i $HOME/.ssh/fresco_bsd_ed25519 -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o LogLevel=ERROR"
V() { "$ROOT/scripts/vssh" "$@"; }
fails=0

V 'pkill -x frescod; pkill -f atrium-popup-probe; sleep 1; mkdir -p /root/pp'
for f in "$FD" "$RD" "$PROBE"; do
  scp -q $K -P 2222 "$f" root@localhost:/root/pp/
done

for mode in direct split; do
  echo "== $mode"
  SPLIT="FRESCOD_SPLIT=0"
  [ $mode = split ] && SPLIT="FRESCOD_SPLIT=1 FRESCOD_RENDERER=compute FRESCOD_RENDERER_LAUNCHER=unconfined FRESCOD_RENDERER_BIN=/root/pp/fresco-renderer"
  V "pkill -x frescod; sleep 1; rm -rf /tmp/pp; mkdir -p /tmp/pp/hl; cd /root/pp && \
     daemon -f -o /tmp/pp/frescod.log env $SPLIT FRESCOD_HEADLESS_PNG=/tmp/pp/hl/f FRESCOD_APP_REGISTRY=/tmp/pp/none FRESCOD_CHROME_UIDS=0 \
       FRESCOD_SOCK=/tmp/pp/fresco.sock FRESCOD_WM_SOCK=/tmp/pp/wm.sock FRESCOD_INPUT_SOCK=/tmp/pp/inject.sock ./frescod </dev/null"
  V 'for i in $(seq 1 60); do [ -S /tmp/pp/fresco.sock ] && [ -S /tmp/pp/inject.sock ] && break; sleep 1; done'
  V 'cd /root/pp && PROBE_WM_SOCK=/tmp/pp/wm.sock timeout 150 ./atrium-popup-probe /tmp/pp/fresco.sock /tmp/pp/inject.sock' | tee /tmp/popup-$mode.log
  grep -q "^popup: 0 failed" /tmp/popup-$mode.log || { fails=$((fails+1)); V 'tail -40 /tmp/pp/frescod.log'; }
done
V 'pkill -x frescod'
echo "popup-check: $fails failed"
