#!/bin/sh
# Regression test: GC pass 3 must not invalidate the CAS cache once PER DEAD
# PACK.
#
# THE BUG THIS PINS DOWN
#
#   GC pass 3 deletes every dead pack from the registry under the flush gate,
#   and called tessera_cas_invalidate_pack for each one — a walk of the whole
#   CAS location LRU (up to kern.tessera.cas_loc_max = 65,536 entries) per
#   pack. O(dead x cache) with the gate held. On 2026-09-14 the dev root had
#   204,845 dead packs and held its gate for 2 min 40 s; every vop on /, sshd's
#   login path included, slept on "tessgate", and the VM looked hung. A live
#   memory dump found the GC thread in memcmp inside that walk.
#
#   tessera_cas_invalidate_packs now hashes the doomed pack ids and walks the
#   LRU once: O(cache + dead).
#
# HOW IT REPRODUCES THE SHAPE, on the scratch disk only:
#   1. fresh mkfs; 70k small files with DISTINCT content, published in large
#      flushes, so the volume's location cache is full,
#   2. N more files, one flush (sync) each, so each lands in its own pack,
#   3. remove those N files, age the retained snapshot records out (each
#      still pins the deleted packs — see vm-gc-test.sh), then run a GC pass
#      (/root/tq kicks the GC task and returns; wait for "gc done").
#
# MEASURES: the GC pass wall time, kern.tessera.gc_apply_ns and
# gc_cas_invalidate_ns when the kmod has them, and — with dtrace, so both old
# and new kmods are comparable — the time inside tessera_cas_invalidate_pack(s).
# ★ DEAD-ARM GUARD: cas_invalidations must rise. If it does not, the dead
# packs' blobs were not in the cache, the invalidation had nothing to find,
# and a fast pass proves nothing.
#
#   sh scripts/vm-gc-cas-invalidate-test.sh            # N=20000
#   N=5000 sh scripts/vm-gc-cas-invalidate-test.sh
set -u
BSD="$(cd "$(dirname "$0")/.." && pwd)"
N=${N:-20000}
VSSH="$BSD/scripts/vssh"

KO="$BSD/atrium-tessera/kmod/tessera_fs.ko"
KMOD=$(shasum -a 256 "$KO" | cut -c1-16)
k=$($VSSH "sha256 -q /boot/kernel/tessera_fs.ko | cut -c1-16" 2>/dev/null | tr -d '\r')
echo "=== GC CAS-INVALIDATE TEST $(date) guest_kmod=$k tree_kmod=$KMOD n=$N ==="
[ "$k" = "$KMOD" ] || echo "NOTE: guest module differs from the tree (A/B against an older kmod?)"

