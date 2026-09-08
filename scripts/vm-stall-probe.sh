#!/bin/sh
# Stall probe — runs ALONGSIDE a long soak to catch the rare multi-second stall
# that makes ssh fail its banner exchange (project_tessera_guest_spin_on_damaged_volume).
#
# WHY THIS EXISTS, AND WHY THE OBVIOUS PROBE LIES:
#
#   scripts/vssh sets ConnectTimeout=3. So EVERY "ssh failed" observed with it
#   means only ">3 seconds" — never "dead". Wrapping it in `timeout 90` does
#   nothing, because ssh itself gives up at 3s. Chasing this bug with vssh
#   produced a "3.03s" datum that is just the timeout constant, and an
#   "unresponsive for minutes" impression that is equally consistent with one
#   long stall or many short ones. This probe uses a LONG ConnectTimeout so it
#   measures the ACTUAL duration.
#
#   Likewise the in-guest half must time sub-second: a first version used
#   `date +%s` (1s granularity) and reported four spurious "1s" stalls from
#   execs merely straddling a second boundary.
#
# WHAT IT RECORDS
#   host  : real ssh connect+exec latency, unbounded, every INTERVAL seconds.
#   guest : fork+exec latency at 0.01s (`time -p`), and on any event > THRESH
#           a D-state thread + wchan histogram — which NAMES the blocking
#           resource, the one thing the earlier investigation never captured.
#
#   OUT=/tmp/stall-probe sh scripts/vm-stall-probe.sh &     # start
#   touch /tmp/stall-probe.stop                             # stop
set -u
BSD="$(cd "$(dirname "$0")/.." && pwd)"
OUT=${OUT:-/tmp/stall-probe}; INTERVAL=${INTERVAL:-5}; THRESH=${THRESH:-0.5}
KEY="$HOME/.ssh/fresco_bsd_ed25519"
mkdir -p "$OUT"; rm -f /tmp/stall-probe.stop

# ★ LONG ConnectTimeout — the entire point. Do NOT use scripts/vssh here.
slowssh() {
    ssh -i "$KEY" -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null \
        -o LogLevel=ERROR -o ConnectTimeout=120 -o ServerAliveInterval=0 \
        -p 2222 root@localhost "$@"
}

# guest-side probe
slowssh 'cat > /root/execprobe.sh <<EOF
#!/bin/sh
: > /tmp/slow
while [ ! -f /tmp/execprobe.stop ]; do
  r=\$(/usr/bin/time -p /usr/bin/true 2>&1 | awk "/^real/{print \\\$2}")
  if [ "\$(echo "\$r" | awk "{print (\\\$1>'"$THRESH"')?1:0}")" = "1" ]; then
    { echo "=== SLOW exec \${r}s at \$(date +%s) numvnodes=\$(sysctl -n vfs.numvnodes) free=\$(sysctl -n vfs.freevnodes) v_free=\$(sysctl -n vm.stats.vm.v_free_count)"
      ps -axo stat,wchan,comm | awk "\\\$1 ~ /^D/" | sort | uniq -c | sort -rn | head -12
    } >> /tmp/slow
  fi
  sleep 0.2
done
EOF
chmod +x /root/execprobe.sh; rm -f /tmp/execprobe.stop
nohup /root/execprobe.sh >/dev/null 2>&1 &
echo guest-probe-started' >/dev/null 2>&1

echo "stall-probe: started (host interval ${INTERVAL}s, guest threshold ${THRESH}s) -> $OUT"
while [ ! -f /tmp/stall-probe.stop ]; do
    S=$(python3 -c 'import time;print(time.time())')
    if slowssh 'echo ok' >/dev/null 2>&1; then R=ok; else R=FAIL; fi
    E=$(python3 -c 'import time;print(time.time())')
    python3 -c "
d=$E-$S
if d > 1.0 or '$R' != 'ok':
    print(f'{d:.2f}s $R', flush=True)
" >> "$OUT/ssh-stalls.log"
    sleep "$INTERVAL"
done

slowssh 'touch /tmp/execprobe.stop; sleep 1; cat /tmp/slow 2>/dev/null' > "$OUT/guest-stalls.log" 2>/dev/null
echo "stall-probe: stopped"
echo "  host  ssh stalls >1s : $(wc -l < "$OUT/ssh-stalls.log" 2>/dev/null | tr -d ' ')"
echo "  guest exec stalls    : $(grep -c 'SLOW exec' "$OUT/guest-stalls.log" 2>/dev/null || echo 0)"
