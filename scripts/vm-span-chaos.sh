#!/bin/sh
# Span chaos (fresco-span.md S5): a game (atrium-span-probe) spanning three
# 1080p monitors through STEPS random steps: zoom toggles, short dropouts,
# HPD glitches, flapping bursts, a member gone past the 30 s grace, bezel
# changes (pixels and millimetres). After each step settles: frescod and
# Forum alive, nothing panicked, and, while a span is up, the window as wide
# as the tiles frescod last told it and every member it names on the desktop.
# Ends zoomed over all three at CHAOS-END-MARK, where the host screendumps the
# pipes for scripts/span-probe-check.py. Runs IN the guest from the VM test
# layout (/root/s: up.sh, modes-run.sh, forum-ctl, atrium-span-probe); see
# RUNBOOK.md (display tests). STEPS / SEED pick the storm; ~25 min in TCG.
sh /root/s/up.sh >/dev/null 2>&1
sysctl -n dev.atrium_gpu_amd.0.build_id
cd /root/s
./fresco-renderer --seed-kernels /usr/local/share/atrium/fresco-kernels >/dev/null 2>&1
rm -f /var/db/atrium/root/forum-displays.state
: > /var/log/atrium/atrium-frescod.log; service atrium-jaild start >/dev/null 2>&1; sleep 2
sed -i '' 's/timeout [0-9]* .\/forum-wm/timeout 4000 .\/forum-wm/' /root/s/modes-run.sh
sh /root/s/modes-run.sh >/dev/null 2>&1
export FORUM_CTL_SOCKET=/tmp/fctl.sock
D=dev.atrium_gpu_amd_display.0
C=./forum-ctl
L=/var/log/atrium/atrium-frescod.log
G=/tmp/game.log
STEPS=${STEPS:-60}
SEED=${SEED:-11}
fails=0
rnd() { awk -v s="$SEED" -v n="$1" -v k="$2" 'BEGIN{srand(s*1000+k); print int(rand()*n)}'; }
# Plug codes: output 1 the desktop monitor's twin, output 2 the VRR one.
code() { [ "$1" = 1 ] && echo 5 || echo 6; }

sysctl $D.sim_plug_at=1:5 >/dev/null; sleep 20
sysctl $D.sim_plug_at=2:6 >/dev/null; sleep 30
(timeout 4000 ./atrium-span-probe /atrium/sockets/fresco/fresco.sock >$G 2>&1 &); sleep 15
GAME=$($C surfaces | sed 's/^\*/ /' | awk '$2=="Document"{id=$1} END{print id}')
echo "game window $GAME"
[ -n "$GAME" ] || { echo "no game window"; cat $G; exit 1; }
$C span 0 1 2 gap 40 >/dev/null || { echo "span group refused"; exit 1; }
$C focus $GAME >/dev/null; sleep 3; $C zoom >/dev/null; sleep 30
zoomed=1

check() {
	step=$1
	ok=1
	ps axo comm | grep -q atrium-frescod || { echo "step $step: frescod DEAD"; ok=0; }
	pgrep -x forum-wm >/dev/null || { echo "step $step: forum-wm DEAD"; ok=0; }
	grep -q "panicked" $L && { echo "step $step: frescod panicked: $(grep panicked $L | head -1)"; ok=0; }
	if $C spans 2>/dev/null | grep -q "window $GAME spans"; then
		# The window is as wide as the tiles it was last told.
		ext=$(grep "Span" $G | tail -1 | grep -o "rect: \[[0-9, ]*\]" | tr -d 'rect:[],' |
			awk '{ if ($1 + $3 > m) m = $1 + $3 } END { print m + 0 }')
		w=$($C surfaces | sed 's/^\*/ /' | awk -v g=$GAME '$1==g{split($4, a, "[x+]"); print a[1]}')
		[ "$ext" = "$w" ] || { echo "step $step: window $w wide, tiles reach $ext"; ok=0; }
	elif [ $zoomed = 1 ]; then
		# Zoomed but not spanning: only while fewer than two members are here.
		n=$($C outputs | grep -c "^output [012] ")
		[ "$n" -lt 2 ] || grep -q "span refused" /tmp/wm.log || { echo "step $step: zoomed, $n members here, no span"; ok=0; }
	fi
	[ $ok = 1 ] || fails=$((fails+1))
}

i=0
while [ $i -lt "$STEPS" ]; do
	i=$((i+1))
	k=$(rnd 12 $i)
	m=$(( $(rnd 2 $((i+500))) + 1 ))
	case $k in
	0|1)	what="zoom"; $C zoom >/dev/null; zoomed=$((1 - zoomed)); sleep 20 ;;
	2|3|4)	t=$(( $(rnd 6 $((i+900))) + 3 )); what="output $m out ${t} s"
		sysctl $D.sim_plug_at=$m:-1 >/dev/null; sleep $t; sysctl $D.sim_plug_at=$m:$(code $m) >/dev/null; sleep 20 ;;
	5|6)	what="output $m HPD glitch"
		sysctl $D.sim_plug_at=$m:-1 >/dev/null; sysctl $D.sim_plug_at=$m:$(code $m) >/dev/null; sleep 10 ;;
	7)	what="output $m flapping"
		for f in 1 2 3; do sysctl $D.sim_plug_at=$m:-1 >/dev/null; sleep 3; sysctl $D.sim_plug_at=$m:$(code $m) >/dev/null; sleep 4; done
		sleep 20 ;;
	8)	what="output $m gone 35 s"
		sysctl $D.sim_plug_at=$m:-1 >/dev/null; sleep 35; sysctl $D.sim_plug_at=$m:$(code $m) >/dev/null; sleep 35 ;;
	9)	g=$(( $(rnd 3 $((i+1300))) * 20 )); what="gap $g px"; $C span 0 1 2 gap $g >/dev/null; sleep 25 ;;
	10)	what="bezel 11 mm"; $C span 0 1 2 bezel 11 >/dev/null; sleep 25 ;;
	11)	what="focus the game"; $C focus $GAME >/dev/null; sleep 8 ;;
	esac
	before=$fails
	check $i
	[ $fails = $before ] && r=ok || r=FAIL
	echo "step $i: $what: $r"
done

echo "== end: all three back, gap 40, zoomed over them"
sysctl $D.sim_plug_at=1:5 >/dev/null; sysctl $D.sim_plug_at=2:6 >/dev/null; sleep 40
$C span 0 1 2 gap 40 >/dev/null; sleep 20
[ $zoomed = 1 ] || { $C focus $GAME >/dev/null; sleep 3; $C zoom >/dev/null; }
sleep 40
$C outputs | grep "^output"
grep "Span" $G | tail -1 | cut -c1-200
grep -q "span: direct scanout" $L && echo "  ok members scanned the game out directly" || { echo "  FAIL never direct"; fails=$((fails+1)); }
echo "CHAOS-END-MARK (host: screendump the pipes now)"
sleep 40
grep -E "frescod: span|frescod: window [0-9]+ (no longer )?spans" $L | sed 's/[0-9a-f]\{16\}/M/g' | sort | uniq -c | sort -rn | head -12
grep -c "told the user" /tmp/wm.log | sed 's/^/flapping notices posted or tried: /'
ps axo comm | grep -q atrium-frescod && echo "  ok frescod alive" || { echo "  FAIL frescod DEAD"; fails=$((fails+1)); }
echo "span chaos: $STEPS steps, $fails failed"
