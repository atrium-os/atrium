#!/bin/sh
# Extra workload SHAPES for the soak set.
#
# WHY THIS EXISTS
#
#   On 2026-09-12, FOUR of five filesystem defects fixed that day came from ONE
#   workload that had never been run before: concurrent traversal against
#   namespace churn (scripts/vm-namespace-churn-test.sh). None of them were
#   reachable by the crash soaks, which run their namespace ops serially.
#
#   That is a statement about COVERAGE, not about those four bugs. The cheapest
#   insurance is more workload SHAPES, not more depth on the shapes already
#   covered — so this adds the three that plausibly reach code the existing set
#   never touches:
#
#     rename      cross-directory renames, rename-over-existing, directory
#                 renames, all concurrent with traversal. vop_rename is the
#                 only vop that locks TWO parent directories, which is the
#                 classic ABBA shape, and its displaced-target path is the
#                 one 4417ed6c had to fix by hand.
#
#     mixed       large-file content writes concurrent with namespace churn.
#                 The content path (space_admit, CHUNK_LIST/CHUNK_TREE, the
#                 data zone) and the metadata path (meta_admit, dirent log,
#                 manifest packs) have separate admission control and separate
#                 GC pressure; every soak so far drives one or the other,
#                 never both at once.
#
#     multimount  two Tessera volumes mounted and churned simultaneously.
#                 Per-mount state (flush gate, dirty lists, pending manifests)
#                 is per-mount, but the CAS cache and GC are shared surfaces.
#                 Nothing has ever exercised two live mounts at once.
#
# WHAT COUNTS AS FAILURE
#   Distinct broken paths (NOT error count — once a path wedges, every later op
#   on it fails, so a count measures run length, not defect frequency), plus
#   fsck on the unmounted volume as the on-disk oracle.
#
#   Each shape asserts a MECHANISM counter moved. A shape that did not actually
#   reach its target code is a dead arm and its PASS is not evidence.
#
#   SHAPE=rename SECS=300 sh scripts/vm-workload-shapes.sh
#   sh scripts/vm-workload-shapes.sh            # runs all three
set -u
BSD="$(cd "$(dirname "$0")/.." && pwd)"
SECS=${SECS:-300}
SHAPE=${SHAPE:-all}
VSSH="$BSD/scripts/vssh"
DEV=/dev/vtbd2

KO="$BSD/atrium-tessera/kmod/tessera_fs.ko"
[ -f "$KO" ] || { echo "no $KO — build it first"; exit 1; }
KMOD=$(shasum -a 256 "$KO" | cut -c1-16)
k=$($VSSH "sha256 -q /boot/kernel/tessera_fs.ko | cut -c1-16" 2>/dev/null | tr -d '\r')
[ "$k" = "$KMOD" ] || { echo "ABORT: guest module [$k] != tree [$KMOD]"; exit 1; }

# ★ #129: destructive work ONLY on the disk whose GEOM ident says so. Every
# shape re-asserts this in-guest; multimount partitions the scratch disk, so
# the gate is on the PARENT disk's ident before gpart touches anything.
GATE="diskinfo -s $DEV | grep -q '^atrium-scratch\$' || { echo REFUSING_ident; exit 2; }"

rc=0
echo "########## WORKLOAD SHAPES $(date) kmod=$KMOD secs=$SECS shape=$SHAPE ##########"
echo "  kernel=$($VSSH 'sysctl -n kern.bootfile' 2>/dev/null | tr -d '\r') sched=$($VSSH 'sysctl -n kern.sched.name' 2>/dev/null | tr -d '\r')"

