#!/bin/sh
# Game spanning (fresco-span.md) end to end in the FreeBSD guest: three
# 1080p monitors made a span group with a 40 px bezel gap, a window zoomed
# over them, then a short dropout, flapping, a member gone for good and
# the un-zoom, checking what frescod, Forum and the window see. The window
# is atrium-span-probe (a test game whose columns encode their x); the host
# screendumps each monitor at SPAN-DIRECT-MARK and checks with
# scripts/span-probe-check.py that the members show it continuously. Runs IN
# the guest from the VM test layout (/root/s: up.sh, modes-run.sh, forum-ctl,
# atrium-span-probe, praecod); see RUNBOOK.md (display tests). ~14 min in TCG.
sh /root/s/up.sh >/dev/null 2>&1
sysctl -n dev.atrium_gpu_amd.0.build_id
cd /root/s
cp fresco-renderer /root/fr-bundle/bin/ && ./fresco-renderer --seed-kernels /root/fr-bundle/usr/local/share/fresco-kernels && ./opifex install /root/fr-bundle 2>&1 | tail -1
./fresco-renderer --seed-kernels /usr/local/share/atrium/fresco-kernels >/dev/null 2>&1
rm -f /var/db/atrium/root/forum-displays.state
: > /var/log/atrium/atrium-frescod.log; service atrium-jaild start >/dev/null 2>&1; sleep 2
sed -i '' 's/timeout [0-9]* .\/forum-wm/timeout 2500 .\/forum-wm/' /root/s/modes-run.sh
# Praeco, with Forum (root here) a system poster, for the flapping notice.
pkill -x praecod; rm -f /tmp/praeco.sock
export PRAECO_SOCKET=/tmp/praeco.sock
(PRAECO_SYSTEM_POSTERS=0:Forum PRAECO_SUBSCRIBERS=0 ./praecod >/tmp/praecod.log 2>&1 &)
sh /root/s/modes-run.sh >/dev/null 2>&1
export FORUM_CTL_SOCKET=/tmp/fctl.sock
D=dev.atrium_gpu_amd_display.0
C=./forum-ctl
L=/var/log/atrium/atrium-frescod.log
G=/tmp/game.log
fails=0
ok() { echo "  ok $1"; }
bad() { echo "  FAIL $1"; fails=$((fails+1)); }
# The window's newest span event: "tiles present..." as true/false words.
tiles() { grep "Span" $G | tail -1 | grep -o "present: [a-z]*" | awk '{printf "%s ", $2}'; }

echo "== three 1080p monitors: the desktop one, its twin, the 60 Hz VRR model"
sysctl $D.sim_plug_at=1:5 >/dev/null; sleep 20
sysctl $D.sim_plug_at=2:6 >/dev/null; sleep 30
$C outputs
(timeout 2400 ./atrium-span-probe /atrium/sockets/fresco/fresco.sock >$G 2>&1 &); sleep 15
GAME=$($C surfaces | sed 's/^\*/ /' | awk '$2=="Document"{id=$1} END{print id}')
echo "game window $GAME"
[ -n "$GAME" ] || { echo "  FAIL no game window"; cat $G; exit 1; }

echo "== refusals: one output of another mode, unknown output"
$C span 0 9 && bad "span with output 9 accepted" || ok "unknown output refused"

echo "== span 0 1 2 gap 40, zoom the game"
$C span 0 1 2 gap 40 || bad "span group refused"
$C spans
$C focus $GAME; sleep 3; $C zoom; sleep 30
$C spans
$C outputs
grep -E "frescod: (window [0-9]+ spans|span)" $L | tail -3
sysctl -n $D.outputs 2>/dev/null | head -4
$C outputs | grep -q "^output 1 .* at 1960,0" && $C outputs | grep -q "^output 2 .* at 3920,0" \
	&& ok "members moved apart by the bezels (1960, 3920)" || bad "members not moved for the bezels"
[ "$(tiles)" = "true true true " ] && ok "window told three tiles" || bad "tiles: $(tiles)"
grep "Span" $G | tail -1 | grep -q "hidden: \[\[1920, 0, 40, 1080\], \[3880, 0, 40, 1080\]\]" \
	&& ok "window told the bezel strips" || { bad "hidden strips"; grep "Span" $G | tail -1; }
$C surfaces | grep -E "^\*? *$GAME " | grep -q "5840x1080+0+0" && ok "the window covers 5840x1080" \
	|| { bad "window rect"; $C surfaces | grep -E " $GAME "; }
