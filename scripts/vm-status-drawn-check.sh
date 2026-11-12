#!/bin/sh
# App-drawn status panels (docs/spec/forum-status-items.md §4), live on a
# headless split-mode frescod in the cloud FreeBSD VM: frescod (unconfined
# fresco-renderers), forum-wm (FORUM_STATUS_ANY=1), forum-bar (Chrome) and
# Pergola's status_drawn example. The injector presses the app's icon in
# the bar; the bar asks frescod for a grant and hands the token to the app
# through forum-wm; the app claims the pop-up and draws its panel; frescod
# puts a tab naming the app under it. Then a press outside closes it, and
# the icon opens it again. Runs on the HOST; ends "drawn: N failed".
#
#   sh scripts/vm-status-drawn-check.sh            (after building, below)
#
# Builds (x86_64-unknown-freebsd, release, CROSS_RUSTFLAGS as cloud-vm.sh):
#   frescod (frescod, fresco-renderer), forum-wm, forum-bar, forum-ctl,
#   pergola --example status_drawn.
# ICON=x,y: the app's icon in the bar (default 942,43: split mode, the
# strip above the bar). OUT=dir: frames (default /tmp/status-drawn).
set -u
ROOT=$(cd "$(dirname "$0")/.." && pwd)
T=x86_64-unknown-freebsd
R=release
KEY="$HOME/.ssh/fresco_bsd_ed25519"
SSHOPT="-i $KEY -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o LogLevel=ERROR"
g() { ssh $SSHOPT -o ConnectTimeout=30 -p 2222 root@localhost "$@"; }
put() { scp $SSHOPT -P 2222 -q "$1" "root@localhost:$2"; }
get() { scp $SSHOPT -P 2222 -q "root@localhost:$1" "$2"; }
ICON=${ICON:-942,43}
OUT=${OUT:-/tmp/status-drawn}
mkdir -p "$OUT"
D=/opt/sd
S=/tmp/sd
fails=0
ok() { echo "  ok: $*"; }
bad() { echo "  FAIL: $*"; fails=$((fails + 1)); }

g "pkill -x frescod; pkill -x forum-wm; pkill -x forum-bar; pkill -f status_drawn; sleep 1; rm -rf $S; mkdir -p $D $S; chmod 777 $S; true"
for b in frescod/target/$T/$R/frescod frescod/target/$T/$R/fresco-renderer forum-wm/target/$T/$R/forum-wm \
         forum-bar/target/$T/$R/forum-bar forum-ctl/target/$T/$R/forum-ctl pergola/target/$T/$R/examples/status_drawn; do
  put "$ROOT/$b" "$D/" || { echo "missing $b: build it first"; exit 2; }
done
g "mkdir -p /usr/local/share/fonts/ibm-plex"
for f in "$ROOT"/fonts/ibm-plex/*.ttf "$ROOT"/fonts/phosphor/*.ttf; do
  g "test -f /usr/local/share/fonts/ibm-plex/$(basename "$f")" || put "$f" /usr/local/share/fonts/ibm-plex/
done

E="FRESCO_SOCKET=$S/fresco.sock FRESCO_WM_SOCKET=$S/wm.sock FORUM_CTL_SOCKET=$S/ctl.sock FORUM_STATUS_SOCKET=$S/status.sock"
g "cd $S && env RUST_LOG=${RUST_LOG:-warn,frescod::split=info} FRESCOD_SPLIT=1 FRESCOD_RENDERER=compute FRESCOD_RENDERER_LAUNCHER=unconfined FRESCOD_RENDERER_BIN=$D/fresco-renderer \
    FRESCOD_APP_REGISTRY=$S/none FRESCOD_HEADLESS_PNG=$S/screen FRESCOD_SOCK=$S/fresco.sock FRESCOD_CHROME_UIDS=0 \
    FRESCOD_WM_SOCK=$S/wm.sock FRESCOD_INPUT_SOCK=$S/inject.sock daemon -f -o $S/frescod.log $D/frescod </dev/null"
i=0; while [ $i -lt 60 ] && ! g "test -S $S/fresco.sock"; do sleep 2; i=$((i+1)); done
g "cd $S && env $E FORUM_STATUS_ANY=1 daemon -f -o $S/wm.log $D/forum-wm </dev/null"
sleep 5
g "cd $S && env $E daemon -f -o $S/bar.log $D/forum-bar </dev/null"
sleep 8
g "cd $S && env $E daemon -f -o $S/app.log $D/status_drawn </dev/null"
sleep ${SETTLE:-30}

press() {
  g "printf 'MOVE $1 $2\\n' | nc -N -U $S/inject.sock; sleep 1; printf 'BTN 1 1\\n' | nc -N -U $S/inject.sock; sleep 1; printf 'BTN 1 0\\n' | nc -N -U $S/inject.sock"
}
shot() { sleep ${GAP:-20}; g "cp $S/screen.png $S/shot.png"; get $S/shot.png "$OUT/$1.png"; }
x=${ICON%,*}; y=${ICON#*,}

echo "== the item is in the bar"
g "env FORUM_CTL_SOCKET=$S/ctl.sock $D/forum-ctl status" | tee "$OUT/status.txt"
grep -q "its own panel, 300x300" "$OUT/status.txt" && ok "forum-wm lists the drawn panel" || bad "no drawn item listed"
shot 0-closed

echo "== press the icon: grant, claim, panel"
press "$x" "$y"
shot 1-open
g "grep -c 'panel open' $S/app.log" | grep -q '^1$' && ok "the app opened its panel" || bad "app did not open its panel"
g "grep -q 'OpenPanel' $S/app.log" && ok "forum-wm passed the token" || bad "no OpenPanel at the app"
g "grep -E 'grant refused|claim refused' $S/frescod.log" && bad "frescod refused" || ok "frescod took the grant and claim"

echo "== a press inside the panel picks a clip"
# The panel hangs end-aligned under the icon: the first clip row is
# about 50 px below its top.
press $((x - 120)) $((y + 30 + 50))
sleep 4
g "grep -q 'picked clip 0' $S/app.log" && ok "a click reached the app's panel" || bad "click did not reach the panel"

echo "== a press outside closes it"
press 200 500
shot 2-closed
g "grep -c 'panel closed' $S/app.log" | grep -q '^1$' && ok "the panel closed" || bad "panel did not close"

echo "== and the icon opens it again"
press "$x" "$y"
shot 3-again
g "grep -c 'panel open' $S/app.log" | grep -q '^2$' && ok "opened again with a new grant" || bad "did not open again"

echo "== logs"
g "for f in frescod wm bar app; do echo -- \$f; grep -v '^\$' $S/\$f.log | tail -6; done"
echo "drawn: $fails failed"
[ "$fails" = 0 ]
