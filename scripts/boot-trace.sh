#!/bin/sh
# Measure where Atrium's boot time goes (docs/spec/fast-boot.md §2, §8).
# Runs on the HOST (Mac) with the dev VM already up and reachable through
# scripts/vssh. Changes nothing in the guest but two loader tunables, which
# it takes out again at the end unless KEEP=1.
#
#   sh scripts/boot-trace.sh [out-dir]          # 3 reboots
#   RUNS=1 sh scripts/boot-trace.sh             # one
#   SHUTDOWN=1 sh scripts/boot-trace.sh         # power the VM off when done
#
# Per reboot it keeps, in out-dir/run-N/:
#   serial.txt     the serial console, each line stamped with host time
#                  (seconds since the reboot was asked for). Needs VMDIR's
#                  serial.sock free; skipped if another reader holds it.
#   boottrace.txt  kern.boottrace.log: the kernel's milestones and the start
#                  and end of every rc.d script (rc.subr records them when
#                  kern.boottrace.enabled=1).
#   sockets.txt    birth time of every Atrium socket = when each service
#                  bound, the moment it could first answer.
#   ps.txt         every process's start, its jail, and its command line.
#   atrium-logs/   /var/log/atrium/*.log (bootstrap's launch times, retries).
#   dmesg.txt      dmesg -a (kernel lines carry timestamps).
#   summary.txt    the above folded onto one clock: seconds since the
#                  kernel's boottime.
# and once, in out-dir/: rc.conf, loader.conf, the rcorder order, services.d.
#
# Note for the Mac's /bin/sh (bash 3.2): no `case` inside $( ).
set -u
ROOT=$(cd "$(dirname "$0")/.." && pwd)
OUT=${1:-$ROOT/scratch/boot-trace-$(date +%Y%m%d-%H%M%S)}
RUNS=${RUNS:-3}
VMDIR=${VMDIR:-$HOME/atrium-vm}
SETTLE=${SETTLE:-45}
mkdir -p "$OUT"
V() { VSSH_CONNECT_TIMEOUT=${VSSH_CONNECT_TIMEOUT:-3} "$ROOT/scripts/vssh" "$@"; }
now() { python3 -c 'import time; print("%.3f" % time.time())'; }

V true || { echo "boot-trace: the VM does not answer on ssh (start it first)"; exit 1; }

echo "=== guest configuration"
V 'cat /etc/rc.conf /etc/rc.conf.local 2>/dev/null' > "$OUT/rc.conf"
V 'cat /boot/loader.conf' > "$OUT/loader.conf"
V 'rcorder /etc/rc.d/* /usr/local/etc/rc.d/* 2>/dev/null' > "$OUT/rcorder.txt"
V 'service -e' > "$OUT/enabled.txt"
V 'ls -l /etc/atrium/services.d/; head -n 400 /etc/atrium/services.d/*.toml' > "$OUT/services.d.txt" 2>&1
V 'uname -a; sysctl -n hw.ncpu hw.physmem' > "$OUT/uname.txt"