echo "== frames flow: 20 s, no refused group flip"
echo "SPAN-DIRECT-MARK (host: screendump the pipes now)"
sleep 45
grep -q "span flip refused" $L && bad "a group flip was refused" || ok "no group flip refused"
echo "== direct scanout: composed while the strip shows, then the window straight"
grep -q "span: composed (the strip is shown)" $L && ok "composed while the strip shows" || bad "no strip compose"
grep -q "span: direct scanout" $L && ok "members scan out the window directly" || bad "never direct"
grep -E "frescod: span: (direct|composed)" $L | tail -3

echo "== short dropout: output 2 out 10 s"
sysctl $D.sim_plug_at=2:-1 >/dev/null; sleep 12
[ "$(tiles)" = "true true false " ] && ok "tile 3 dark while gone" || bad "dropout tiles: $(tiles)"
$C spans | grep -q "window $GAME spans" && ok "still spanning" || bad "span lost in a dropout"
sysctl $D.sim_plug_at=2:6 >/dev/null; sleep 20
[ "$(tiles)" = "true true true " ] && ok "tile 3 back" || bad "return tiles: $(tiles)"

echo "== flapping: output 1 drops 3 times in a minute"
for i in 1 2 3; do sysctl $D.sim_plug_at=1:-1 >/dev/null; sleep 4; sysctl $D.sim_plug_at=1:5 >/dev/null; sleep 6; done
sleep 2
grep -q "held out" $L && ok "held out ($(grep 'held out' $L | tail -1 | cut -c1-80))" || bad "not held out"
[ "$(tiles)" = "true false true " ] && ok "its tile stays dark while held" || bad "held tiles: $(tiles)"
sleep 25
grep -q "told the user: A monitor keeps disconnecting" /tmp/wm.log && ok "Forum told the user ($(grep 'told the user' /tmp/wm.log | tail -1 | cut -c1-70))" \
	|| { bad "no flapping notice"; grep -E "tell the user|praeco" /tmp/wm.log | tail -2; tail -3 /tmp/praecod.log; }
grep -q "it rejoins" $L && ok "rejoined after staying up" || bad "never rejoined"
[ "$(tiles)" = "true true true " ] && ok "all three tiles again" || bad "rejoin tiles: $(tiles)"

echo "== gone: output 2 out 45 s (past the 30 s span grace)"
sysctl $D.sim_plug_at=2:-1 >/dev/null; sleep 50
$C spans; $C outputs
[ "$(tiles)" = "true true " ] && ok "the span went on over the two left" || bad "after gone: $(tiles)"
$C surfaces | grep -E "^\*? *$GAME " | grep -q "3880x1080+0+0" && ok "one resize to 3880x1080" \
	|| { bad "window rect after gone"; $C surfaces | grep -E " $GAME "; }
sysctl $D.sim_plug_at=2:6 >/dev/null; sleep 40
[ "$(tiles)" = "true true true " ] && ok "it came back: three tiles again" || bad "after return: $(tiles)"

echo "== un-zoom: the span ends, the monitors go back"
$C zoom; sleep 30
grep "Span" $G | tail -1 | grep -q "tiles: \[\]" && ok "window told the span ended" || bad "no end event"
$C outputs | grep -q "^output 1 .* at 1920,0" && $C outputs | grep -q "^output 2 .* at 3840,0" \
	&& ok "monitors back in their places" || { bad "monitors not back"; $C outputs; }
sysctl -n $D.outputs 2>/dev/null | head -4

echo "== bezel in millimetres: 11 mm between pictures of 531 mm / 1920 px monitors = 40 px"
$C span 0 1 2 bezel 11 || bad "bezel in mm refused"
$C spans | grep -q "bezel 11 mm" && ok "group kept in mm" || { bad "spans"; $C spans; }
$C outputs | grep -q "531x299 mm" && ok "outputs show their size" || bad "no physical size"
$C zoom; sleep 30
$C outputs | grep -q "^output 1 .* at 1960,0" && ok "11 mm moved the members 40 px apart" || { bad "mm gap"; $C outputs; }
$C zoom; sleep 20

grep -E "frescod: (span|window [0-9]+ (no longer )?spans)" $L | sort | uniq -c | sort -rn | head -12
grep -E "panicked|refused|reconfiguration failed" $L | sort | uniq -c | head
ps axo comm | grep -q atrium-frescod && ok "frescod alive" || bad "frescod DEAD"
pgrep -x forum-wm >/dev/null && ok "forum-wm alive" || bad "forum-wm DEAD"
cp $L /tmp/frescod-span-e2e.log
echo "span: $fails failed"