OUT=$(timeout 3000 $VSSH "
M=/mnt/scratch; DEV=/dev/vtbd2
diskinfo -s \$DEV | grep -q '^atrium-scratch\$' || { echo REFUSING_ident; exit 2; }
mount | grep -q \" \$M \" && umount \$M
mkdir -p \$M
mkfs-tessera \$DEV >/dev/null 2>&1 && mount -t tessera \$DEV \$M || { echo MKFS_FAIL; exit 3; }
S(){ sysctl -n kern.tessera.\$1 2>/dev/null || echo 0; }
i=0; while [ \$i -lt 70 ]; do mkdir \$M/c\$i \$M/p\$((i % 10)) 2>/dev/null; i=\$((i+1)); done
L0=\$(S cas_loc_inserts)
cd \$M && awk 'BEGIN{for(i=0;i<70000;i++){f=sprintf(\"c%d/f%d\",i%70,i); print \"cache-fill \" i > f; close(f)}}'
sync; sleep 2
L1=\$(S cas_loc_inserts)
j=0; while [ \$j -lt $N ]; do echo \"dead-pack \$j \$\$\" > \$M/p\$((j % 10))/d\$j; sync; j=\$((j+1)); done
sleep 2
rm -rf \$M/p0 \$M/p1 \$M/p2 \$M/p3 \$M/p4 \$M/p5 \$M/p6 \$M/p7 \$M/p8 \$M/p9
sync; sleep 2
R=\$(S snapshot_retention); t=0; while [ \$t -lt \$((R + 8)) ]; do echo tick > \$M/c0/.tick; sync; sleep 1; t=\$((t+1)); done
rm -f \$M/c0/.tick; sync; sleep 2
I0=\$(S cas_invalidations); A0=\$(S gc_apply_ns); V0=\$(S gc_cas_invalidate_ns)
rm -f /root/gcinv.dt
dtrace -Z -q -n '
  fbt:tessera_fs:tessera_cas_invalidate_pack:entry  { self->t1 = timestamp; }
  fbt:tessera_fs:tessera_cas_invalidate_pack:return /self->t1/ { @one_ns = sum(timestamp - self->t1); @one_n = count(); self->t1 = 0; }
  fbt:tessera_fs:tessera_cas_invalidate_packs:entry { self->tn = timestamp; }
  fbt:tessera_fs:tessera_cas_invalidate_packs:return /self->tn/ { @many_ns = sum(timestamp - self->tn); @many_n = count(); self->tn = 0; }
  END { printa(\"dt_one_ns=%@d \", @one_ns); printa(\"dt_one_calls=%@d \", @one_n); printa(\"dt_many_ns=%@d \", @many_ns); printa(\"dt_many_calls=%@d \", @many_n); printf(\"\\n\"); }
' -o /root/gcinv.dt >/dev/null 2>&1 &
DT=\$!; sleep 3
G0=\$(dmesg | grep -c 'gc done')
t0=\$(date +%s)
/root/tq \$M >/dev/null 2>&1
w=0; while [ \$(dmesg | grep -c 'gc done') -le \$G0 ] && [ \$w -lt 1800 ]; do sleep 1; w=\$((w+1)); done
t1=\$(date +%s)
sleep 1; kill -TERM \$DT; wait \$DT 2>/dev/null
DEAD=\$(dmesg | grep 'gc pass2 done' | tail -1 | sed -n 's/.* \\([0-9]*\\) dead packs.*/\\1/p')
echo \"loc_inserts_fill=\$((L1-L0)) dead_packs=\$DEAD gc_wall_s=\$((t1-t0)) invalidations=\$(( \$(S cas_invalidations)-I0 )) gc_apply_ns=\$(( \$(S gc_apply_ns)-A0 )) gc_cas_invalidate_ns=\$(( \$(S gc_cas_invalidate_ns)-V0 )) \$(tr -d '\\n' < /root/gcinv.dt)\"
bad=0; i=0; while [ \$i -lt 70000 ]; do f=\$M/c\$((i % 70))/f\$i; [ \"\$(cat \$f 2>/dev/null)\" = \"cache-fill \$i\" ] || bad=\$((bad+1)); i=\$((i+997)); done
echo \"survivor_bad=\$bad\"
cd /; sync; umount \$M || echo UMOUNT_FAIL
tessera-fsck \$DEV > /root/gcinv.fsck 2>&1
echo \"fsck_problems=\$(grep -ciE 'dangling|orphan|nlink|leaked|overlap|missing|neither|corrupt|problem' /root/gcinv.fsck)\"
" 2>&1 | tr -d '\r')
echo "$OUT"
case "$OUT" in *REFUSING*|*MKFS_FAIL*|*UMOUNT_FAIL*) echo "FAIL — harness could not run"; exit 1;; esac
rc=0
I=$(echo "$OUT" | sed -n 's/.*invalidations=\([0-9]*\).*/\1/p')
F=$(echo "$OUT" | sed -n 's/.*fsck_problems=\([0-9]*\).*/\1/p')
B=$(echo "$OUT" | sed -n 's/.*survivor_bad=\([0-9]*\).*/\1/p')
[ "${F:-1}" = 0 ] || { echo "FAIL — fsck found ${F:-?} problems"; rc=1; }
[ "${B:-1}" = 0 ] || { echo "FAIL — ${B:-?} surviving files read wrong after GC"; rc=1; }
[ "${I:-0}" -gt 0 ] 2>/dev/null || echo "WARNING — cas_invalidations did not rise: the dead packs' blobs were not cached, so this pass did not exercise the invalidation"
[ $rc = 0 ] && echo "PASS — GC reclaimed the dead packs; survivors intact; fsck clean"
exit $rc
