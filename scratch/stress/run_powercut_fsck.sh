#!/bin/sh
# Stable power-cut crash harness for tessera (run from the macOS HOST).
#
# Why this exists: chaining QMP `system_reset`s on one long-lived VM
# eventually wedges the guest and races sshd at boot, producing ambiguous
# crash results. This harness instead simulates each power cut with a full
# QEMU `quit` + relaunch, so every cycle runs against a PRISTINE boot.
# With crash-test.img on cache=directsync, an abrupt `quit` is a faithful
# power cut: synchronously-written sectors are already on the host disk,
# the guest buffer cache is gone. (run-vm.sh sets directsync; commit 004616f.)
#
# Each cycle: fresh mkfs on the crash-test disk (vtbd0 = tessera-crashtest,
# 256M — NOT the zfs root), do the op, power-cut, relaunch, verify recovery.
#
# Env: CYCLES (default 5), N (files, default 30), MODE = fsync|sync|nosync.
#   fsync  — create+fsync each + fsync dir  → MUST recover N/N (POSIX contract)
#   sync   — create N then sync(2)
#   nosync — create N, no durability call   → may legitimately lose data
set -u
BSD=/Users/girivs/src/bsd
CYCLES=${CYCLES:-5}; N=${N:-30}; MODE=${MODE:-fsync}
SOCK=/tmp/qmp.sock; VSSH="$BSD/scripts/vssh"

wait_ready() {
  i=0; while [ $i -lt 60 ]; do
    $VSSH 'echo ready' >/dev/null 2>&1 && return 0
    i=$((i+1)); sleep 3
  done
  echo "FATAL: VM never became ready"; exit 1
}
relaunch() {
  echo quit | nc -U -w2 $SOCK >/dev/null 2>&1
  i=0; while [ $i -lt 15 ]; do pgrep -f qemu-system-aarch64 >/dev/null || break; i=$((i+1)); sleep 2; done
  pgrep -f qemu-system-aarch64 >/dev/null && pkill -9 -f qemu-system-aarch64
  sleep 2
  ( $BSD/scripts/run-vm.sh --gpusim >/tmp/pc-vmboot.log 2>&1 & )
  wait_ready
}
guest_setup() {
  $VSSH 'kldload p9fs 2>/dev/null; mount -t p9fs -o trans=virtio bsd_share /mnt/host 2>/dev/null
    umount /mnt/tessera 2>/dev/null; kldunload tessera_fs 2>/dev/null
    kldload /mnt/host/atrium-tessera/kmod/tessera_fs.ko
    cc -O2 -o /root/fsync_durable /mnt/host/scratch/stress/fsync_durable.c 2>/dev/null
    mkdir -p /mnt/tessera' >/dev/null 2>&1
}

# SAFETY: confirm vtbd0 is the 256M crashtest disk, never the root.
wait_ready
id=$($VSSH 'diskinfo -s /dev/vtbd0' 2>/dev/null | tr -d '\r')
sz=$($VSSH 'diskinfo /dev/vtbd0' 2>/dev/null | awk '{print $3}')
echo "vtbd0 ident=[$id] size=[$sz]"
[ "$id" = "tessera-crashtest" ] || { echo "ABORT: vtbd0 is not tessera-crashtest"; exit 1; }

pass=0; fail=0
c=1; while [ $c -le $CYCLES ]; do
  guest_setup
  f1=$($VSSH "BIN=/mnt/host/atrium-tessera/rs/target/aarch64-unknown-freebsd/release
    \$BIN/mkfs-tessera --create -s 256 --seed-file h --seed-content x /dev/vtbd0 >/dev/null
    mount -t tessera /dev/vtbd0 /mnt/tessera
    case $MODE in
      fsync)  nohup /root/fsync_durable /mnt/tessera $N >/tmp/op.log 2>&1 </dev/null & sleep 3 ;;
      sync)   i=1; while [ \$i -le $N ]; do echo d-\$i > /mnt/tessera/d\$i; i=\$((i+1)); done; sync; sync; sleep 1 ;;
      nosync) i=1; while [ \$i -le $N ]; do echo d-\$i > /mnt/tessera/d\$i; i=\$((i+1)); done; sleep 1 ;;
    esac
    ls /mnt/tessera/d* 2>/dev/null | wc -l | tr -d ' '" 2>/dev/null | tail -1)
  echo "cycle $c: created=$f1 (mode=$MODE) — power cut (quit+relaunch)"
  relaunch
  got=$($VSSH 'kldload p9fs 2>/dev/null; mount -t p9fs -o trans=virtio bsd_share /mnt/host 2>/dev/null
    kldunload tessera_fs 2>/dev/null; kldload /mnt/host/atrium-tessera/kmod/tessera_fs.ko
    mkdir -p /mnt/tessera; mount -t tessera /dev/vtbd0 /mnt/tessera
    n=$(ls /mnt/tessera/d* 2>/dev/null | wc -l | tr -d " "); echo "GOT=$n"
    umount /mnt/tessera 2>/dev/null' 2>/dev/null | grep GOT= | cut -d= -f2)
  fsckp=$($VSSH "BIN=/mnt/host/atrium-tessera/rs/target/aarch64-unknown-freebsd/release; \$BIN/tessera-fsck /dev/vtbd0 2>&1 | grep -cE 'dangling|orphan|nlink|leaked|overlap'" 2>/dev/null | tail -1)
  if [ "$got" = "$N" ] && [ "${fsckp:-0}" = "0" ]; then echo "  cycle $c: recovered $got/$N fsck=clean PASS"; pass=$((pass+1))
  else echo "  cycle $c: recovered ${got:-?}/$N fsck=${fsckp:-?}  FAIL"; fail=$((fail+1)); fi
  c=$((c+1))
done
echo "=== MODE=$MODE: $pass PASS / $fail FAIL of $CYCLES cycles ==="
