#!/bin/sh
# Lookup-vs-namespace deadlock soak — the workload the CRASH soaks cannot run.
#
# WHY THIS EXISTS
#
#   68ca5329 fixed a lock-order inversion: vop_lookup's retry path held the
#   flush gate across tessera_vget (GATE -> VNODE), while VFS enters
#   VOP_CREATE/MKDIR/REMOVE/RMDIR/LINK/RENAME with dvp already exclusively
#   locked and those vops take the gate inside (VNODE -> GATE).
#
#   Neither crash soak could ever have caught it. Both run their namespace ops
#   SERIALLY, and neither runs a traversal concurrently with them on the same
#   directory. The inversion needs a lookup that MISSES and RETRIES (so it
#   takes the gate) racing a namespace op on the same parent, with GC pressure
#   to make flushes frequent enough that the gate is actually contended.
#
#   So this harness deliberately builds that race and leaves it running for
#   hours:
#       T traversal loops   - find(1) over the whole tree, so lookups miss
#       C churn loops       - mkdir/touch/ln/mv/rm in the SAME directories
#       1 GC loop           - tq, to keep flushes and the gate busy
#
#   Near-band operation is part of the trigger, not decoration: meta pressure
#   is what makes the flush path re-enter often. meta_admit_resv/band_floor
#   are dropped to 0 so the volume can run right up against the band.
#
# WHAT COUNTS AS A FAILURE
#
#   The bug's signature is NOT a panic and NOT fsck damage — it is the guest
#   going unresponsive while every CPU is idle. So the oracle is liveness:
#   a long-ConnectTimeout ssh probe (scripts/vssh's ConnectTimeout=3 would
#   report a 4-second stall and a dead kernel identically). The FIRST time the
#   guest fails to answer within STALL_TIMEOUT, this breaks into ddb and dumps
#   exactly what names the culprit -- ps, the backtrace of the blocked thread,
#   and show lockedvnods -- then stops. A capture is the deliverable; ddb
#   perturbs the machine afterwards, so there is no point continuing past it.
#
#   fsck runs at the end too, but only as a secondary check: a clean fsck here
#   proves nothing about the deadlock.
#
#   DURATION=7200 sh scripts/vm-lookup-deadlock-soak.sh
set -u
BSD="$(cd "$(dirname "$0")/.." && pwd)"
OUT=${OUT:-/tmp/tessera-lookup-soak}; mkdir -p "$OUT"
DEV=/dev/vtbd2; M=/mnt/scratch
DURATION=${DURATION:-7200}          # seconds of concurrent churn
DIRS=${DIRS:-400}; FILES=${FILES:-60}   # ~24k files, ~150k dirents after churn
TRAVERSALS=${TRAVERSALS:-3}; CHURNERS=${CHURNERS:-3}
PROBE_INTERVAL=${PROBE_INTERVAL:-5}
STALL_TIMEOUT=${STALL_TIMEOUT:-90}  # ssh unanswered this long => capture
VSSH="$BSD/scripts/vssh"

KO="$BSD/atrium-tessera/kmod/tessera_fs.ko"
[ -f "$KO" ] || { echo "no $KO — build it first"; exit 1; }
KMOD=$(shasum -a 256 "$KO" | cut -c1-16)

# ★ #129: destructive work ONLY on the disk whose GEOM ident says so.
GATE="diskinfo -s $DEV | grep -q '^atrium-scratch\$' || { echo REFUSING_ident; exit 2; }"

# ★ long ConnectTimeout — the whole point of the liveness oracle.
slowssh() {
    ssh -i "$HOME/.ssh/fresco_bsd_ed25519" -o StrictHostKeyChecking=no \
        -o UserKnownHostsFile=/dev/null -o LogLevel=ERROR \
        -o ConnectTimeout="$STALL_TIMEOUT" -o ServerAliveInterval=0 \
        -p 2222 root@localhost "$@"
}

k=$($VSSH "sha256 -q /boot/kernel/tessera_fs.ko | cut -c1-16" 2>/dev/null | tr -d '\r')
[ "$k" = "$KMOD" ] || { echo "ABORT: guest module [$k] != tree [$KMOD]"; exit 1; }
echo "=== LOOKUP-DEADLOCK SOAK $(date) kmod=$KMOD dur=${DURATION}s ==="
echo "    traversals=$TRAVERSALS churners=$CHURNERS dirs=$DIRS files=$FILES -> $OUT"

echo "--- building the fixture (this is the slow part) ---"
$VSSH "$GATE
  mount | grep -q ' $M ' && umount $M
  mkdir -p $M
  mkfs-tessera $DEV >/dev/null 2>&1 && mount -t tessera $DEV $M || { echo MKFS_FAIL; exit 3; }
  i=0
  while [ \$i -lt $DIRS ]; do
    d=$M/t\$((i/20))/d\$i; mkdir -p \$d
    j=0; while [ \$j -lt $FILES ]; do echo x > \$d/f\$j; j=\$((j+1)); done
    i=\$((i+1))
  done
  sync
  echo \"seeded entries=\$(find $M | wc -l | tr -d ' ')\"" 2>&1 | tr -d '\r' | tee "$OUT/seed.log" | tail -2
grep -q seeded "$OUT/seed.log" || { echo "ABORT: fixture build failed"; exit 1; }