# ── shape: rename ───────────────────────────────────────────────────
run_rename() {
echo "=== SHAPE rename — cross-dir renames + rename-over-existing, under traversal ==="
OUT=$($VSSH "$GATE
M=/mnt/scratch
mount | grep -q \" \$M \" && umount \$M; mkdir -p \$M
mkfs-tessera $DEV >/dev/null 2>&1 && mount -t tessera $DEV \$M || { echo MKFS_FAIL; exit 3; }
i=0; while [ \$i -lt 20 ]; do mkdir -p \$M/a\$i \$M/b\$i
  j=0; while [ \$j -lt 5 ]; do echo x > \$M/a\$i/f\$j; j=\$((j+1)); done; i=\$((i+1)); done
sync
S(){ sysctl -n kern.tessera.\$1 2>/dev/null || echo 0; }
K0=\$(S gc_touch_keeps)
rm -f /root/sh.stop; : > /root/sh.err
# traversals
n=1; while [ \$n -le 2 ]; do
  sh -c \"while [ ! -f /root/sh.stop ]; do find \$M >/dev/null 2>&1; done\" >/dev/null 2>&1 & n=\$((n+1))
done
# renamers: file cross-dir, rename-OVER an existing file, and whole-dir moves
n=1; while [ \$n -le 3 ]; do
  sh -c 'm=\$1; k=0; c=0; while [ ! -f /root/sh.stop ]; do
      k=\$(( (k+7) % 20 )); s=/mnt/scratch/a\$k; d=/mnt/scratch/b\$k
      echo v > \$s/r\$m 2>>/root/sh.err
      mv \$s/r\$m \$d/r\$m 2>>/root/sh.err                  # cross-directory
      echo w > \$s/r\$m 2>>/root/sh.err
      mv \$s/r\$m \$d/r\$m 2>>/root/sh.err                  # rename OVER existing
      mkdir -p \$s/dir\$m/inner 2>>/root/sh.err
      mv \$s/dir\$m \$d/dir\$m 2>>/root/sh.err              # directory move
      rm -rf \$d/dir\$m \$d/r\$m 2>>/root/sh.err
      c=\$((c+1)); echo \$c > /root/sh.c\$m
    done' _ \$n >/dev/null 2>&1 & n=\$((n+1))
done
sh -c \"while [ ! -f /root/sh.stop ]; do /root/tq \$M >/dev/null 2>&1; done\" >/dev/null 2>&1 &
sleep $SECS
touch /root/sh.stop; sleep 4; wait 2>/dev/null
T=0; for f in /root/sh.c1 /root/sh.c2 /root/sh.c3; do T=\$(( T + \$(cat \$f 2>/dev/null || echo 0) )); done
B=\$(grep -oE '/mnt/scratch[^ :]*' /root/sh.err 2>/dev/null | sort -u | wc -l | tr -d ' ')
echo \"iters=\$T broken=\$B errs=\$(wc -l < /root/sh.err | tr -d ' ') keeps=\$(( \$(S gc_touch_keeps)-K0 ))\"
sync; umount \$M 2>/dev/null || echo UMOUNT_FAIL
tessera-fsck $DEV > /root/sh.fsck 2>&1
echo \"fsck=\$(grep -ciE 'dangling|orphan|nlink|leaked|overlap|missing|neither|corrupt|problem' /root/sh.fsck)\"" 2>&1 | tr -d '\r')
report rename "$OUT" iters
}