echo "=== turning on boottrace + kernel message timestamps"
ADDED=$(V 'a=""; for t in kern.boottrace.enabled=1 kern.msgbuf_show_timestamp=1; do
             k=${t%%=*}; grep -q "^$k=" /boot/loader.conf || { echo "$t" >> /boot/loader.conf; a="$a $k"; }; done; echo $a')
echo "  added to loader.conf:${ADDED:- (already there)}"

i=1
while [ "$i" -le "$RUNS" ]; do
    R="$OUT/run-$i"; mkdir -p "$R/atrium-logs"
    echo "=== run $i: reboot"
    T0=$(now)
    SER=""
    if [ -S "$VMDIR/serial.sock" ]; then
        python3 - "$VMDIR/serial.sock" "$T0" > "$R/serial.txt" 2>/dev/null <<'EOF' &
import socket, sys, time
s = socket.socket(socket.AF_UNIX); s.settimeout(240)
try: s.connect(sys.argv[1])
except OSError: sys.exit(0)
t0 = float(sys.argv[2]); buf = b""; end = time.time() + 240
while time.time() < end:
    try: d = s.recv(4096)
    except OSError: break
    if not d: break
    buf += d
    while b"\n" in buf:
        line, buf = buf.split(b"\n", 1)
        print("%8.3f %s" % (time.time() - t0, line.decode("utf-8", "replace").rstrip("\r")), flush=True)
EOF
        SER=$!
        disown "$SER" 2>/dev/null || true
    fi
    V 'shutdown -r now' >/dev/null 2>&1
    sleep 5
    # Back on ssh = sshd up; that is NOT boot done, so we also wait for rc to
    # finish (boottrace's last rc entry) and then SETTLE more seconds for the
    # Atrium tree.
    while ! V true 2>/dev/null; do sleep 1; done
    TSSH=$(now)
    echo "  ssh back after $(python3 -c "print('%.1f' % ($TSSH - $T0))") s (host clock, includes shutdown + firmware + loader)"
    sleep "$SETTLE"
    V 'sysctl -n kern.boottrace.log' > "$R/boottrace.txt" 2>&1
    V 'sysctl -n kern.boottime; date +%s' > "$R/boottime.txt"
    V 'for f in /atrium/sockets/*/*.sock /atrium/sockets/*.sock /var/run/atrium/*.sock /var/run/atrium/*/*.sock /var/run/aqueduct/*.sock /var/run/*.ctl; do
         [ -S "$f" ] && stat -f "%.6FB %N" "$f"; done 2>/dev/null | sort -n' > "$R/sockets.txt"
    V 'ps -axww -o lstart= -o etimes= -o pid= -o jid= -o user= -o command=' > "$R/ps.txt"
    V 'dmesg -a' > "$R/dmesg.txt"
    # This boot's kernel (uname.txt above is the one before the first reboot).
    V 'uname -a; sysctl -n kern.bootfile' > "$R/uname.txt"
    V 'cd /var/log/atrium 2>/dev/null && for f in *.log; do echo "== $f"; tail -n 200 "$f"; done' > "$R/atrium-logs/all.txt" 2>&1
    [ -n "$SER" ] && kill "$SER" 2>/dev/null
    python3 - "$R" <<'EOF' > "$R/summary.txt"
import os, re, sys
R = sys.argv[1]
bt = open(os.path.join(R, "boottime.txt")).read()
m = re.search(r"sec = (\d+), usec = (\d+)", bt)
boot = int(m.group(1)) + int(m.group(2)) / 1e6 if m else None
print("kernel boottime (epoch):", boot)
print("\n-- sockets: seconds after kernel boottime that each was bound")
for line in open(os.path.join(R, "sockets.txt")):
    p = line.split(None, 1)
    if len(p) == 2 and boot:
        d = float(p[0]) - boot
        # A socket older than this boot was left by an earlier one: its
        # service did not bind this boot (not running, or not restarted).
        print("%8.2f  %s%s" % (d, p[1].strip(), "   (stale: not bound this boot)" if d < 0 else ""))
print("\n-- kern.boottrace.log in log order (deltas are ms; absolute times before sysinit 0x4800000 are not meaningful)")
rows = []
for line in open(os.path.join(R, "boottrace.txt")):
    rows.append(line.rstrip())
for r in rows[:400]:
    print(r)
EOF
    echo "  kept $R"
    i=$((i + 1))
done

if [ "${KEEP:-0}" != 1 ] && [ -n "$ADDED" ]; then
    echo "=== taking the tunables out again:$ADDED"
    for k in $ADDED; do V "sed -i '' '/^$k=/d' /boot/loader.conf"; done
fi
if [ "${SHUTDOWN:-0}" = 1 ]; then
    echo "=== shutting the VM down"
    sh "$ROOT/scripts/vshutdown"
fi
echo "boot-trace: done, results in $OUT"
