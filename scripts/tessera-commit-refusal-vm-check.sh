#!/bin/sh
# Tessera refuses to commit over writes the device refused (tessera_fs.c
# tessera_commit_sb and the data-zone GC; mail.md §6.4a A5 follow-ups).
#
# A scratch plain Tessera image on an md, under a gnop(8) provider that is
# made to fail every write for a while:
#   1. GC: a forced GC pass while writes fail latches commit_failed (the
#      console says VOLUME CANNOT COMMIT, the counter moves), and a new
#      write is then refused (fsync fails);
#   2. flush: a sync while writes fail is refused ("delayed writes did not
#      land", or "commit_sb failed" when a synchronous write failed first),
#      commits no superblock and latches;
#   3. a sync while writes fail with ENXIO (which the buffer cache drops
#      rather than re-dirties), then another with the provider back:
#      commit_failed latches and no later SB is written over the dropped
#      write, so tessera-fsck finds the volume clean;
#   4. after each, with writes allowed again, the volume unmounts and
#      tessera-fsck finds it clean (the on-disk SB names only durable
#      sectors).
# Positive control: the same GC and sync with writes allowed print neither
# line and leave the counter where it was.
#
# Needs a guest with tessera_fs.ko from this tree loaded and /root/sealed
# (tessera-mkfs-sealed, tessera-gc) from tessera-sealed-vm.sh deploy, and
# tessera-fsck on PATH.
#
#   sh scripts/tessera-commit-refusal-vm-check.sh
set -u
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
. "$ROOT/scripts/lib/gate.sh"
VSSH="${VSSH:-$ROOT/scripts/vssh}"
g() { VSSH_CONNECT_TIMEOUT=30 "$VSSH" "$@"; }
T=/var/tmp/commit-refusal
M=/tcref
IMG=$T/scratch.img
MKFS=/root/sealed/tessera-mkfs-sealed
GC=/root/sealed/tessera-gc
gate_begin tessera-commit-refusal 16

g "test -x $MKFS && test -x $GC && command -v tessera-fsck >/dev/null && kldstat -q -n tessera_fs.ko" \
    || gate_abort "needs tessera_fs loaded, /root/sealed and tessera-fsck (see the header)"
g "kldload -n geom_nop" || gate_abort "geom_nop does not load"

counter() { g "sysctl -n kern.tessera.commit_failed"; }
# Console lines since mark(): the message buffer wraps, so a count over its
# tail stops growing once it is full. mark() saves the last line; since()
# counts matches after its last occurrence (all lines if it scrolled out).
MARK=/var/tmp/commit-refusal.mark
mark() { g "dmesg | tail -1 > $MARK"; }
since() { g "dmesg | awk -v p='$1' 'BEGIN { getline m < \"$MARK\" } \$0 == m { c = 0; next } \$0 ~ p { c++ } END { print c + 0 }'"; }
teardown() {
    g "umount -f $M >/dev/null 2>&1; for n in \$(ls /dev | grep '\\.nop\$'); do gnop destroy -f \$n >/dev/null 2>&1; done; \
       for u in \$(mdconfig -l); do mdconfig -lv -u \$u | grep -q $IMG && mdconfig -d -u \$u; done; true"
}
fresh() {
    teardown
    g "rm -rf $T && mkdir -p $T $M && $MKFS --plain --create -s 256 -j 64 $IMG >/dev/null" || gate_abort "mkfs failed"
    MD=$(g "mdconfig -a -t vnode -f $IMG")
    g "gnop create /dev/$MD && mount -t tessera /dev/$MD.nop $M" || gate_abort "mount on $MD.nop failed"
}
writes() { g "gnop configure -w $1 -e ${2:-5} /dev/$MD.nop"; }
churn() { # a file written, synced and removed, so GC has packs to reclaim
    g "dd if=/dev/random of=$M/churn bs=1m count=16 status=none && sync && rm $M/churn && sync && sleep 2 && sync"
}
clean_after() { # label
    writes 0
    g "umount $M" && ok "$1: unmounts with writes allowed again" || bad "$1: umount failed: $(g "dmesg | tail -5 | tr '\n' ';'")"
    r=$(g "tessera-fsck /dev/$MD 2>&1")
    n=$(printf '%s\n' "$r" | grep -ciE 'dangling|orphan|nlink|leaked|overlap|missing|neither|corrupt|problem|torn|bad root')
    [ "$n" = 0 ] || printf '%s\n' "$r" | sed 's/^/    fsck: /' | head -40
    check "$1: tessera-fsck finds the volume clean" 0 "$n"
}

