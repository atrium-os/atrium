#!/bin/sh
# navigator-ui (docs/spec/navigator-ui.md §14 step 1) on a live headless
# frescod in the FreeBSD dev VM: a screenshot of each scene, then a run
# driven through frescod's input injector (clicks and keys) whose trace
# lines are checked.
#
#   sh scripts/cloud-vm.sh up
#   sh scripts/navigator-ui-vm.sh [out-dir]          # scenes + drive
#   SCENES="main start" DRIVE=0 sh scripts/navigator-ui-vm.sh
#
# SPLIT=1 (default) runs frescod in split mode with the compute renderer,
# so the window sits under frescod's own 24 px origin strip as it will;
# SPLIT=0 parses the client inside frescod (no strip). TCG is slow: each
# scene waits WAIT seconds (default 60) before its frame is taken.
set -eu
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
OUT="${1:-$ROOT/scratch/navigator-ui}"
SCENES="${SCENES:-main main-dark prompt-dark start live stop}"
SPLIT="${SPLIT:-1}"
WAIT="${WAIT:-60}"
set -- env
. "$ROOT/scripts/cloud-vm.sh" >/dev/null
set --
KEY="$HOME/.ssh/fresco_bsd_ed25519"
SSHOPT="-i $KEY -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o LogLevel=ERROR"
g() { ssh $SSHOPT -o ConnectTimeout=30 -p 2222 root@localhost "$@"; }
put() { scp $SSHOPT -P 2222 -q "$1" "root@localhost:$2"; }
get() { scp $SSHOPT -P 2222 -q "root@localhost:$1" "$2"; }
fails=0
ok() { echo "  ok: $*"; }
bad() { echo "  FAIL: $*"; fails=$((fails + 1)); }

echo "=== cross-build"
xb() { (cd "$ROOT/$1" && RUSTFLAGS="$CROSS_RUSTFLAGS" cargo build -q --release --target "$TARGET" $2); }
xb frescod "--bin frescod --bin fresco-renderer"
xb navigator-ui "--bin navigator-ui"
[ "$SPLIT" = 1 ] && xb forum-wm "--bin forum-wm"

D=/opt/navigator-ui
S=/tmp/nui
g "pkill -x frescod; pkill -x navigator-ui; pkill -x forum-wm; sleep 1; rm -rf $S; mkdir -p $D $S; chmod 777 $S; true"
for b in "$ROOT/frescod/target/$TARGET/release/frescod" "$ROOT/frescod/target/$TARGET/release/fresco-renderer" \
         "$ROOT/navigator-ui/target/$TARGET/release/navigator-ui" \
         $([ "$SPLIT" = 1 ] && echo "$ROOT/forum-wm/target/$TARGET/release/forum-wm"); do
    put "$b" "$D/"
    # TCG scp sometimes lands a run of zeros: check every binary.
    [ "$(sha256sum "$b" | cut -d' ' -f1)" = "$(g "sha256 -q $D/$(basename "$b")")" ] || { echo "ABORT: hash mismatch for $(basename "$b")"; exit 1; }
