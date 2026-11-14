#!/bin/sh
# navigator-ui steps 2 to 4 (docs/spec/navigator-ui.md §14): one live page in the
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
# restart. Step 4: with a budget of one live page, a page left is parked
# (its child closed, its scroll kept), comes back where it was, and after
# a restart comes back from navigatord's store with no fetch. TCG is slow: WAIT (default 90) seconds per step.
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
# nd [log]: navigatord, its store of opened pages in $S/store.
nd() { g "cd $S && env NAVIGATORD_SOCKET=$S/nav.sock NAVIGATORD_PAGE_SOCKET=$S/page.sock NAVIGATORD_FIXTURES=$D/fixtures \
    NAVIGATORD_STORE=$S/store NAVIGATORD_PRERENDER=$D/prerender FRESCOD_CHROME_SOCK=$S/chrome.sock \
    NAVIGATORD_DOWNLOADS=$S/downloads NAVIGATORD_SCHEMES=mailto NAVIGATORD_OPENER='sh $S/opener.sh' \
    daemon -f -o $S/${1:-navigatord}.log $D/navigatord --unconfined-for-testing </dev/null"; }
# The hand-off program: says what it was given.
g "printf '%s\\n' 'echo \"\$@\" >> $S/opener.log' > $S/opener.sh; rm -rf $S/downloads $S/opener.log"
nd
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
    NAVIGATOR_TRAILS=$S/trails.json ${UIENV:-} ${1:+NAVIGATOR_UI_OPEN=$1} daemon -f -o $S/${2:-ui}.log $D/navigator-ui --light </dev/null"; }
ui https://atlas.example/wiki/Atrium
until_trace 'url=https://atlas.example/wiki/Atrium live=shown' && ok "the first page opened through navigatord" || bad "first page: $(last)"
sleep "$GAP"
g "grep -q 'page child attached' $S/ui.log" && ok "limend bound the page child into the slot" || bad "no page child: $(g "tail -5 $S/limend.log")"
g "grep -q 'laid out \"Atrium (architecture)\"' $S/ui.log" && ok "the child redeemed its ticket and laid the page out" || bad "child: $(g "grep navigator-page $S/limend.log | tail -3")"
g "grep -q 'navigator-page: 1 images with the page' $S/limend.log" && ok "the page's image came with its ticket" || bad "no image with the page"
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
# Back with the toolbar (keys from the page are step 7's, below).
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

