#!/bin/sh
# navigator-ui steps 2 and 3 (docs/spec/navigator-ui.md §14): one live page in the
# FreeBSD dev VM. Split-mode frescod with its chrome channel, forum-wm,
# limend (limend-dev: providers started unconfined), navigatord (fixture
# pages, the converter as an ordinary process) and navigator-ui, driven
# through frescod's input injector; trace lines are checked and frames
# taken.
#
#   sh scripts/cloud-vm.sh up
#   sh scripts/navigator-live-vm.sh [out-dir]
#
# ★ NOT the shipped confinement: the doc-viewer child and the converter
# run as ordinary processes here (no signed bundles or worker rules for
# them yet), and pages come from navigator-ui/fixtures, not the network.
# What it does check is the wiring: navigatord opens a page, the child
# redeems its ticket and draws it in the UI's page slot, links go through
# navigatord, Back and Forward walk the trail, and frescod's strip names
# the page's origin. Step 3: the trail store is written as trails change,
# holds no page content, and brings trails and Earlier back after a
# restart. TCG is slow: WAIT (default 90) seconds per step.
set -eu
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
OUT="${1:-$ROOT/scratch/navigator-live}"
WAIT="${WAIT:-90}"
GAP="${GAP:-20}"
set -- env
. "$ROOT/scripts/cloud-vm.sh" >/dev/null
set --
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
KEY="$HOME/.ssh/fresco_bsd_ed25519"
SSHOPT="-i $KEY -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o LogLevel=ERROR"
g() { ssh $SSHOPT -o ConnectTimeout=30 -p 2222 root@localhost "$@"; }
put() { scp $SSHOPT -P 2222 -q "$1" "root@localhost:$2"; }
get() { scp $SSHOPT -P 2222 -q "root@localhost:$1" "$2"; }
fails=0
ok() { echo "  ok: $*"; }
bad() { echo "  FAIL: $*"; fails=$((fails + 1)); }

if [ "${BUILD:-1}" = 1 ]; then
    echo "=== cross-build"
    # ring (the fetcher's TLS, linked by prerender and navigatord) compiles
    # C: a FreeBSD-targeting clang with the sysroot's headers.
    T=$(echo "$TARGET" | tr - _)
    if ! env | grep -q "^CC_$T="; then
        export "CC_$T=$(command -v clang)" "CFLAGS_$T=--target=$TARGET --sysroot=${SYSROOT:-$HOME/atrium-vm/sysroot}" \
               "AR_$T=$(command -v llvm-ar || command -v ar)"
    fi
    xb() { (cd "$ROOT/$1" && RUSTFLAGS="$CROSS_RUSTFLAGS" cargo build -q --release --target "$TARGET" $2); }
    xb frescod "--bin frescod --bin fresco-renderer"
    xb forum-wm "--bin forum-wm"
    xb navigator-ui "--bin navigator-ui"
    xb navigator-page "--bin navigator-page"
    xb limen "--example limend-dev"
    xb navigator-prerender "--bin prerender"
    xb navigatord "--bin navigatord"
fi

D=/opt/navigator-live
S=/tmp/nlive
R="$TARGET/release"
g "pkill -x frescod; pkill -x navigator-ui; pkill -x forum-wm; pkill -x limend-dev; pkill -x navigatord; pkill -x navigator-page; sleep 1; rm -rf $S; mkdir -p $D $S /var/run/atrium; chmod 777 $S; true"
for b in "$ROOT/frescod/target/$R/frescod" "$ROOT/frescod/target/$R/fresco-renderer" "$ROOT/forum-wm/target/$R/forum-wm" \
         "$ROOT/navigator-ui/target/$R/navigator-ui" "$ROOT/navigator-page/target/$R/navigator-page" \
         "$ROOT/limen/target/$R/examples/limend-dev" "$ROOT/navigator-prerender/target/$R/prerender" \
         "$ROOT/navigatord/target/$R/navigatord"; do
    h=$(sha256sum "$b" | cut -d' ' -f1)
    [ "$h" = "$(g "sha256 -q $D/$(basename "$b") 2>/dev/null || true")" ] && continue
    put "$b" "$D/"
    # TCG scp sometimes lands a run of zeros: check every binary.
    [ "$h" = "$(g "sha256 -q $D/$(basename "$b")")" ] || { echo "ABORT: hash mismatch for $(basename "$b")"; exit 1; }