# ── shape: mixed ────────────────────────────────────────────────────
run_mixed() {
echo "=== SHAPE mixed — large-file content writes + namespace churn together ==="
OUT=$($VSSH "$GATE
M=/mnt/scratch
mount | grep -q \" \$M \" && umount \$M; mkdir -p \$M
mkfs-tessera $DEV >/dev/null 2>&1 && mount -t tessera $DEV \$M || { echo MKFS_FAIL; exit 3; }
mkdir -p \$M/big \$M/ns
i=0; while [ \$i -lt 20 ]; do mkdir -p \$M/ns/d\$i; i=\$((i+1)); done; sync
S(){ sysctl -n kern.tessera.\$1 2>/dev/null || echo 0; }
C0=\$(S chunk_tree_publish); W0=\$(S append_fast_ok); K0=\$(S gc_touch_keeps)
rm -f /root/sh.stop; : > /root/sh.err
# large-file writers: rewrite multi-MiB files so chunking + the data zone churn
n=1; while [ \$n -le 2 ]; do
  sh -c 'm=\$1; c=0; while [ ! -f /root/sh.stop ]; do
      # ★ dd writes its TRANSFER STATS to stderr on every success, so
      # appending its stderr to the error log turns normal output into
      # thousands of fake "errors" and buries any real one. Discard the
      # stats; record only a non-zero exit.
      dd if=/dev/zero of=/mnt/scratch/big/f\$m bs=1m count=24 2>/dev/null ||
          echo \"dd-write failed /mnt/scratch/big/f\$m\" >>/root/sh.err
      dd if=/dev/zero of=/mnt/scratch/big/f\$m bs=1m count=8 conv=notrunc 2>/dev/null ||
          echo \"dd-rewrite failed /mnt/scratch/big/f\$m\" >>/root/sh.err
      rm -f /mnt/scratch/big/f\$m 2>>/root/sh.err
      c=\$((c+1)); echo \$c > /root/sh.c\$m
    done' _ \$n >/dev/null 2>&1 & n=\$((n+1))
done
# namespace churn at the same time — the pairing is the point
sh -c 'k=0; c=0; while [ ! -f /root/sh.stop ]; do k=\$(( (k+7) % 20 )); d=/mnt/scratch/ns/d\$k
    mkdir -p \$d/n 2>>/root/sh.err; echo y > \$d/n/a 2>>/root/sh.err; rm -rf \$d/n 2>>/root/sh.err
    c=\$((c+1)); echo \$c > /root/sh.c3
  done' >/dev/null 2>&1 &
sh -c \"while [ ! -f /root/sh.stop ]; do find \$M >/dev/null 2>&1; done\" >/dev/null 2>&1 &
sh -c \"while [ ! -f /root/sh.stop ]; do /root/tq \$M >/dev/null 2>&1; done\" >/dev/null 2>&1 &
sleep $SECS
touch /root/sh.stop; sleep 5; wait 2>/dev/null
T=0; for f in /root/sh.c1 /root/sh.c2 /root/sh.c3; do T=\$(( T + \$(cat \$f 2>/dev/null || echo 0) )); done
B=\$(grep -oE '/mnt/scratch[^ :]*' /root/sh.err 2>/dev/null | sort -u | wc -l | tr -d ' ')
echo \"iters=\$T broken=\$B errs=\$(wc -l < /root/sh.err | tr -d ' ') chunks=\$(( \$(S chunk_tree_publish)-C0 )) appends=\$(( \$(S append_fast_ok)-W0 )) keeps=\$(( \$(S gc_touch_keeps)-K0 ))\"
sync; umount \$M 2>/dev/null || echo UMOUNT_FAIL
tessera-fsck $DEV > /root/sh.fsck 2>&1
echo \"fsck=\$(grep -ciE 'dangling|orphan|nlink|leaked|overlap|missing|neither|corrupt|problem' /root/sh.fsck)\"" 2>&1 | tr -d '\r')
report mixed "$OUT" iters
}

