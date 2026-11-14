#!/bin/sh
# The dock's Applications view (docs/spec/forum.md §6.1), live on a headless
# split-mode frescod in the cloud FreeBSD VM: frescod, forum-wm, forum-bar and
# forum-dock, unjailed, with a catalog of sample manifests read off
# FORUM_APPS_DIR (no portcullisd, so a launch fails and the view says so).
# Runs on the HOST; ends "apps-grid: N failed".
#
#   ARCH=amd64 sh scripts/cloud-vm.sh up
#   sh scripts/vm-apps-grid-check.sh [out-dir]
#
# Builds first (x86_64-unknown-freebsd, release, CROSS_RUSTFLAGS from
# cloud-vm.sh): frescod (frescod, fresco-renderer), forum-wm, forum-bar,
# forum-dock.
#
# Checks: the dock row (Applications button + pins); Super+A opens the view
# (forum-wm answers the dock's parked WaitAppsKey); typing filters and
# selects the best match; Enter asks for a launch and the failure shows
# under the grid; Page Down turns the page; Escape clears, then closes; the
# dock's button opens it again and a press on empty space closes it.
set -u
ROOT=$(cd "$(dirname "$0")/.." && pwd)
OUT=${1:-/tmp/apps-grid}
T=x86_64-unknown-freebsd
R=release
KEY="$HOME/.ssh/fresco_bsd_ed25519"
SSHOPT="-i $KEY -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o LogLevel=ERROR"
g() { ssh $SSHOPT -o ConnectTimeout=30 -p 2222 root@localhost "$@"; }
put() { scp $SSHOPT -P 2222 -q "$1" "root@localhost:$2"; }
get() { scp $SSHOPT -P 2222 -q "root@localhost:$1" "$2"; }
mkdir -p "$OUT"
D=/opt/ag
S=/tmp/ag
fails=0
ok() { echo "  ok: $*"; }
bad() { echo "  FAIL: $*"; fails=$((fails + 1)); }

g "pkill -x frescod; pkill -x forum-wm; pkill -x forum-bar; pkill -x forum-dock; sleep 1; rm -rf $S; mkdir -p $D $S/apps; chmod 777 $S; true"
for b in frescod/target/$T/$R/frescod frescod/target/$T/$R/fresco-renderer forum-wm/target/$T/$R/forum-wm \
         forum-bar/target/$T/$R/forum-bar forum-dock/target/$T/$R/forum-dock; do
  put "$ROOT/$b" "$D/" || { echo "missing $b: build it first"; exit 2; }
