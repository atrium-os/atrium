#!/bin/sh
# Multi-monitor chaos test: random plug / unplug / dropout / swap / burst /
# arrange / mirror storms under a running frescod + forum-wm with two window clients,
# checking invariants after each step settles. Runs IN the FreeBSD guest, from
# the VM test layout (/root/s: up.sh, modes-run.sh, forum-ctl, demos); see
# RUNBOOK.md (display tests). STEPS / SEED pick the storm.
sh /root/s/up.sh >/dev/null 2>&1
sysctl -n dev.atrium_gpu_amd.0.build_id
cd /root/s
./fresco-renderer --seed-kernels /usr/local/share/atrium/fresco-kernels >/dev/null 2>&1
rm -f /var/db/atrium/root/forum-displays.state
: > /var/log/atrium/atrium-frescod.log; service atrium-jaild start >/dev/null 2>&1; sleep 2
sed -i '' 's/timeout [0-9]* .\/forum-wm/timeout 3000 .\/forum-wm/' /root/s/modes-run.sh
sh /root/s/modes-run.sh >/dev/null 2>&1
for i in 1 2; do (timeout 3000 ./atrium-window-demo /atrium/sockets/fresco/fresco.sock >/tmp/w$i.log 2>&1 &); sleep 5; done
sleep 10
export FORUM_CTL_SOCKET=/tmp/fctl.sock
D=dev.atrium_gpu_amd_display.0
C=./forum-ctl
STEPS=${STEPS:-60}
SEED=${SEED:-7}
# MIXED=1 (default) adds UI-size changes on a monitor and moving the focused
# window to the next one, so monitors run at mixed scales and windows cross
# between them (resolution-independence.md §7). MIXED=0 is the old storm.
MIXED=${MIXED:-1}
ACTIONS=$([ "$MIXED" = 1 ] && echo 10 || echo 8)
# Monitors the simulator offers on outputs 1-3 (4K and VGA left out: 4K
# needs more VRAM than a long run leaves). Code 3 (the desktop monitor) is
# on output 0 throughout.
CODES="1 4 5"
: > /tmp/sets
fails=0
plugged1=-1; plugged2=-1; plugged3=-1
get() { eval echo \$plugged$1; }
put() { eval plugged$1=$2; }
inuse() { for q in 1 2 3; do [ "$(get $q)" = "$1" ] && return 0; done; return 1; }
rnd() { awk -v s="$SEED" -v n="$1" -v k="$2" 'BEGIN{srand(s*1000+k); print int(rand()*n)}'; }