echo "--- arming the workload ---"
$VSSH "$GATE
  # near-band: let the volume run up against the emergency band, which is what
  # keeps the flush path (and therefore the gate) hot.
  sysctl kern.tessera.meta_admit_resv=0 kern.tessera.meta_band_floor=0 >/dev/null 2>&1
  rm -f /root/soak.stop; : > /root/soak.err
  # traversal loops: force lookups that MISS and RETRY under gate contention
  i=1; while [ \$i -le $TRAVERSALS ]; do
    nohup sh -c 'while [ ! -f /root/soak.stop ]; do find $M -type f >/dev/null 2>&1; done' \
      >/dev/null 2>&1 & i=\$((i+1))
  done
  # churn loops: namespace ops in the SAME directories the traversals walk
  i=1; while [ \$i -le $CHURNERS ]; do
    nohup sh -c 'n=\$1; k=0; while [ ! -f /root/soak.stop ]; do
        k=\$(( (k+7) % $DIRS ))
        d=$M/t\$((k/20))/d\$k
        mkdir -p \$d/n\$n 2>>/root/soak.err
        echo y > \$d/n\$n/a 2>>/root/soak.err
        ln \$d/n\$n/a \$d/n\$n/b 2>>/root/soak.err
        mv \$d/n\$n/b \$d/n\$n/c 2>>/root/soak.err
        rm -rf \$d/n\$n 2>>/root/soak.err
      done' _ \$i >/dev/null 2>&1 & i=\$((i+1))
  done
  # GC loop
  nohup sh -c 'while [ ! -f /root/soak.stop ]; do /root/tq $M >/dev/null 2>&1; done' \
    >/dev/null 2>&1 &
  sleep 2; echo \"armed procs=\$(pgrep -c -f soak.stop)\"" 2>&1 | tr -d '\r' | tail -1

START=$(python3 -c 'import time;print(int(time.time()))')
END=$((START+DURATION))
echo "--- probing liveness every ${PROBE_INTERVAL}s until $(date -r $END) ---"
captured=0; probes=0; slow=0
while [ "$(python3 -c 'import time;print(int(time.time()))')" -lt $END ]; do
    probes=$((probes+1))
    S=$(python3 -c 'import time;print(time.time())')
    if slowssh 'echo ok' >/dev/null 2>&1; then R=ok; else R=FAIL; fi
    E=$(python3 -c 'import time;print(time.time())')
    D=$(python3 -c "print(f'{$E-$S:.2f}')")
    over=$(python3 -c "print(1 if $E-$S > 2.0 else 0)")
    if [ "$over" = 1 ] || [ "$R" != ok ]; then
        slow=$((slow+1)); echo "probe $probes: ${D}s $R" | tee -a "$OUT/stalls.log"
    fi
    [ "$R" = ok ] && { sleep "$PROBE_INTERVAL"; continue; }

    # ---- STALL: passive state FIRST (ddb perturbs CPU%), then ddb ----
    echo "########## STALL at probe $probes ($(date)) ##########" | tee -a "$OUT/capture"
    {
        echo "qemu cpu: $(ps -axo %cpu,command | grep '[q]emu-system-aarch64' | head -1 | awk '{print $1}')%"
        echo "--- per-vCPU PC (passive, pre-ddb) ---"
        printf 'info registers -a\n' | timeout 15 nc -U -w 6 /tmp/qmp.sock 2>/dev/null \
            | grep -E "^CPU#|^ PC=" | head -12
    } >>"$OUT/capture" 2>&1
    timeout 60 python3 "$BSD/scripts/ddb_session.py" break >/dev/null 2>&1
    timeout 180 python3 "$BSD/scripts/ddb_session.py" "ps" 15 >"$OUT/ps" 2>&1
    {
        echo "--- blocked threads ---"
        grep -E "tessgat|tessflsh|tessckpt|getblk|biord|biowait|vnode|tessgc" "$OUT/ps" | head -20
    } >>"$OUT/capture" 2>&1
    for w in tessgat getblk biord; do
        TID=$(grep -E "$w" "$OUT/ps" | head -1 | awk '{print $1}')
        [ -n "$TID" ] || continue
        echo "--- bt of first $w waiter (tid $TID) ---" >>"$OUT/capture"
        timeout 180 python3 "$BSD/scripts/ddb_session.py" "bt $TID" 15 >>"$OUT/capture" 2>&1
    done
    timeout 180 python3 "$BSD/scripts/ddb_session.py" "show lockedvnods" 15 >"$OUT/lockedvnods" 2>&1
    captured=1
    echo "CAPTURED -> $OUT/capture, $OUT/ps, $OUT/lockedvnods"
    break
done

echo "--- stopping the workload ---"
if [ $captured = 0 ]; then
    $VSSH "touch /root/soak.stop; sleep 5
      pkill -f soak.stop >/dev/null 2>&1
      echo \"errs=\$(wc -l < /root/soak.err | tr -d ' ') entries=\$(find $M | wc -l | tr -d ' ')\"
      sync; umount $M 2>/dev/null || echo UMOUNT_FAIL
      $GATE
      tessera-fsck $DEV > /root/l.fsck 2>&1
      echo \"fsck_problems=\$(grep -ciE 'dangling|orphan|nlink|leaked|overlap|missing|neither|corrupt|problem' /root/l.fsck)\"" \
      2>&1 | tr -d '\r' | tee -a "$OUT/final.log"
fi

echo "########## RESULT ##########"
echo "  probes=$probes  slow(>2s)=$slow  ran=$(( $(python3 -c 'import time;print(int(time.time()))') - START ))s"
if [ $captured = 1 ]; then
    echo "  VERDICT: STALL CAPTURED — the hang is NOT fixed. See $OUT/capture"
    exit 1
fi
echo "  VERDICT: no stall in ${DURATION}s of concurrent traversal+churn near the band"
grep -E "errs=|fsck_problems=" "$OUT/final.log" 2>/dev/null | tail -2
exit 0