done
g "mkdir -p /usr/local/share/fonts/ibm-plex"
for f in "$ROOT"/fonts/ibm-plex/*.ttf "$ROOT"/fonts/phosphor/*.ttf; do
  g "test -f /usr/local/share/fonts/ibm-plex/$(basename "$f")" || put "$f" /usr/local/share/fonts/ibm-plex/
done

echo "== sample catalog"
# id|name|icon|description: 28 apps, more than one page at 1280x720; two
# with no icon (letter tiles), and Forum's own bar (never listed).
g "sh -s" <<'EOF'
cd /tmp/ag/apps
while IFS='|' read id name icon desc; do
  mkdir -p "$id"
  {
    echo '[app]'
    echo "id = \"$id\""
    echo "name = \"$name\""
    echo 'version = "1.0.0"'
    echo 'entry = "bin/app"'
    echo "description = \"$desc\""
    [ -n "$icon" ] && echo "icon = \"$icon\""
    echo '[capabilities]'
    echo 'graphics = "fresco"'
  } > "$id/atrium.toml"
done <<'LIST'
org.atrium.navigator|Navigator|compass|The web, read safely
org.atrium.scrinium|Scrinium|files|Files and folders
org.atrium.write|Atrium Write|note-pencil|Documents
org.atrium.sheets|Atrium Sheets|table|Spreadsheets
org.atrium.show|Atrium Show|presentation-chart|Slides
org.atrium.player|Player|play|Music and video
org.atrium.curia|Settings|settings|System settings
org.atrium.stoa|Terminal|terminal|A shell in a jail
org.atrium.paint|Paint|paint-brush-broad|Pictures
org.atrium.photos|Photos|image|Your photos
org.atrium.mail|Mail|envelope|Email
org.atrium.calendar|Calendar|calendar|Days and events
org.atrium.contacts|Contacts|address-book|People
org.atrium.notes|Notes|notebook|Quick notes
org.atrium.clock|Clock|clock|Alarms and timers
org.atrium.calc|Calculator|calculator|Sums
org.atrium.maps|Maps|map-trifold|Places
org.atrium.weather|Weather|cloud-sun|Forecast
org.atrium.camera|Camera|camera|Take pictures
org.atrium.chat|Chat|chat-circle|Messages
org.atrium.books|Books|book-open|Reading
org.atrium.code|Code|code|Programming
org.atrium.games|Games|game-controller|Play
org.atrium.arca|Passwords|key|Arca, your keychain
org.atrium.monitor|Activity|chart-bar|What is running
org.example.zephyr|Zephyr||A third-party app, no icon
org.example.quill|quill||Another one, no icon
org.atrium.forum-bar|Forum Bar|bell|Session component
LIST
ls | wc -l
EOF
g "printf 'org.atrium.navigator\norg.atrium.scrinium\norg.atrium.write\norg.atrium.stoa\n' > $S/pins"

E="FRESCO_SOCKET=$S/fresco.sock FRESCO_WM_SOCKET=$S/wm.sock FORUM_CTL_SOCKET=$S/ctl.sock FORUM_STATUS_SOCKET=$S/status.sock"
g "cd $S && env RUST_LOG=${RUST_LOG:-warn} FRESCOD_SPLIT=1 FRESCOD_RENDERER=compute FRESCOD_RENDERER_LAUNCHER=unconfined FRESCOD_RENDERER_BIN=$D/fresco-renderer \
    FRESCOD_APP_REGISTRY=$S/none FRESCOD_HEADLESS_PNG=$S/screen FRESCOD_SOCK=$S/fresco.sock FRESCOD_CHROME_UIDS=0 \
    FRESCOD_WM_SOCK=$S/wm.sock FRESCOD_INPUT_SOCK=$S/inject.sock daemon -f -o $S/frescod.log $D/frescod </dev/null"
i=0; while [ $i -lt 60 ] && ! g "test -S $S/fresco.sock"; do sleep 2; i=$((i+1)); done
g "cd $S && env $E daemon -f -o $S/wm.log $D/forum-wm </dev/null"
sleep 5
g "cd $S && env $E daemon -f -o $S/bar.log $D/forum-bar </dev/null"
sleep 5
g "cd $S && env $E FORUM_APPS_DIR=$S/apps FORUM_DOCK_PINS=$S/pins daemon -f -o $S/dock.log $D/forum-dock </dev/null"
sleep ${SETTLE:-40}

inj() { g "printf '$1\\n' | nc -N -U $S/inject.sock"; sleep ${KGAP:-2}; }
key() { inj "KEY $1 1 ${2:-0}"; inj "KEY $1 0 ${2:-0}"; }
press() { inj "MOVE $1 $2"; inj "BTN 1 1"; inj "BTN 1 0"; }
shot() { sleep ${GAP:-20}; g "cp $S/screen.png $S/shot.png"; get $S/shot.png "$OUT/$1.png"; }
has() { g "grep -q -- \"$1\" $S/dock.log"; }
last() { g "grep 'apps: ' $S/dock.log | tail -1"; }

echo "== the dock"
has "28 app(s)\|27 app(s)" && ok "catalog read off the app tree" || bad "no catalog"
has "Forum Bar" && bad "Forum's own bar is listed" || ok "session components not listed"
BTN=$(g "grep 'Applications button at' $S/dock.log | tail -1" | sed 's/.* at //')
[ -n "$BTN" ] && ok "Applications button at $BTN" || bad "no Applications button logged"
shot 0-dock

echo "== Super+A opens the view"
key 0x04 0x08
sleep 10
has "Applications view open" && ok "opened by Super+A" || bad "Super+A did not open it"
g "grep -q 'no dock is waiting' $S/wm.log" && bad "forum-wm had no waiter" || ok "forum-wm answered the dock"
shot 1-grid

echo "== type to filter"
key 0x11; key 0x04   # n a
sleep 6
last | tee "$OUT/filter.txt"
grep -q 'query="na".*selected=org.atrium.navigator' "$OUT/filter.txt" && ok "\"na\" selects Navigator" || bad "filter did not select Navigator"
shot 2-filtered

echo "== Enter asks for a launch; the failure shows"
key 0x28
sleep 8
last | tee "$OUT/launch.txt"
grep -q 'note=Navigator could not start' "$OUT/launch.txt" && ok "the view says why" || bad "no note after a failed launch"
has "Applications view closed" && bad "closed on a failed launch" || ok "stays up on a failure"
shot 3-launch-failed

echo "== Escape clears the filter; Page Down turns the page"
key 0x29
sleep 4
last | grep -q 'query="" matches=27' && ok "filter cleared, 27 apps" || bad "Escape did not clear the filter"
key 0x4E
sleep 4
last | grep -q 'page=1' && ok "page two" || bad "Page Down did not turn the page"
shot 4-page-two
key 0x4B
key 0x4F; key 0x51
sleep 4
last | tee "$OUT/move.txt"
shot 5-moved

echo "== Escape closes"
key 0x29
sleep 6
has "Applications view closed" && ok "closed" || bad "Escape did not close it"
shot 6-closed

echo "== the dock's button opens it; a press on empty space closes it"
x=${BTN%,*}; y=${BTN#*,}
press "$x" "$y"
sleep 10
[ "$(g "grep -c 'Applications view open' $S/dock.log")" = 2 ] && ok "opened from the dock" || bad "the button did not open it"
shot 7-from-dock
press 40 300
sleep 8
[ "$(g "grep -c 'Applications view closed' $S/dock.log")" = 2 ] && ok "a press on empty space closed it" || bad "did not close on empty space"

echo "== logs"
g "for f in frescod wm dock; do echo -- \$f; grep -v '^\$' $S/\$f.log | grep -v 'inject' | tail -12; done"
echo "apps-grid: $fails failed"
[ "$fails" = 0 ]