check() {
	step=$1
	ok=1
	# A monitor still within its grace is not settled yet: wait it out
	# (frames are slow in TCG and hold the frame loop), but not forever.
	w=0
	while :; do
		$C outputs > /tmp/outs 2>&1
		grep -q "(gone" /tmp/outs || break
		w=$((w+1)); [ $w -ge 15 ] && { echo "step $step: a monitor still in its grace after 30 s more"; ok=0; break; }
		sleep 2
	done
	$C surfaces > /tmp/surfs 2>&1
	ps axo comm | grep -q atrium-frescod || { echo "step $step: frescod DEAD"; ok=0; }
	pgrep -x forum-wm >/dev/null || { echo "step $step: forum-wm DEAD"; ok=0; }
	# Every plugged monitor is on the desktop and present; nothing else is.
	want=1; for q in 1 2 3; do [ "$(get $q)" != -1 ] && want=$((want+1)); done
	have=$(grep -c "^output .*monitor [0-9a-f]*$" /tmp/outs)
	[ "$have" = "$want" ] || { echo "step $step: $have monitors present, want $want"; cat /tmp/outs; ok=0; }
	for q in 1 2 3; do
		if [ "$(get $q)" != -1 ]; then
			grep -q "^output $q .*monitor [0-9a-f]*$" /tmp/outs || { echo "step $step: output $q plugged but not shown"; ok=0; }
		fi
	done
	# No window off-screen: every document inside one present monitor.
	awk '
		FNR==NR { if ($0 ~ /\(gone/) next; split($3,wh,"[x@]"); split($5,xy,","); n++; X[n]=xy[1]; Y[n]=xy[2]; W[n]=wh[1]; H[n]=wh[2]; next }
		{ sub(/^\*/," "); if ($2!="Document") next; split($4,g,"[x+]"); w=g[1]; h=g[2]; x=g[3]; y=g[4]; inside=0
		  for (i=1;i<=n;i++) if (x>=X[i] && y>=Y[i] && x+w<=X[i]+W[i] && y+h<=Y[i]+H[i]) inside=1
		  if (!inside) { print "window " $1 " at " $4 " is on no monitor"; bad=1 } }
		END { exit bad }' /tmp/outs /tmp/surfs || ok=0
	# A set of monitors seen before comes back exactly as it was left.
	set=$(awk '/^output/ && !/gone/ {print $NF}' /tmp/outs | sort | tr '\n' ' ')
	lay=$(awk '/^output/ && !/gone/ {print $NF ":" $5 ":" $3}' /tmp/outs | sort | tr '\n' ' ')
	prev=$(grep -F "set $set|" /tmp/sets | tail -1 | cut -d'|' -f2)
	if [ -n "$prev" ] && [ "$restore" = 1 ] && [ "$prev" != "$lay" ]; then
		echo "step $step: set came back as '$lay', was '$prev'"; ok=0
	fi
	grep -vF "set $set|" /tmp/sets > /tmp/sets.n; echo "set $set|$lay" >> /tmp/sets.n; mv /tmp/sets.n /tmp/sets
	[ $ok = 1 ] || fails=$((fails+1))
}

# The whole storm, drawn up front from one seeded stream: output, action,
# and a coin for each step.
awk -v s="$SEED" -v n="$STEPS" -v a="$ACTIONS" 'BEGIN{srand(s); for(i=0;i<n;i++) print int(rand()*3)+1, int(rand()*a), int(rand()*2), int(rand()*4)}' > /tmp/plan
i=0
exec 3< /tmp/plan
while read o a coin pick <&3; do
	i=$((i+1))
	[ $a = 6 ] && a=0	# plugs twice as likely
	cur=$(get $o)
	restore=0
	case $a in
	0|1)	# plug a monitor not in use, or swap the one there
		c=""; free=""
		for k in $CODES; do inuse $k || free="$free $k"; done
		set -- $free
		[ $# -gt 0 ] && { shift $(( pick % $# )); c=$1; }
		if [ -n "$c" ]; then sysctl $D.sim_plug_at=$o:$c >/dev/null; put $o $c; what="plug $c on $o"
		else what="(no free monitor)"; fi ;;
	2)	# unplug
		sysctl $D.sim_plug_at=$o:-1 >/dev/null; put $o -1; what="unplug $o"; restore=1 ;;
	3)	# dropout: gone and back within the grace
		if [ "$cur" != -1 ]; then
			sysctl $D.sim_plug_at=$o:-1 >/dev/null; sleep 1; sysctl $D.sim_plug_at=$o:$cur >/dev/null; what="dropout on $o"; restore=1
		else what="(dropout on empty $o)"; fi ;;
	4)	# burst: several edges inside the settle window, ends plugged
		if [ "$cur" != -1 ]; then
			for b in 1 2 3; do sysctl $D.sim_plug_at=$o:-1 >/dev/null; sysctl $D.sim_plug_at=$o:$cur >/dev/null; done; what="burst on $o"; restore=1
		else what="(burst on empty $o)"; fi ;;
	5)	# rearrange: put a plugged output left of / below output 0
		if [ "$cur" != -1 ]; then
			s=$( [ $coin = 0 ] && echo left-of || echo below )
			$C arrange $o $s 0 >/dev/null 2>&1; what="arrange $o $s 0"
		else what="(arrange empty $o)"; fi ;;
	7)	# mirror output 0 on it, or stop mirroring
		if [ "$cur" != -1 ]; then
			if $C outputs | grep -A1 "^output $o " | grep -q "mirrors monitor"; then
				$C mirror $o off >/dev/null 2>&1; what="mirror $o off"
			else
				$C mirror $o 0 >/dev/null 2>&1; what="mirror $o 0"
			fi
		else what="(mirror empty $o)"; fi ;;
	8)	# another UI size on it: a monitor of another scale than its neighbours
		if [ "$cur" != -1 ]; then
			set -- smaller default larger 1.5; shift $pick; f=$1
			$C ui-size $o $f >/dev/null 2>&1; what="ui-size $o $f"
		else what="(ui-size empty $o)"; fi ;;
	9)	# the focused window to the next monitor (maybe of another scale)
		$C move-monitor >/dev/null 2>&1; what="move-monitor" ;;
	esac
	sleep 9
	echo "step $i: $what"
	check $i
done
echo "== windows after the storm"; $C outputs; $C surfaces
grep -E "flip on output|reconfiguration failed|stays dark|refused" /var/log/atrium/atrium-frescod.log | sort | uniq -c | head
echo "chaos: $STEPS steps, $fails failed"
