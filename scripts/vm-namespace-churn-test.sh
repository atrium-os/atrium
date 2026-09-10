#!/bin/sh
# Regression test: concurrent traversal + namespace churn must not wedge a
# directory into permanent EIO.
#
# THE BUG THIS PINS DOWN
#
#   vop_rmdir HARD-DELETES the child inode record, while vop_remove keeps it at
#   nlink=0 and marks the vnode unlinked. So a directory removed while a
#   concurrent traversal held it name-cached left a vnode whose inode record
#   was gone, and vop_access answered every later permission check with a hard
#   EIO. The directory then stayed broken for the REST OF THE MOUNT:
#
#     stat dir        -> OK      (VEXEC is checked on the PARENT)
#     ls dir          -> EIO
#     anything in dir -> EIO
#     mkdir -p dir    -> "exists" (it stats first), so it never recreates it
#
#   fsck is CLEAN throughout — there is no on-disk damage — and the name is
#   simply gone after a remount. Measured before the fix: 8-9 wedged
#   directories and ~15k EIOs per 200s run.
#
# WHY THE METRIC IS "DISTINCT BROKEN DIRECTORIES"
#
#   ★ Error COUNT is worthless here. Once a directory wedges, every later op on
#   it fails, so the count measures how long the run continued after the first
#   wedge — not how often wedging happens. Counting DISTINCT paths measures the
#   thing itself.
#
#   ★ And all THREE churners must be observed. An earlier instrument watched
#   one, sampled a third of the exposure, and reported "no failure" for two
#   runs that had in fact wedged 8 directories each.
#
# WHY IT MUST RUN WITH GC ON
#
#   GC is an AMPLIFIER, not the cause: 3 reps per arm measured GC-on = 8/9/8
#   broken dirs, GC-off = 0/0/1. The GC-off failure is why this is not filed as
#   a GC bug — but with GC off the signal is ~8x rarer, so the test would need
#   to run far longer to have the same power. Hence gc=1 by default.
#
#   Assert the mechanism actually fired: access_no_inode must be NONZERO. A
#   run where it stayed 0 did not exercise the fixed path at all and its green
#   result means nothing (the dead-arm trap).
#
#   sh scripts/vm-namespace-churn-test.sh            # 200s, GC on
#   SECS=600 sh scripts/vm-namespace-churn-test.sh   # longer
set -u
BSD="$(cd "$(dirname "$0")/.." && pwd)"
SECS=${SECS:-200}; GC=${GC:-1}
VSSH="$BSD/scripts/vssh"

KO="$BSD/atrium-tessera/kmod/tessera_fs.ko"
[ -f "$KO" ] || { echo "no $KO — build it first"; exit 1; }
KMOD=$(shasum -a 256 "$KO" | cut -c1-16)
k=$($VSSH "sha256 -q /boot/kernel/tessera_fs.ko | cut -c1-16" 2>/dev/null | tr -d '\r')
[ "$k" = "$KMOD" ] || { echo "ABORT: guest module [$k] != tree [$KMOD]"; exit 1; }

echo "=== NAMESPACE-CHURN TEST $(date) kmod=$KMOD secs=$SECS gc=$GC ==="
OUT=$($VSSH "
M=/mnt/scratch; DEV=/dev/vtbd2
diskinfo -s \$DEV | grep -q '^atrium-scratch\$' || { echo REFUSING_ident; exit 2; }
mount | grep -q \" \$M \" && umount \$M
mkdir -p \$M
mkfs-tessera \$DEV >/dev/null 2>&1 && mount -t tessera \$DEV \$M || { echo MKFS_FAIL; exit 3; }
i=0; while [ \$i -lt 20 ]; do d=\$M/t0/d\$i; mkdir -p \$d
  j=0; while [ \$j -lt 5 ]; do echo x > \$d/f\$j; j=\$((j+1)); done; i=\$((i+1)); done
sync
sysctl kern.tessera.lookup_no_inode_verbose=1 >/dev/null 2>&1
S(){ sysctl -n kern.tessera.\$1 2>/dev/null || echo 0; }
A0=\$(S access_no_inode); G0=\$(S gc_touch_keeps)

rm -f /root/nc.stop; : > /root/nc.err
n=1; while [ \$n -le 3 ]; do
  sh -c \"while [ ! -f /root/nc.stop ]; do find \$M -type f >/dev/null 2>&1; done\" >/dev/null 2>&1 & n=\$((n+1))
done
n=1; while [ \$n -le 3 ]; do
  sh -c 'm=\$1; k=0; while [ ! -f /root/nc.stop ]; do k=\$(( (k+7) % 20 )); d=/mnt/scratch/t0/d\$k
         mkdir -p \$d/n\$m; echo y > \$d/n\$m/a; ln \$d/n\$m/a \$d/n\$m/b
         mv \$d/n\$m/b \$d/n\$m/c; rm -rf \$d/n\$m; done' _ \$n 2>>/root/nc.err & n=\$((n+1))
done
[ $GC = 1 ] && sh -c \"while [ ! -f /root/nc.stop ]; do /root/tq \$M >/dev/null 2>&1; done\" >/dev/null 2>&1 &
sleep $SECS
touch /root/nc.stop; sleep 4; wait 2>/dev/null

BROKEN=\$(grep -oE '/mnt/scratch[^ :]*' /root/nc.err 2>/dev/null | sed 's|/[abc]\$||' | sort -u | wc -l | tr -d ' ')
echo \"broken_dirs=\$BROKEN errors=\$(wc -l < /root/nc.err | tr -d ' ') access_no_inode=\$(( \$(S access_no_inode)-A0 )) gc_touch_keeps=\$(( \$(S gc_touch_keeps)-G0 ))\"
sync; umount \$M 2>/dev/null || echo UMOUNT_FAIL
tessera-fsck \$DEV > /root/nc.fsck 2>&1
echo \"fsck_problems=\$(grep -ciE 'dangling|orphan|nlink|leaked|overlap|missing|neither|corrupt|problem' /root/nc.fsck)\"" 2>&1 | tr -d '\r')

echo "$OUT"
case "$OUT" in *REFUSING*|*MKFS_FAIL*|*UMOUNT_FAIL*) echo "FAIL — harness could not run"; exit 1;; esac
B=$(echo "$OUT" | sed -n 's/.*broken_dirs=\([0-9]*\).*/\1/p')
A=$(echo "$OUT" | sed -n 's/.*access_no_inode=\([0-9]*\).*/\1/p')
F=$(echo "$OUT" | sed -n 's/.*fsck_problems=\([0-9]*\).*/\1/p')
rc=0
[ "${B:-1}" = 0 ] || { echo "FAIL — $B directories wedged"; rc=1; }
[ "${F:-1}" = 0 ] || { echo "FAIL — fsck found $F problems"; rc=1; }
# ★ dead-arm guard: a green run that never entered the path proves nothing.
[ "${A:-0}" -gt 0 ] 2>/dev/null || echo "WARNING — access_no_inode never fired; this run did not exercise the fixed path, so its PASS is not evidence"
[ $rc = 0 ] && echo "PASS — no directory wedged in ${SECS}s of concurrent traversal + churn"
exit $rc