done
g "mkdir -p /usr/local/share/fonts/ibm-plex"
for f in "$ROOT"/fonts/ibm-plex/*.ttf "$ROOT"/fonts/phosphor/*.ttf; do
    g "test -f /usr/local/share/fonts/ibm-plex/$(basename "$f")" || put "$f" /usr/local/share/fonts/ibm-plex/
done
(cd "$ROOT/navigator-ui" && tar czf - fixtures) | g "rm -rf $D/fixtures; tar xzf - -C $D"

echo "=== start"
g "cd $S && env FRESCOD_SPLIT=1 FRESCOD_RENDERER=compute FRESCOD_RENDERER_LAUNCHER=unconfined FRESCOD_RENDERER_BIN=$D/fresco-renderer \
    FRESCOD_APP_REGISTRY=$S/none FRESCOD_HEADLESS_PNG=$S/screen FRESCOD_SOCK=$S/fresco.sock FRESCOD_WM_SOCK=$S/wm.sock \
    FRESCOD_INPUT_SOCK=$S/inject.sock FRESCOD_CHROME_SOCK=$S/chrome.sock FRESCOD_SLOT_UIDS=0 \
    daemon -f -o $S/frescod.log $D/frescod </dev/null"
i=0; while [ $i -lt 60 ] && ! g "test -S $S/fresco.sock && test -S /var/run/atrium/fresco-activation.sock"; do sleep 2; i=$((i+1)); done
g "cd $S && env FRESCO_SOCKET=$S/fresco.sock FRESCO_WM_SOCKET=$S/wm.sock FORUM_CTL_SOCKET=$S/ctl.sock FORUM_STATE=$S/forum-state daemon -f -o $S/wm.log $D/forum-wm </dev/null"
# The page child runs as its own user: frescod gives an embed connection
# only to a uid that is not a slot binder (limend-dev, as root, is one).
g "pw usershow navpage >/dev/null 2>&1 || pw useradd navpage -u 50901 -d /nonexistent -s /usr/sbin/nologin"
g "cd $S && env LIMEN_SOCKET=$S/limend.sock LIMEN_DEV_UID=50901 LIMEN_DEFAULTS=doc-viewer=org.atrium.navigator.page \
    LIMEN_DEV_APPS=org.atrium.navigator.page=$D/navigator-page LIMEN_DEV_APP=org.atrium.navigator \
    NAVIGATOR_PAGE_SOCKET=$S/page.sock daemon -f -o $S/limend.log $D/limend-dev </dev/null"
g "cd $S && env NAVIGATORD_SOCKET=$S/nav.sock NAVIGATORD_PAGE_SOCKET=$S/page.sock NAVIGATORD_FIXTURES=$D/fixtures \
    NAVIGATORD_PRERENDER=$D/prerender FRESCOD_CHROME_SOCK=$S/chrome.sock daemon -f -o $S/navigatord.log $D/navigatord --unconfined-for-testing </dev/null"
sleep 10
g "test -S $S/limend.sock && test -S $S/nav.sock" || { echo "ABORT: limend or navigatord did not start"; g "tail -5 $S/limend.log $S/navigatord.log"; exit 1; }
mkdir -p "$OUT"

shot() { g "cp $S/screen.png $S/shot.png"; get $S/shot.png "$OUT/$1.png" && echo "  frame: $OUT/$1.png"; }
inj() { g "printf '$1\\n' | nc -N -U $S/inject.sock"; }
press() { inj "MOVE $1 $2"; sleep 1; inj "BTN 1 1"; sleep 1; inj "BTN 1 0"; sleep "$GAP"; }
key() { inj "KEY $1 1 ${2:-0}"; sleep 1; inj "KEY $1 0 ${2:-0}"; sleep "$GAP"; }
last() { g "grep 'trace' $S/ui.log | tail -1"; }
# Wait (up to WAIT s) for the last trace line to match.
until_trace() { i=0; while [ $i -lt "$WAIT" ]; do last | grep -q "$1" && return 0; sleep 3; i=$((i+3)); done; return 1; }

echo "=== drive"
# ui [open-url] [log]: start navigator-ui on the run's trail store.
ui() { g "cd $S && env FRESCO_SOCKET=$S/fresco.sock NAVIGATORD_SOCKET=$S/nav.sock LIMEN_SOCKET=$S/limend.sock NAVIGATOR_UI_TRACE=1 \
    NAVIGATOR_TRAILS=$S/trails.json ${1:+NAVIGATOR_UI_OPEN=$1} daemon -f -o $S/${2:-ui}.log $D/navigator-ui --light </dev/null"; }
ui https://atlas.example/wiki/Atrium
until_trace 'url=https://atlas.example/wiki/Atrium live=shown' && ok "the first page opened through navigatord" || bad "first page: $(last)"
sleep "$GAP"
g "grep -q 'page child attached' $S/ui.log" && ok "limend bound the page child into the slot" || bad "no page child: $(g "tail -5 $S/limend.log")"
g "grep -q 'laid out \"Atrium (architecture)\"' $S/ui.log" && ok "the child redeemed its ticket and laid the page out" || bad "child: $(g "grep navigator-page $S/limend.log | tail -3")"
shot live-1-atrium
# The page's top left on screen: forum-wm puts the window under the strip
# and the bar (62), the page slot starts at the trace's page=x,y.
P=$(last | sed -n 's/.* page=\([0-9.]*\),\([0-9.]*\).*/\1 \2/p'); PX=${P% *}; PY=${P#* }; PX=${PX%.*}; PY=${PY%.*}
TOP=62
# h1 64 px tall, then each nav line 40 px: Impluvium, old atlas, script.
press $((PX + 50)) $((TOP + PY + 64 + 20))
until_trace 'url=https://atlas.example/wiki/Impluvium live=shown' && ok "a link in the page went through navigatord and followed" || bad "link: $(last)"
shot live-2-impluvium
press 270 $((TOP + 20))     # the toolbar's Back
until_trace 'url=https://atlas.example/wiki/Atrium live=shown' && ok "Back went to the parent page" || bad "back: $(last)"
key 0x4f 0x04               # Alt-Right
until_trace 'url=https://atlas.example/wiki/Impluvium live=shown' && ok "Alt-Right went forward" || bad "forward: $(last)"
key 0x50 0x04               # Alt-Left
until_trace 'url=https://atlas.example/wiki/Atrium live=shown' && ok "Alt-Left went back" || bad "alt-left: $(last)"
press $((PX + 50)) $((TOP + PY + 64 + 60))
until_trace 'url=http://old-atlas.example/ live=shown' && ok "an http link opened (amber)" || bad "http link: $(last)"
shot live-3-http
# The page child has the focus now, and the Navigator's keys don't reach
# it yet (step 7's keyboard map): Back with the toolbar.
press 270 $((TOP + 20))
until_trace 'url=https://atlas.example/wiki/Atrium live=shown' || bad "back from http: $(last)"
press $((PX + 50)) $((TOP + PY + 64 + 100))
g "grep -q 'link refused' $S/ui.log" && ok "a javascript: link was refused by navigatord" || bad "script link: $(g "tail -3 $S/ui.log")"
# Focus in the page (a press on text, not a link): the strip still names it.
press $((PX + 300)) $((TOP + PY + 260))
shot live-4-page-focused
g "pkill -x navigator-ui; sleep 2; true"

