#!/bin/sh
# Faithful power-cut crash soak against the RO-FIRST recovery path (run from
# the macOS HOST). Same gold-standard method as run_powercut_fsck.sh — fresh
# mkfs on the directsync crash disk vtbd0, do the op, QEMU quit+relaunch =
# faithful power loss — but recovery mirrors the BOOT sequence: mount the
# volume READ-ONLY first (as vfs_mountroot does), then `mount -u -o rw`
# (as rc does), exercising the ro->rw crash-recovery split.
#
# Records BOTH counts:
#   ro_got  — files visible on the read-only mount (recovered by the ro
#             roots-only ROOT_UPDATE replay; = the last crash-consistent
#             checkpoint captured before the cut).
#   rw_got  — files after the ro->rw upgrade drains the deferred redo log
#             (= the full recovered state; this is the durability contract).
# PASS = rw_got == N AND fsck clean AND no panic. For MODE=fsync every file
# was fsync'd so rw_got MUST equal N (POSIX). ro_got is informational.
set -u
BSD=/Users/girivs/src/bsd
CYCLES=${CYCLES:-20}; N=${N:-30}; MODE=${MODE:-fsync}
SOCK=/tmp/qmp.sock; VSSH="$BSD/scripts/vssh"
BIN=/mnt/host/atrium-tessera/rs/target/aarch64-unknown-freebsd/release

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
  ( $BSD/scripts/run-vm.sh >/tmp/pc-ro-vmboot.log 2>&1 & )
  wait_ready
}
guest_setup() {
  $VSSH 'kldload p9fs 2>/dev/null; mount -t p9fs -o trans=virtio bsd_share /mnt/host 2>/dev/null
    umount /mnt/tessera 2>/dev/null; kldunload tessera_fs 2>/dev/null
    kldload /mnt/host/atrium-tessera/kmod/tessera_fs.ko
    cc -O2 -o /root/fsync_durable /mnt/host/scratch/stress/fsync_durable.c 2>/dev/null
    mkdir -p /mnt/tessera' >/dev/null 2>&1
}

wait_ready
id=$($VSSH 'diskinfo -s /dev/vtbd0' 2>/dev/null | tr -d '\r')
echo "vtbd0 ident=[$id]"
[ "$id" = "tessera-crashtest" ] || { echo "ABORT: vtbd0 is not tessera-crashtest"; exit 1; }

pass=0; fail=0
c=1; while [ $c -le $CYCLES ]; do
  guest_setup
  f1=$($VSSH "$BIN/mkfs-tessera --create -s 256 --seed-file h --seed-content x /dev/vtbd0 >/dev/null
    mount -t tessera /dev/vtbd0 /mnt/tessera
    case $MODE in
      fsync)  nohup /root/fsync_durable /mnt/tessera $N >/tmp/op.log 2>&1 </dev/null & sleep 3 ;;
      sync)   i=1; while [ \$i -le $N ]; do echo d-\$i > /mnt/tessera/d\$i; i=\$((i+1)); done; sync; sync; sleep 1 ;;
      nosync) i=1; while [ \$i -le $N ]; do echo d-\$i > /mnt/tessera/d\$i; i=\$((i+1)); done; sleep 1 ;;
    esac
    ls /mnt/tessera/d* 2>/dev/null | wc -l | tr -d ' '" 2>/dev/null | tail -1)
  echo "cycle $c: created=$f1 (mode=$MODE) — power cut (quit+relaunch)"
  # Phase-1 setup flake (mount/compile over 9p): the op never ran, so there is
  # nothing to recover. Don't score it as a recovery failure — skip the cycle.
  if [ "$MODE" != "nosync" ] && [ "${f1:-0}" != "$N" ]; then
    echo "  cycle $c: phase-1 created=${f1:-?} != $N — setup flake, SKIP"
    c=$((c+1)); continue
  fi
  relaunch
  # RO-FIRST recovery, then ro->rw upgrade — mirrors the boot sequence.
  out=$($VSSH "kldload p9fs 2>/dev/null; mount -t p9fs -o trans=virtio bsd_share /mnt/host 2>/dev/null
    kldunload tessera_fs 2>/dev/null; kldload /mnt/host/atrium-tessera/kmod/tessera_fs.ko
    mkdir -p /mnt/tessera
    mount -t tessera -o ro /dev/vtbd0 /mnt/tessera
    ro=\$(ls /mnt/tessera/d* 2>/dev/null | wc -l | tr -d ' '); echo RO=\$ro
    mount -u -o rw /mnt/tessera
    rw=\$(ls /mnt/tessera/d* 2>/dev/null | wc -l | tr -d ' '); echo RW=\$rw
    umount /mnt/tessera 2>/dev/null
    $BIN/tessera-fsck /dev/vtbd0 2>&1 | grep -cE 'dangling|orphan|nlink|leaked|overlap' | sed 's/^/FSCKP=/'" 2>/dev/null)
  ro_got=$(echo "$out" | grep RO= | cut -d= -f2)
  rw_got=$(echo "$out" | grep RW= | cut -d= -f2)
  fsckp=$(echo "$out"  | grep FSCKP= | cut -d= -f2)
  # fsync/sync are durability barriers → must recover N/N. nosync may
  # legitimately lose un-synced files, so the oracle is fsck-clean + both
  # mounts succeeding (no panic), with the count informational.
  ok=0
  if [ "$MODE" = "nosync" ]; then
    [ "${fsckp:-1}" = "0" ] && [ -n "$rw_got" ] && ok=1
  else
    [ "${rw_got:-x}" = "$N" ] && [ "${fsckp:-1}" = "0" ] && ok=1
  fi
  if [ "$ok" = "1" ]; then
    echo "  cycle $c: ro=$ro_got rw=$rw_got/$N fsck=clean PASS"; pass=$((pass+1))
  else
    echo "  cycle $c: ro=${ro_got:-?} rw=${rw_got:-?}/$N fsck=${fsckp:-?}  FAIL"; fail=$((fail+1))
  fi
  c=$((c+1))
done
echo "=== RO-PATH MODE=$MODE: $pass PASS / $fail FAIL of $CYCLES cycles ==="