GRACE=$(g "sysctl -n kern.tessera.gc_grace_seconds")
g "sysctl kern.tessera.gc_grace_seconds=0 >/dev/null"

echo "=== control: GC and sync with writes allowed"
fresh
c0=$(counter); mark
churn
g "$GC $M >/dev/null 2>&1; sync"
check "control: commit_failed does not move" "$c0" "$(counter)"
check "control: no cannot-commit line" 0 "$(since 'VOLUME CANNOT COMMIT')"
check "control: no refused-commit line" 0 "$(since 'delayed writes did not land')"
clean_after control

echo "=== 1. GC while every write fails"
g "mount -t tessera /dev/$MD.nop $M" || gate_abort "remount failed"
c0=$(counter); mark
churn
writes 100
g "$GC $M >/dev/null 2>&1; true"
[ "$(counter)" -gt "$c0" ] && ok "GC: commit_failed latched ($c0 -> $(counter))" || bad "GC: commit_failed stayed $c0"
[ "$(since 'VOLUME CANNOT COMMIT')" -gt 0 ] && ok "GC: the console says VOLUME CANNOT COMMIT" || bad "GC: no cannot-commit line: $(g "dmesg | tail -8 | tr '\n' ';'")"
writes 0
g "dd if=/dev/zero of=$M/after bs=1m count=4 conv=fsync status=none" 2>/dev/null \
    && bad "GC: a new write was accepted after the latch" || ok "GC: a new write is refused after the latch"
clean_after GC

echo "=== 2. flush (sync) while every write fails"
fresh
# The refusal shows as "delayed writes did not land" when the data sat in
# delayed-write buffers, or as "commit_sb failed" when a synchronous publish
# or the journal write itself failed first; either way no SB is committed.
mark; c0=$(counter)
g "dd if=/dev/random of=$M/f bs=1m count=8 status=none"
writes 100
g "sync; sleep 2; sync; true"
[ "$(since 'delayed writes did not land|flush — commit_sb failed')" -gt 0 ] && [ "$(counter)" -gt "$c0" ] && ok "flush: the superblock commit is refused and latches" || bad "flush: no refused commit or no latch: $(g "dmesg | tail -8 | tr '\n' ';'")"
clean_after flush

echo "=== 3. flush while writes fail ENXIO (dropped, not re-dirtied), then a sync with the provider back"
fresh
c0=$(counter)
g "dd if=/dev/random of=$M/e bs=1m count=8 status=none"
writes 100 6
g "sync; sleep 2; sync; true"
writes 0
g "dd if=/dev/random of=$M/e2 bs=1m count=1 status=none 2>/dev/null; sync; sleep 2; sync; true"
[ "$(counter)" -gt "$c0" ] && ok "ENXIO: commit_failed latched ($c0 -> $(counter))" || bad "ENXIO: commit_failed stayed $c0"
g "umount -f $M" && ok "ENXIO: unmounts (forced)" || bad "ENXIO: umount -f failed"
r=$(g "tessera-fsck /dev/$MD 2>&1")
n=$(printf '%s\n' "$r" | grep -ciE 'dangling|orphan|nlink|leaked|overlap|missing|neither|corrupt|problem|torn|bad root|lost')
[ "$n" = 0 ] || printf '%s\n' "$r" | sed 's/^/    fsck: /' | head -40
check "ENXIO: tessera-fsck finds the volume clean (no SB over a dropped write)" 0 "$n"

teardown
g "rm -rf $T $MARK; sysctl kern.tessera.gc_grace_seconds=$GRACE >/dev/null"
gate_end