echo "=== parking (step 4)"
# One live page at most: every page off screen that is not the parent of
# the page on screen is parked.
last() { g "grep 'trace' $S/ui5.log | tail -1"; }
UIENV=NAVIGATOR_UI_LIVE=1 ui https://atlas.example/wiki/Atrium ui5
until_trace 'url=https://atlas.example/wiki/Atrium live=shown' || bad "parking: first page: $(last)"
P=$(last | sed -n 's/.* page=\([0-9.]*\),\([0-9.]*\).*/\1 \2/p'); PX=${P% *}; PY=${P#* }; PX=${PX%.*}; PY=${PY%.*}
press $((PX + 50)) $((TOP + PY + 64 + 20))
until_trace 'url=https://atlas.example/wiki/Impluvium live=shown' || bad "parking: link: $(last)"
g "grep -q 'parking https://atlas.example/wiki/Atrium' $S/ui5.log" && bad "the parent of the page on screen was parked" || ok "the parent of the page on screen stays live"
# Scroll the page down, then leave it.
inj "MOVE $((PX + 300)) $((TOP + PY + 300))"; for n in 1 2 3; do inj "SCROLL 120"; sleep 2; done; sleep "$GAP"
press 270 $((TOP + 20))     # Back
until_trace 'url=https://atlas.example/wiki/Atrium live=shown' || bad "parking: back: $(last)"
g "grep -q 'showed live page https://atlas.example/wiki/Atrium' $S/ui5.log" && ok "Back to a live page swapped children (no open)" || bad "no swap: $(g "grep -v trace $S/ui5.log | tail -5")"
sleep 5
Y=$(g "sed -n 's|.*parked https://atlas.example/wiki/Impluvium at y=\([0-9]*\) .*|\1|p' $S/ui5.log | tail -1")
[ -n "$Y" ] && [ "$Y" -gt 0 ] && ok "the page left was parked at y=$Y" || bad "not parked (y='$Y'): $(g "grep -v trace $S/ui5.log | tail -5")"
key 0x4f 0x04               # Alt-Right
until_trace 'url=https://atlas.example/wiki/Impluvium live=shown' || bad "parking: forward: $(last)"
sleep 5
g "grep -q 'page https://atlas.example/wiki/Impluvium .* at y=$Y)' $S/ui5.log" && g "grep -q 'navigator-page: opened at y=$Y' $S/limend.log" \
    && ok "it came back scrolled to y=$Y" || bad "restore: $(g "grep -v trace $S/ui5.log | tail -4")"
shot live-6-parked-and-back
# After a restart of the UI and navigatord, with the pages' source gone:
# the page on screen comes back from navigatord's store, not the network.
g "pkill -x navigator-ui; pkill -x navigatord; sleep 2; mv $D/fixtures $D/fixtures.off"
nd navigatord2
sleep 5
last() { g "grep 'trace' $S/ui6.log | tail -1"; }
UIENV=NAVIGATOR_UI_LIVE=1 ui "" ui6
until_trace 'url=https://atlas.example/wiki/Impluvium live=shown' && g "grep -q 'opened https://atlas.example/wiki/Impluvium .*Store)' $S/navigatord2.log" \
    && ok "after a restart the parked page came back from the store (no fetch)" || bad "store: $(last); $(g "tail -3 $S/navigatord2.log")"
g "grep -q 'brought back from the store .* at y=$Y)' $S/ui6.log" && ok "still at y=$Y" || bad "restart scroll: $(g "grep -v trace $S/ui6.log | tail -3")"
g "mv $D/fixtures.off $D/fixtures"
echo "=== presence (step 5; no Arca in this run)"
last() { g "grep 'trace' $S/ui7.log | tail -1"; }
g "pkill -x navigator-ui; sleep 2; true"
ui https://atlas.example/wiki/Atrium ui7
until_trace 'url=https://atlas.example/wiki/Atrium live=shown' || bad "presence: first page: $(last)"
press 1110 $((TOP + 20))     # the presence control: let the site remember you
sleep "$GAP"
g "grep -q 'opens again: its site knows you now' $S/ui7.log" && ok "letting the site remember you opens the page again" || bad "presence: $(g "grep -v trace $S/ui7.log | tail -4")"
g "grep -q \"can't remember you: Arca isn't running\" $S/ui7.log" && ok "…and with no Arca here it says so, and the page stays anonymous" || bad "no note: $(g "grep -v trace $S/ui7.log | tail -4")"
sleep 3
g "grep -A2 '\"knows\"' $S/trails.json | grep -q atlas.example" && bad "a site that could not remember you was saved as knowing you" || ok "…and the control and the trail store say it doesn't know you"
shot live-7-presence
g "pkill -x navigator-ui; sleep 2; true"

echo "=== prompts (step 6)"
last() { g "grep 'trace' $S/ui8.log | tail -1"; }
ui https://atlas.example/wiki/Atrium ui8
until_trace 'url=https://atlas.example/wiki/Atrium live=shown' || bad "prompts: first page: $(last)"
P=$(last | sed -n 's/.* page=\([0-9.]*\),\([0-9.]*\).*/\1 \2/p'); PX=${P% *}; PY=${P#* }; PX=${PX%.*}; PY=${PY%.*}
W=$(last | sed -n 's/.* w=\([0-9]*\).*/\1/p')
# The prompt row's primary answer is its rightmost button (Save, Open in
# Mail, Try it), in the row under the toolbar.
answer() { press $((W - 40)) $((TOP + 41 + 22)); }
press $((PX + 50)) $((TOP + PY + 64 + 20 + 4 * 40))    # The survey (PDF)
until_trace 'prompt=download .*url=https://atlas.example/wiki/Atrium ' && ok "a link to a PDF asked in the prompt row, and the trail stayed on its page" || bad "download: $(last); $(g "tail -3 $S/navigatord2.log")"
g "grep -q 'survey.pdf is a file (PDF' $S/navigatord*.log" && ok "…navigatord told it from a document by its bytes, and did not convert it" || bad "file: $(g "tail -3 $S/navigatord2.log")"
last | grep -q 'pages=1 ' && ok "…and the file did not become a page of the trail" || bad "pages: $(last)"
shot live-8-download
answer
sleep 3
g "test -f $S/downloads/survey.pdf" && ok "Save wrote it to the downloads directory" || bad "not saved: $(g "grep -v trace $S/ui8.log | tail -3; ls -l $S/downloads")"
g "stat -f %Lp $S/downloads/survey.pdf" | grep -qx 644 && ok "…not executable (0644)" || bad "mode $(g "stat -f %Lp $S/downloads/survey.pdf")"
g "getextattr -q user atrium.origin $S/downloads/survey.pdf" | grep -q 'https://atlas.example/files/survey.pdf' && ok "…with where it came from recorded on the file" || bad "origin: $(g "getextattr user atrium.origin $S/downloads/survey.pdf 2>&1")"
until_trace 'prompt=- ' || bad "the row stayed: $(last)"
press $((PX + 50)) $((TOP + PY + 64 + 20 + 5 * 40))    # mailto:
until_trace 'prompt=handoff' && ok "a mailto: link asked before going anywhere" || bad "mailto: $(last)"
shot live-9-handoff
answer
sleep 3
g "grep -qx 'open mailto:curator@atlas.example' $S/opener.log" && ok "…Open in Mail handed it on" || bad "opener: $(g "cat $S/opener.log; grep -v trace $S/ui8.log | tail -3")"
until_trace 'prompt=- ' || bad "the row stayed: $(last)"
press $((PX + 50)) $((TOP + PY + 64 + 20 + 6 * 40))    # atrium-app:
until_trace 'prompt=app' && ok "an atrium-app: link asked to try or open the app" || bad "app: $(last)"
answer
sleep 3
g "grep -qx 'try atrium-app://atlas.example/viewer' $S/opener.log" && ok "…Try it handed it on to be tried" || bad "opener: $(g "cat $S/opener.log; grep -v trace $S/ui8.log | tail -3")"
g "grep -c . $S/opener.log" | grep -qx 2 && ok "nothing else was handed on" || bad "opener log: $(g "cat $S/opener.log")"
echo "--- park and restore times (TCG)"
g "grep -hE 'showed live page|parked .* in [0-9]+ ms|page .* in [0-9]+ ms' $S/ui5.log $S/ui6.log"
g "pkill -x navigator-ui; sleep 2; true"

echo "=== reading, find, history, keys, accessibility (step 7)"
last() { g "grep 'trace' $S/ui9.log | tail -1"; }
# NAVIGATOR_UI_IDLE_S: a page not looked at for a minute is parked (§4.4).
UIENV="NAVIGATOR_UI_AX=1 NAVIGATOR_UI_IDLE_S=60" ui https://atlas.example/wiki/Atrium ui9
until_trace 'url=https://atlas.example/wiki/Atrium live=shown' || bad "step 7: first page: $(last)"
P=$(last | sed -n 's/.* page=\([0-9.]*\),\([0-9.]*\).*/\1 \2/p'); PX=${P% *}; PY=${P#* }; PX=${PX%.*}; PY=${PY%.*}
W=$(last | sed -n 's/.* w=\([0-9]*\).*/\1/p')
# The left margin: no link there in either width.
inpage() { press $((PX + 12)) $((TOP + PY + 400)); }
typed() { for k in "$@"; do key "$k"; done; }
# A press in the page gives the page the keys; Ctrl-F there is handed
# back to the chrome with the focus, so the typing below lands in find.
inpage
key 0x09 0x01               # Ctrl-F
g "grep -q 'a key from the page: Find' $S/ui9.log" && ok "Ctrl-F in the page was handed back to the chrome" || bad "ctrl-f from the page: $(g "grep -v trace $S/ui9.log | tail -3")"
g "grep -q 'ax  *text-input \"Find in page\" \[focused\]' $S/ui9.log" && ok "…and opened the find bar, focused" || bad "ctrl-f: $(g "grep -v trace $S/ui9.log | tail -3")"
typed 0x04 0x17 0x15 0x0c 0x18 0x10    # a t r i u m
sleep 3
g "grep -q 'navigator-page: find: [1-9][0-9]* matches' $S/limend.log" && ok "typing in the find bar found matches in the page" || bad "find: $(g "grep 'find' $S/limend.log $S/ui9.log | tail -4")"
N=$(g "grep 'navigator-ui: find:' $S/ui9.log | tail -1" | sed -n 's/.*find: \([0-9]*\) matches.*/\1/p')
[ "${N:-0}" -gt 0 ] && ok "…and the chrome got a count ($N), not text" || bad "count: $(g "grep 'find:' $S/ui9.log | tail -2")"
key 0x28                    # Enter: the next match
g "grep 'navigator-ui: find:' $S/ui9.log | tail -1" | grep -q ', at 2' && ok "Enter went to the next match" || bad "next: $(g "grep 'find:' $S/ui9.log | tail -2")"
shot live-10-find
key 0x29                    # Esc
g "grep 'navigator-ui: find:' $S/ui9.log | tail -1" | grep -q 'find: 0 matches' && ok "Esc closed find and cleared the mark" || bad "esc: $(g "grep 'find:' $S/ui9.log | tail -2")"
# F6 (§9): rail, toolbar, page, and from the page (handed back) the rail.
key 0x3f                    # F6
g "grep -q 'F6: the keys are in the rail' $S/ui9.log" && g "grep -q 'ax  *tree \"Trails\" \[focused\]' $S/ui9.log" \
    && ok "F6 put the keys on the rail" || bad "f6 rail: $(g "grep -v trace $S/ui9.log | tail -3")"
key 0x3f
g "grep -q 'F6: the keys are in the toolbar' $S/ui9.log" && ok "…F6 again, the toolbar's Where field" || bad "f6 toolbar: $(g "grep -v trace $S/ui9.log | tail -3")"
key 0x3f
g "grep -q 'F6: the page has the keys' $S/ui9.log" && ok "…F6 again, the page (frescod handed it the keys)" || bad "f6 page: $(g "grep -v trace $S/ui9.log | tail -3")"
N6=$(g "grep -c 'F6: the keys are in the rail' $S/ui9.log")
key 0x3f
g "grep -q 'a key from the page: NextRegion' $S/ui9.log" && [ "$(g "grep -c 'F6: the keys are in the rail' $S/ui9.log")" -gt "$N6" ] \
    && ok "…and F6 in the page went round to the rail" || bad "f6 from the page: $(g "grep -v trace $S/ui9.log | tail -3")"
key 0x29                    # Esc: the rail lets go
# Aa: the pop-up hangs under its button, above the page's window.
press $((W - 90)) $((TOP + 20))
sleep "$GAP"
shot live-11-aa
press 1190 152              # Text size +
g "grep -q 'reading changed: text 72 ' $S/ui9.log" && ok "Aa: a larger text size went to the page" || bad "aa text: $(g "grep -v trace $S/ui9.log | tail -3")"
g "grep -q 'laid out again: text 112%' $S/limend.log" && ok "…which laid the page out again, without fetching it" || bad "relayout: $(g "grep 'laid out again' $S/limend.log | tail -2")"
press 1204 270              # Comfortable
g "grep -q 'reading changed: .* comfortable' $S/ui9.log" && ok "Aa: comfortable width" || bad "aa width: $(g "grep 'reading changed' $S/ui9.log | tail -2")"
shot live-12-comfortable
key 0x29                    # Esc: the pop-up goes
sleep 3
g "grep -q '\"site\": \"atlas.example\"' $S/trails.json" && g "grep -A2 '\"site\": \"atlas.example\"' $S/trails.json | grep -q '\"text\": 72'" \
    && ok "the choice is remembered for the site in the trail store" || bad "store: $(g "grep -A6 reading $S/trails.json")"
# Ctrl-plus in the page: its ScrollView zooms, and once it settles the
# chrome remembers it.
inpage
key 0x2e 0x01               # Ctrl-=
sleep 5
g "grep -q 'the page zoomed to 1[01][0-9]%' $S/ui9.log" && g "grep -q 'laid out again: .* zoom 1[01][0-9]%' $S/limend.log" && ok "Ctrl-plus in the page zoomed it, and the chrome remembered it" || bad "zoom: $(g "grep -i zoom $S/ui9.log $S/limend.log | tail -3")"
# Ctrl-H: the history view in place of the page.
inpage
key 0x0b 0x01               # Ctrl-H
g "grep -q 'history view open; the page is withdrawn' $S/ui9.log" && ok "Ctrl-H opened the history view, the page withdrawn" || bad "history: $(g "grep -v trace $S/ui9.log | tail -3")"
shot live-13-history
typed 0x0c 0x10 0x13 0x0f   # i m p l
key 0x28
until_trace 'url=https://atlas.example/wiki/Impluvium live=shown' && ok "filtering and Enter opened Impluvium from history" || bad "history open: $(last)"
g "grep -q 'ax  *tree \"Trails\"' $S/ui9.log" && g "grep -q 'ax  *embedded-content \"Impluvium' $S/ui9.log" && g "grep -q 'ax  *document \"History' $S/ui9.log" \
    && ok "the accessibility tree has the rail as a tree, the page slot, the history view" || bad "ax: $(g "grep ' ax ' $S/ui9.log | tail -12")"
# A page from history opens as its stored version and offers the current one.
until_trace 'prompt=stored .*url=https://atlas.example/wiki/Impluvium ' && ok "…as the version stored when it was seen, offering the current one" || bad "stored: $(last)"
F0=$(g "cat $S/navigatord*.log | grep -c 'opened https://atlas.example/wiki/Impluvium .*Fetched'")
sleep 1
answer                      # Fetch the current version
until_trace 'prompt=- .*url=https://atlas.example/wiki/Impluvium live=shown' || bad "fetch: $(last)"
F1=$(g "cat $S/navigatord*.log | grep -c 'opened https://atlas.example/wiki/Impluvium .*Fetched'")
g "grep -q 'fetching the current version of https://atlas.example/wiki/Impluvium' $S/ui9.log" && [ "$F1" -gt "$F0" ] \
    && ok "Fetch the current version fetched it again (not the cache or the store)" || bad "fetch: $F0 -> $F1; $(g "grep -v trace $S/ui9.log | tail -3")"
# Clearing history: it asks first, then the trail store and navigatord's
# store lose the pages, and each open trail keeps the page it is on. The
# trail on screen is put away first, so its pages are cleared whole.
# Atrium, in another trail, has not been looked at since history opened
# Impluvium: idle, it is parked (never the page on screen).
i=0; until g "grep -q 'parking https://atlas.example/wiki/Atrium (not looked at for' $S/ui9.log" || [ $i -ge 120 ]; do sleep 5; i=$((i+5)); done
g "grep -q 'parking https://atlas.example/wiki/Atrium (not looked at for' $S/ui9.log" && g "grep -q 'parked https://atlas.example/wiki/Atrium at y=' $S/ui9.log" \
    && ok "a page not looked at for a minute was parked" || bad "idle: $(g "grep -E 'parking|parked' $S/ui9.log | tail -3")"
key 0x1a 0x01               # Ctrl-W
key 0x0b 0x01               # Ctrl-H
P=$(last | sed -n 's/.* page=\([0-9.]*\),\([0-9.]*\).*/\1 \2/p'); PX=${P% *}; PY=${P#* }; PX=${PX%.*}; PY=${PY%.*}
PW=$((W - PX)); COL=$((PW - 64)); [ "$COL" -gt 720 ] && COL=720
X0=$((PX + (PW - COL) / 2))
N0=$(g "ls $S/store | wc -l")
press $((X0 + COL - 120 + 42)) $((TOP + PY + 38))    # Clear all…
g "grep -q 'ax  *alert \"Clear all history, [0-9]* pages' $S/ui9.log" && ok "Clear all asked first, saying how many pages it takes" || bad "clear: $(g "grep -v trace $S/ui9.log | tail -3")"
shot live-14-clear
key 0x28                    # Enter: Clear
sleep 3
g "grep -q 'history: cleared [1-9][0-9]* pages from all history' $S/ui9.log" && ok "…Enter cleared them from the trail store" || bad "cleared: $(g "grep -v trace $S/ui9.log | tail -3")"
N1=$(g "ls $S/store | wc -l")
# A stored version still named by a page left (each trail's own page) stays:
# the UI says how many it asked navigatord to take out.
V=$(g "grep 'history: cleared' $S/ui9.log | tail -1" | sed -n 's/.*(\([0-9]*\) stored versions).*/\1/p')
if [ "${V:-0}" -gt 0 ]; then
    g "grep -q 'navigatord: cleared $V page records' $S/navigatord*.log" && [ "$N1" -lt "$N0" ] \
        && ok "…and navigatord took their $V stored versions out of its store ($N0 -> $N1 objects)" || bad "store: $V asked, $N0 -> $N1; $(g "grep cleared $S/navigatord*.log")"
else
    bad "store: no stored version was only the cleared pages' ($N0 -> $N1)"
fi
g "grep -q '\"earlier\": \[\]' $S/trails.json" \
    && ok "…nothing put away is left" || bad "trails: $(g "head -c 600 $S/trails.json")"
g "pkill -x navigator-ui; sleep 2; true"

echo "=== logs"
for l in navigatord limend ui ui4 ui5 ui6 ui7 ui8 ui9 navigatord2; do echo "--- $l"; g "grep -v trace $S/$l.log | tail -12"; done
g "grep -iE 'chrome|slot|origin|panic' $S/frescod.log | tail -8; true"
echo "navigator-live: $fails failed"
[ "$fails" = 0 ]
