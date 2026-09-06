#!/bin/sh
# Residual GC race harness — rm -rf of a big subtree racing continuous
# on-demand GC passes (tq loop) on the SCRATCH disk. GEOM-ident gated.
#
# CHARACTERISED 2026-09-06 (kmod bed4eaa8, after the VMIO stale-page fix):
#   - reproduces with the OLD pass 2 and OLD walk (prefix=0 trust=0) at 2
#     scans, so it predates #133 / the leaf skip / gc_now rewrite;
#   - rm gets TRANSIENT EIO on paths inside the tree (then ENOTEMPTY and
#     ENOTDIR chains as rmdir fails); every leftover path is READABLE once
#     the passes stop (UNREADABLE_leftovers=0 in 4/4 arms); survivors are
#     byte-identical to the source;
#   - fsck's single "problem" is one ORPHAN INODE (an unlink interrupted by
#     the EIO); tessera-fsck --repair relinks it.
# So: not data loss. Shape: a read racing a reclaim returns EIO instead of
# retrying (pack_reloc_gen / cas_invalidate_pack family). Open.
S(){ sysctl -n kern.tessera.$1 2>/dev/null || echo 0; }
DEV=/dev/vtbd2; M=/mnt/scratch
diskinfo -v $DEV | grep -q "atrium-scratch" || { echo "REFUSING"; exit 2; }
[ -f /root/src.sha ] || ( cd /usr/src/sys && find . -type f -exec sha256 -r {} + | sort -k2 > /root/src.sha )
arm(){ # label prefix trust rep
  mount | grep -q " $M " && umount $M
  mkfs-tessera $DEV >/dev/null 2>&1 && mkdir -p $M && mount -t tessera $DEV $M || { echo "ARM $1: mkfs/mount failed"; return; }
  sysctl kern.tessera.gc_pack_prefix_read=$2 >/dev/null; sysctl kern.tessera.gc_walk_trust_leaf=$3 >/dev/null
  cp -R /usr/src/sys $M/sys 2>/dev/null; sync; sleep 2
  s0=$(S gc_scans); r0=$(S gc_reclaimed); m0=$(S gc_pack_id_mismatch); a0=$(S gc_aborts)
  touch /root/gcloop; ( while [ -f /root/gcloop ]; do /root/tq $M >/dev/null 2>&1; done ) & sleep 0.5
  : > /root/rmr.err
  rm -rf $M/sys/dev 2>>/root/rmr.err; rm -rf $M/sys/contrib 2>>/root/rmr.err; rm -rf $M/sys/arm64 2>>/root/rmr.err; sync
  rm -f /root/gcloop; wait; sleep 1
  left=$(find $M/sys/dev $M/sys/contrib $M/sys/arm64 2>/dev/null | wc -l | tr -d ' ')
  unread=0; for f in $(find $M/sys/dev $M/sys/contrib $M/sys/arm64 -type f 2>/dev/null | head -400); do cat "$f" >/dev/null 2>&1 || unread=$((unread+1)); done
  ( cd $M/sys && find . -type f -exec sha256 -r {} + 2>/root/rd.err | sort -k2 > /root/vol.sha ); rderr=$(wc -l < /root/rd.err | tr -d ' ')
  sort -k2 /root/vol.sha > /root/v; sort -k2 /root/src.sha > /root/s; mism=$(join -j2 /root/v /root/s 2>/dev/null | awk '$2!=$3' | wc -l | tr -d ' ')
  umount $M; tessera-fsck $DEV > /root/fsck.$1.$4.out 2>&1; fs=$(grep -ciE "dangling|problem|error|corrupt" /root/fsck.$1.$4.out)
  echo "ARM $1 prefix=$2 trust=$3 rep=$4: scans=$(( $(S gc_scans)-s0 )) reclaimed=$(( $(S gc_reclaimed)-r0 )) aborts=$(( $(S gc_aborts)-a0 )) id_mismatch=$(( $(S gc_pack_id_mismatch)-m0 )) | rm_errs=$(wc -l < /root/rmr.err | tr -d ' ') left_after_rm=$left UNREADABLE_leftovers=$unread survivor_read_errs=$rderr survivor_sha_mismatch=$mism fsck_lines=$fs"
  [ -s /root/rmr.err ] && { echo "   rm errors by kind:"; sed -E 's#/mnt/scratch/sys/[^:]*#PATH#' /root/rmr.err | sort | uniq -c | sort -rn | head -4 | sed 's/^/     /'; }
  [ "$fs" != 0 ] && { echo "   fsck:"; grep -iE "dangling|problem|error|corrupt" /root/fsck.$1.$4.out | head -4 | sed 's/^/     /'; }
}
echo "=== rm-race $(date) kmod=$(sha256 -q /boot/kernel/tessera_fs.ko | cut -c1-16) ==="
arm old 0 0 1
arm old 0 0 2
arm new 1 1 1
arm new 1 1 2
sysctl kern.tessera.gc_pack_prefix_read=1 >/dev/null; sysctl kern.tessera.gc_walk_trust_leaf=1 >/dev/null
echo "=== DONE ==="