echo "=== trails on disk (step 3)"
# The trail store was written as it changed; it holds records, not pages.
g "test -f $S/trails.json" && ok "the trail store was written" || bad "no $S/trails.json"
g "stat -f %Lp $S/trails.json" | grep -qx 600 && ok "the store is the user's alone (0600)" || bad "mode $(g "stat -f %Lp $S/trails.json")"
g "grep -q 'old-atlas.example' $S/trails.json && grep -q 'wiki/Impluvium' $S/trails.json" && ok "it has the trail's pages" || bad "store: $(g "head -c 400 $S/trails.json")"
g "grep -q 'rooms of the house' $S/trails.json" && bad "page text in the trail store" || ok "no page text in it"
# A restart comes back on the same trail and page.
last() { g "grep 'trace' $S/ui2.log | tail -1"; }
ui "" ui2
until_trace 'url=https://atlas.example/wiki/Atrium live=shown' && ok "after a restart the trail came back on its page" || bad "restart: $(last)"
last | grep -q 'pages=3 ' && ok "with its three pages" || bad "pages: $(last)"
# A second trail, put away with Ctrl-W (a press on the toolbar first, so
# the keys go to the Navigator and not the page).
g "pkill -x navigator-ui; sleep 2; true"
last() { g "grep 'trace' $S/ui3.log | tail -1"; }
ui http://old-atlas.example/ ui3
until_trace 'url=http://old-atlas.example/ live=shown' && last | grep -q 'trails=2 ' && ok "a second trail opened beside the restored one" || bad "second trail: $(last)"
press 270 $((TOP + 20))     # the toolbar (Back is off here)
key 0x1a 0x01               # Ctrl-W (HID modifier byte: ctrl 1, shift 2, alt 4)
until_trace 'trails=1 earlier=1' && ok "Ctrl-W put it away" || bad "put away: $(last)"
sleep 3
g "pkill -x navigator-ui; sleep 2; true"
last() { g "grep 'trace' $S/ui4.log | tail -1"; }
ui "" ui4
until_trace 'trails=1 earlier=1 .*url=https://atlas.example/wiki/Atrium live=shown' && ok "Earlier survived a restart" || bad "earlier: $(last)"
press 270 $((TOP + 20))     # the toolbar (Back is off here)
key 0x17 0x03               # Ctrl-Shift-T
until_trace 'trails=2 earlier=0 .*url=http://old-atlas.example/ live=shown' && ok "Ctrl-Shift-T brought it back and opened its page" || bad "bring back: $(last)"
shot live-5-restored
g "pkill -x navigator-ui; sleep 2; true"

echo "=== logs"
for l in navigatord limend ui ui4; do echo "--- $l"; g "grep -v trace $S/$l.log | tail -12"; done
g "grep -iE 'chrome|slot|origin|panic' $S/frescod.log | tail -8; true"
echo "navigator-live: $fails failed"
[ "$fails" = 0 ]