done
g "mkdir -p /usr/local/share/fonts/ibm-plex"
for f in "$ROOT"/fonts/ibm-plex/*.ttf "$ROOT"/fonts/phosphor/*.ttf; do
    g "test -f /usr/local/share/fonts/ibm-plex/$(basename "$f")" || put "$f" /usr/local/share/fonts/ibm-plex/
done

if [ "$SPLIT" = 1 ]; then
    FENV="FRESCOD_SPLIT=1 FRESCOD_RENDERER=compute FRESCOD_RENDERER_LAUNCHER=unconfined FRESCOD_RENDERER_BIN=$D/fresco-renderer FRESCOD_APP_REGISTRY=$S/none"
    SIZE=1280x696
else
    FENV="FRESCOD_SPLIT=0"
    SIZE=1280x720
fi
g "cd $S && env $FENV FRESCOD_HEADLESS_PNG=$S/screen FRESCOD_SOCK=$S/fresco.sock \
    FRESCOD_WM_SOCK=$S/wm.sock FRESCOD_INPUT_SOCK=$S/inject.sock daemon -f -o $S/frescod.log $D/frescod </dev/null"
i=0; while [ $i -lt 60 ] && ! g "test -S $S/fresco.sock"; do sleep 2; i=$((i+1)); done
# In split mode forum-wm places the window under frescod's strip, as in a
# session (without it the strip covers the window's top rows).
if [ "$SPLIT" = 1 ]; then
    g "cd $S && env FRESCO_SOCKET=$S/fresco.sock FRESCO_WM_SOCKET=$S/wm.sock FORUM_CTL_SOCKET=$S/ctl.sock FORUM_STATE=$S/forum-state daemon -f -o $S/wm.log $D/forum-wm </dev/null"
    sleep 8
fi
mkdir -p "$OUT"

run() { # run <log> <args...>: start navigator-ui in the background
    log=$1; shift
    g "cd $S && env FRESCO_SOCKET=$S/fresco.sock NAVIGATOR_UI_SIZE=$SIZE NAVIGATOR_UI_TRACE=1 daemon -f -o $S/$log.log $D/navigator-ui $* </dev/null"
}
stop() { g "pkill -x navigator-ui; sleep 2; true"; }
shot() { g "cp $S/screen.png $S/shot.png"; get $S/shot.png "$OUT/$1.png" && echo "  frame: $OUT/$1.png"; }

echo "=== scenes"
for sc in $SCENES; do
    name=${sc%-dark}
    theme=--light
    [ "$name" != "$sc" ] && theme=--dark
    run "scene-$sc" --scene "$name" $theme
    sleep "$WAIT"
    shot "navigator-ui-$sc"
    stop
done

if [ "${DRIVE:-1}" = 1 ]; then
    echo "=== drive: clicks and keys through the injector"
    inj() { g "printf '$1\\n' | nc -N -U $S/inject.sock"; }
    press() { inj "MOVE $1 $2"; sleep 1; inj "BTN 1 1"; sleep 1; inj "BTN 1 0"; sleep "${GAP:-12}"; }
    # KEY <hid> with optional modifier byte (0x01 = left Ctrl, 0x04 = left Alt).
    key() { inj "KEY $1 1 ${2:-0}"; sleep 1; inj "KEY $1 0 ${2:-0}"; sleep "${GAP:-12}"; }
    last() { g "grep 'trace' $S/drive.log | tail -1"; }
    # Window-relative points go through the strip in split mode.
    TOP=0; [ "$SPLIT" = 1 ] && TOP=62   # forum-wm puts the window under the strip and the bar's band
    run drive --light
    sleep "$WAIT"
    shot drive-0
    # The first stand-in link on "Atrium (architecture)" is Impluvium:
    # rail 248 + 1, link rows start at y 41+36+40+28+76+18.
    press 700 $((TOP + 41 + 198 + 15))
    last | grep -q 'trail="Impluvium"' && ok "a link click followed in the trail" || bad "link click: $(last)"
    press 270 $((TOP + 20))
    last | grep -q 'trail="Atrium (architecture)"' && ok "Back went to the parent page" || bad "back: $(last)"
    key 0x17 0x01   # Ctrl-T
    last | grep -q 'trail="New trail".*where=""' && ok "Ctrl-T opened a trail on Start, Where focused" || bad "Ctrl-T: $(last)"
    for k in 0x15 0x12 0x10 0x04 0x11 0x2c 0x19 0x0c 0x0f 0x0f 0x04; do inj "KEY $k 1"; inj "KEY $k 0"; done   # "roman villa"
    sleep "${GAP:-12}"
    shot drive-1-typing
    key 0x28        # Enter
    last | grep -q 'trail="Search: roman villa" pages=1' && ok "Enter searched in the new trail" || bad "enter: $(last)"
    shot drive-2-search
    key 0x1a 0x01   # Ctrl-W
    last | grep -q 'earlier=4' && ok "Ctrl-W put the trail away" || bad "Ctrl-W: $(last)"
    key 0x05 0x01   # Ctrl-B
    last | grep -q 'rail=56' && ok "Ctrl-B collapsed the rail" || bad "Ctrl-B: $(last)"
    shot drive-3-collapsed
    stop
    g "tail -20 $S/drive.log"
fi

echo "=== logs"
g "grep -v '^\$' $S/frescod.log | tail -8; test -f $S/wm.log && tail -4 $S/wm.log; for f in $S/scene-*.log; do echo \"-- \$f\"; grep -v trace \$f | tail -4; done"
echo "navigator-ui: $fails failed"
[ "$fails" = 0 ]
