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
#      sectors). A read-only remount must return the same bytes for a
#      file synced before the fault.
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
MD=
NOP=
MKFS=/root/sealed/tessera-mkfs-sealed
GC=/root/sealed/tessera-gc
gate_begin tessera-commit-refusal 20

g "test -x $MKFS && test -x $GC && command -v tessera-fsck >/dev/null && kldstat -q -n tessera_fs.ko" \
    || gate_abort "needs tessera_fs loaded, /root/sealed and tessera-fsck (see the header)"
g "kldload -n geom_nop" || gate_abort "geom_nop does not load"

counter() { g "sysctl -n kern.tessera.commit_failed"; }
# Console lines since mark(): the message buffer wraps, so a count over its
# tail stops growing once it is full. mark() saves the last line; since()
# counts matches after its last occurrence (all lines if it scrolled out).
T=$(g 'mktemp -d /var/tmp/commit-refusal.XXXXXXXX') || gate_abort "cannot create scratch directory"
M=$T/mnt
IMG=$T/scratch.img
MARK=$T/mark
mark() { g "dmesg | tail -1 > $MARK"; }
since() { g "dmesg | awk -v p='$1' 'BEGIN { getline m < \"$MARK\" } \$0 == m { c = 0; next } \$0 ~ p { c++ } END { print c + 0 }'"; }
teardown() {
    if [ -n "$NOP" ]; then
        g "gnop destroy $NOP" || return 1
        NOP=
    fi
    if [ -n "$MD" ]; then
        g "mdconfig -d -u $MD" || return 1
        MD=
    fi
}
fresh() {
    teardown || gate_abort "cannot release the previous fixture ($T)"
    g "mkdir -p $M && $MKFS --plain --create -s 256 -j 64 $IMG >/dev/null" || gate_abort "mkfs failed ($T)"
    MD=$(g "mdconfig -a -t vnode -f $IMG") || gate_abort "md attach failed ($T)"
    g "gnop create /dev/$MD" || gate_abort "gnop create failed for $MD ($T)"
    NOP=/dev/$MD.nop
    g "mount -t tessera $NOP $M" || gate_abort "mount on $NOP failed ($T)"
    g "dd if=/dev/random of=$T/reference bs=1m count=2 status=none &&
       dd if=$T/reference of=$M/committed bs=1m conv=fsync status=none" || gate_abort "could not sync reference data"
}
writes() { g "gnop configure -w $1 -e ${2:-5} /dev/$MD.nop"; }
churn() { # a file written, synced and removed, so GC has packs to reclaim
    g "dd if=/dev/random of=$M/churn bs=1m count=16 status=none && sync && rm $M/churn && sync && sleep 2 && sync"
}
check_fsck() {
    if r=$(g "tessera-fsck -n /dev/$MD 2>&1") &&
        printf '%s\n' "$r" | grep -qE '^[[:space:]]*result:[[:space:]]+CLEAN — no inconsistencies found$'; then
        ok "$1: tessera-fsck finds the volume clean"
    else
        bad "$1: fsck failed or did not report a clean volume"
        printf '%s\n' "$r" | tail -40
    fi
}
check_image() {
    check_fsck "$1"
    g "mount -t tessera -o ro $NOP $M" || gate_abort "$1: read-only remount failed"
    if g "cmp -s $T/reference $M/committed"; then
        ok "$1: committed file matches after remount"
    else
        bad "$1: committed file differs or cannot be read"
    fi
    g "umount $M" || gate_abort "$1: read-only unmount failed"
}
clean_after() { # label
    writes 0
    g "umount $M" || gate_abort "$1: unmount failed"
    ok "$1: unmounts with writes allowed again"
    check_image "$1"
}

GRACE=$(g "sysctl -n kern.tessera.gc_grace_seconds") || gate_abort "cannot read GC grace"
cleanup() {
    rc=$?
    trap - 0 HUP INT TERM
    if [ -n "$NOP" ]; then
        g "gnop configure -w 0 $NOP" || rc=1
        # This mountpoint is inside the run's private directory.
        g "umount $M" >/dev/null 2>&1 || :
    fi
    if teardown; then
        g "rm -f $IMG $MARK $T/reference && rmdir $M $T" || rc=1
    else
        echo "could not release the fixture at $T" >&2
        rc=1
    fi
    g "sysctl kern.tessera.gc_grace_seconds=$GRACE >/dev/null" || rc=1
    exit "$rc"
}
trap cleanup 0
trap 'exit 1' HUP INT TERM
g "sysctl kern.tessera.gc_grace_seconds=0 >/dev/null" || gate_abort "cannot set GC grace"

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
g "umount -f $M" || gate_abort "ENXIO: unmount failed"
ok "ENXIO: unmounts (forced)"
check_image ENXIO

gate_end