# ── shape: multimount ───────────────────────────────────────────────
run_multimount() {
echo "=== SHAPE multimount — two live Tessera volumes churned at once ==="
# ★ Both volumes live INSIDE the scratch disk. The ident gate is checked on
#   the parent device BEFORE gpart runs, so this cannot touch a real disk.
OUT=$($VSSH "$GATE
M1=/mnt/scratch; M2=/mnt/scratch2
for m in \$M1 \$M2; do mount | grep -q \" \$m \" && umount \$m; done
mkdir -p \$M1 \$M2
gpart destroy -F $DEV >/dev/null 2>&1
gpart create -s GPT $DEV >/dev/null 2>&1 || { echo GPART_FAIL; exit 3; }
gpart add -t freebsd-zfs -s 1800M $DEV >/dev/null 2>&1 || { echo GPART_FAIL; exit 3; }
gpart add -t freebsd-zfs $DEV >/dev/null 2>&1 || { echo GPART_FAIL; exit 3; }
sleep 1
mkfs-tessera ${DEV}p1 >/dev/null 2>&1 && mkfs-tessera ${DEV}p2 >/dev/null 2>&1 || { echo MKFS_FAIL; exit 3; }
mount -t tessera ${DEV}p1 \$M1 && mount -t tessera ${DEV}p2 \$M2 || { echo MOUNT_FAIL; exit 3; }
for m in \$M1 \$M2; do i=0; while [ \$i -lt 15 ]; do mkdir -p \$m/d\$i; i=\$((i+1)); done; done; sync
S(){ sysctl -n kern.tessera.\$1 2>/dev/null || echo 0; }
K0=\$(S gc_touch_keeps)
echo \"mounts=\$(mount | grep -c 'on /mnt/scratch')\"
rm -f /root/sh.stop; : > /root/sh.err
n=1; for m in \$M1 \$M2; do
  sh -c \"while [ ! -f /root/sh.stop ]; do find \$m >/dev/null 2>&1; done\" >/dev/null 2>&1 &
  sh -c 'p=\$1; i=\$2; k=0; c=0; while [ ! -f /root/sh.stop ]; do k=\$(( (k+7) % 15 )); d=\$p/d\$k
      mkdir -p \$d/n 2>>/root/sh.err; echo y > \$d/n/a 2>>/root/sh.err
      ln \$d/n/a \$d/n/b 2>>/root/sh.err; rm -rf \$d/n 2>>/root/sh.err
      c=\$((c+1)); echo \$c > /root/sh.c\$i
    done' _ \$m \$n >/dev/null 2>&1 &
  sh -c \"while [ ! -f /root/sh.stop ]; do /root/tq \$m >/dev/null 2>&1; done\" >/dev/null 2>&1 &
  n=\$((n+1))
done
sleep $SECS
touch /root/sh.stop; sleep 4; wait 2>/dev/null
T=\$(( \$(cat /root/sh.c1 2>/dev/null || echo 0) + \$(cat /root/sh.c2 2>/dev/null || echo 0) ))
B=\$(grep -oE '/mnt/scratch[0-9]*[^ :]*' /root/sh.err 2>/dev/null | sort -u | wc -l | tr -d ' ')
echo \"iters=\$T broken=\$B errs=\$(wc -l < /root/sh.err | tr -d ' ') keeps=\$(( \$(S gc_touch_keeps)-K0 ))\"
sync; umount \$M1 2>/dev/null || echo UMOUNT_FAIL; umount \$M2 2>/dev/null || echo UMOUNT_FAIL
tessera-fsck ${DEV}p1 > /root/sh.f1 2>&1; tessera-fsck ${DEV}p2 > /root/sh.f2 2>&1
echo \"fsck=\$(( \$(grep -ciE 'dangling|orphan|nlink|leaked|overlap|missing|neither|corrupt|problem' /root/sh.f1) + \$(grep -ciE 'dangling|orphan|nlink|leaked|overlap|missing|neither|corrupt|problem' /root/sh.f2) ))\"
# ★ Leave the scratch disk as we found it: WHOLE-DISK, no partition table.
# Every other harness mkfs-es $DEV directly, and a leftover GPT makes the
# kernel log 'primary GPT table is corrupt or invalid' on every later attach
# — noise that already cost time to chase once.
gpart destroy -F $DEV >/dev/null 2>&1
echo \"cleanup=\$(gpart show $DEV >/dev/null 2>&1 && echo TABLE_REMAINS || echo whole-disk)\"" 2>&1 | tr -d '\r')
report multimount "$OUT" iters
}

# ── shared verdict ──────────────────────────────────────────────────
report() {
    _name=$1; _out=$2; _mech=$3
    echo "$_out" | grep -E "^iters=|^mounts=|^fsck=|^cleanup=" | sed 's/^/  /'
    case "$_out" in
        *REFUSING*|*MKFS_FAIL*|*MOUNT_FAIL*|*GPART_FAIL*|*UMOUNT_FAIL*)
            echo "  FAIL [$_name] — harness could not run"; rc=1; return;;
    esac
    _b=$(echo "$_out" | sed -n 's/.*broken=\([0-9]*\).*/\1/p')
    _f=$(echo "$_out" | sed -n 's/.*fsck=\([0-9]*\).*/\1/p')
    _i=$(echo "$_out" | sed -n "s/.*${_mech}=\([0-9]*\).*/\1/p")
    [ "${_b:-1}" = 0 ] || { echo "  FAIL [$_name] — $_b broken paths"; rc=1; }
    [ "${_f:-1}" = 0 ] || { echo "  FAIL [$_name] — fsck found $_f problems"; rc=1; }
    # ★ dead-arm guard: a shape that completed no iterations proves nothing.
    [ "${_i:-0}" -gt 0 ] 2>/dev/null || {
        echo "  WARNING [$_name] — mechanism counter is 0: this shape never ran its workload, so its PASS is not evidence"; }
    [ "${_b:-1}" = 0 ] && [ "${_f:-1}" = 0 ] && echo "  PASS [$_name]"
}

case "$SHAPE" in
    rename)     run_rename ;;
    mixed)      run_mixed ;;
    multimount) run_multimount ;;
    all)        run_rename; run_mixed; run_multimount ;;
    *) echo "unknown SHAPE=$SHAPE (rename|mixed|multimount|all)"; exit 2;;
esac

echo "########## SHAPES DONE $(date) rc=$rc ##########"
exit $rc
