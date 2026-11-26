#!/bin/sh
# Fenestra R2 bad-network gate (docs/spec/fenestra.md §8.6, §19 R2): one
# whole-desktop session (scripts/lib/fenestra-vm.sh: frescod with Sheets in
# the cloud VM, the viewer in the container) through a UDP impairment proxy
# (scripts/lib/udp-impair.py; the container has no tc) whose profile moves
# on a schedule. Typing probes time each keystroke to the first frame after
# it (click-to-frame) under each profile.
#
#   0-40 s     clean: connect, the whole state
#   40-160     added RTT 0, 50, 150, 300 ms (30 s each, 4 probes each)
#   160-250    2 % loss, 10 % loss, 1 Mbit/s each way
#   250-280    30 ms jitter (10 ms delay)
#   280-310    a 5 s blackout (285-290): the connection survives
#   310-340    the client's address changes (312): QUIC migrates
#   340-560    a 3 min blackout (345-525): the connection times out, the
#              viewer reconnects and resumes; a key pressed during it is
#              dropped, never replayed
#
# Checks: each added RTT costs about one round trip (median against the
# clean median, between 0.6x and 1x RTT + 400 ms); every probe outside the
# blackouts gets its frame; one reconnect, after the long blackout only, and
# it resumes without resending; the key pressed in the blackout never
# reaches the host; at the end the viewer's desktop equals frescod's screen.
# TCG timings: the numbers are the cost of the network, not product numbers.
#
#   NETDEV_EXTRA=hostfwd=udp:127.0.0.1:24300-:24300 sh scripts/cloud-vm.sh up
#   sh scripts/fenestra-netem-vm.sh
set -u
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
. "$ROOT/scripts/lib/gate.sh"
gate_begin fenestra-netem 15
. "$ROOT/scripts/lib/fenestra-vm.sh"
fen_setup

W=$(mktemp -d "${TMPDIR:-/tmp}/fenestra-netem.XXXXXX")
mkdir -p "$W/mint"
PROXY_PORT=$((PORT + 100))
cat >"$W/schedule.txt" <<'EOF'
0
70 delay=25
100 delay=75
130 delay=150
160 loss=0.02
190 loss=0.10
220 rate=1000
250 delay=10 jitter=30
280
285 blackout=1
290
312 rebind=1
313
345 blackout=1
525
EOF
# Click a cell at 30 s, then a digit key (press is the probe) at +8, +14,
# +20, +26 s into each 30 s segment, and one at 400 s, in the blackout.
{
    echo "30 MOVE 300 260"; echo "30.2 BTN 1 1"; echo "30.3 BTN 1 0"
    k=30
    for seg in 40 70 100 130 160 190 220 250; do
        for off in 8 14 20 26; do
            t=$((seg + off)); echo "$t KEY $k 1 PROBE"; echo "$t.1 KEY $k 0"
            k=$((k + 1)); [ $k -gt 38 ] && k=30
        done
    done
    for t in 296 302 318 324 330; do echo "$t KEY $k 1 PROBE"; echo "$t.1 KEY $k 0"; k=$((k + 1)); [ $k -gt 38 ] && k=30; done
    echo "400 KEY 4 1"; echo "400.1 KEY 4 0"
    for t in 545 552; do echo "$t KEY $k 1 PROBE"; echo "$t.1 KEY $k 0"; k=$((k + 1)); [ $k -gt 38 ] && k=30; done
} >"$W/input.txt"

python3 -I "$ROOT/scripts/lib/udp-impair.py" --listen "127.0.0.1:$PROXY_PORT" --target "127.0.0.1:$PORT" \
    --schedule "$W/schedule.txt" --log "$W/proxy.log" &
PP=$!
"$VIEW" --headless --input "$W/input.txt" --mint-dir "$W/mint" --out "$W/view" --run-secs 575 \
    --connect "127.0.0.1:$PROXY_PORT" >"$W/view.log" 2>&1 &
VP=$!
fen_mint "$W"
wait $VP
kill $PP; wait $PP 2>/dev/null
sleep 3
get "$G/screen.png" "$W/host.png"
g "cat $G/fenestrad.log" >"$W/fenestrad.log" 2>&1
g "cat $G/app.log" >"$W/app.log" 2>&1
V="$W/view/viewer.txt"
cat "$W/view.log" "$W/proxy.log" "$V"
grep 'viewer [0-9] gone' "$W/fenestrad.log"

# Probes by segment: their medians, and how many got no frame.
seg() { awk -v a="$1" -v b="$2" '$1 == "probe" && $2 >= a && $2 < b { print $3 }' "$V"; }
median() { sort -n | awk '{ v[NR] = $1 } END { if (NR == 0) print "none"; else if (NR % 2) print v[(NR + 1) / 2]; else print (v[NR / 2] + v[NR / 2 + 1]) / 2 }'; }
answered() { seg "$1" "$2" | grep -vc timeout; }
base=$(seg 40 70 | grep -v timeout | median)
echo "  click-to-frame medians (ms): clean $base, +50 $(seg 70 100 | grep -v timeout | median), +150 $(seg 100 130 | grep -v timeout | median), +300 $(seg 130 160 | grep -v timeout | median)"
check "clean: 4 probes answered" 4 "$(answered 40 70)"
for rtt in 50:70:100 150:100:130 300:130:160; do
    r=${rtt%%:*}; rest=${rtt#*:}; a=${rest%%:*}; b=${rest#*:}
    m=$(seg "$a" "$b" | grep -v timeout | median)
    v=$(awk -v m="$m" -v base="$base" -v r="$r" 'BEGIN { if (m == "none" || base == "none") print "no probes"; else { d = m - base; print (d >= 0.6 * r && d <= r + 400) ? "one round trip" : "added " d " ms" } }')
    check "+$r ms RTT costs about one round trip" "one round trip" "$v"
done
check "2 % loss: 4 probes answered" 4 "$(answered 160 190)"
check "10 % loss: 4 probes answered" 4 "$(answered 190 220)"
check "1 Mbit/s: 4 probes answered" 4 "$(answered 220 250)"
check "30 ms jitter: 4 probes answered" 4 "$(answered 250 280)"
check "after a 5 s blackout: 2 probes answered" 2 "$(answered 280 310)"
check "after an address change: 3 probes answered" 3 "$(answered 310 340)"
check "after a 3 min blackout: 2 probes answered" 2 "$(answered 530 575)"
check "one reconnect, after the long blackout" 2 "$(awk '$1 == "connections" { print $2 }' "$V")"
r2=$(grep 'viewer 2 gone' "$W/fenestrad.log" | sed 's/.*resumed \([0-9]*\) resent \([0-9]*\).*/\1 \2/')
check "the reconnect resumed without resending (resumed, resent)" "1 0" "$r2"
# Key 4 ("a") was pressed only in the blackout; the host never got it.
check "the key pressed in the blackout was not replayed" 0 "$(awk '/viewer 2 gone/ { for (i = 1; i < NF; i++) if ($i == "inputs") print ($(i+1) == 4) ? 0 : "inputs " $(i+1) }' "$W/fenestrad.log")"
compare "$W/host.png" >"$W/compare.txt"
check "the viewer's desktop equals frescod's screen at the end" equal "$(cat "$W/compare.txt")"
g "pkill -x fenestrad; pkill -x fenestra-session; pkill -x atrium-sheets; pkill -x frescod; true"
echo "  evidence in $W"
gate_end
