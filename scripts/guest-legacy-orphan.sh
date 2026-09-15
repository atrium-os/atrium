#!/bin/sh
# Guest half of scripts/vm-legacy-orphan-crash-test.sh — runs ON the dev VM.
#
#   guest-legacy-orphan.sh arm      old-kmod-shaped orphans (unlinked while
#                                   open, UNLINKED flag stripped) plus live
#                                   legacy nlink=0 records; left for the cut
#   guest-legacy-orphan.sh verify   after the reboot: mount (sweep), check,
#                                   remount (sweep must not rerun), fsck
set -u
DISK=vtbd2; DEV=/dev/vtbd2; PART=/dev/vtbd2p1; M=/mnt/lorph
N=${N:-40}
S() { sysctl -n kern.tessera.$1 2>/dev/null || echo 0; }

arm() {
    diskinfo -s $DEV | grep -q '^atrium-scratch$' || { echo REFUSING_ident; exit 2; }
    sysctl -n kern.tessera.fault_legacy_nlink0 >/dev/null 2>&1 || { echo NO_HOOK; exit 4; }
    mount | grep -q " $M " && timeout 120 umount -f $M
    gpart destroy -F $DISK >/dev/null 2>&1
    gpart create -s gpt $DISK >/dev/null && gpart add -t freebsd-ufs -s 256M -i 1 $DISK >/dev/null || { echo PART_FAIL; exit 3; }
    mkdir -p $M
    mkfs-tessera $PART >/dev/null 2>&1 && mount -t tessera $PART $M || { echo MKFS_FAIL; exit 3; }

    mkdir -p $M/keep $M/gone $M/gonedir $M/keep/legacydir/sub
    i=0; while [ $i -lt 10 ]; do dd if=/dev/random of=$M/keep/k$i bs=16384 count=3 2>/dev/null; i=$((i+1)); done
    echo legacy-file > $M/keep/legacyfile
    echo inside-legacy-dir > $M/keep/legacydir/sub/f
    i=0; while [ $i -lt $N ]; do dd if=/dev/random of=$M/gone/v$i bs=8192 count=2 2>/dev/null; i=$((i+1)); done
    i=0; while [ $i -lt 5 ]; do mkdir $M/gonedir/d$i; i=$((i+1)); done
    sync; sleep 7
    (cd $M/keep && sha256 -q k* legacyfile legacydir/sub/f) > /root/lorph.keep.sha

    vino=$(stat -f %i $M/gone/v* $M/gonedir/d* | tr '\n' ' ')
    linos="$(stat -f %i $M/keep/legacyfile) $(stat -f %i $M/keep/legacydir)"
    echo "$linos" > /root/lorph.legacy.inos
    i=0; while [ $i -lt $N ]; do (exec 3<$M/gone/v$i; sleep 100000) >/dev/null 2>&1 & i=$((i+1)); done
    i=0; while [ $i -lt 5 ]; do (cd $M/gonedir/d$i && exec sleep 100000) >/dev/null 2>&1 & i=$((i+1)); done
    sleep 2
    rm -f $M/gone/v*
    rmdir $M/gonedir/d*
    # Strip the flag the way an old kmod would have written these records,
    # and make two LIVE records nlink=0-as-unset.
    hooked=0
    for n in $vino $linos; do sysctl kern.tessera.fault_legacy_nlink0=$n >/dev/null 2>&1 && hooked=$((hooked+1)); done
    sync; sleep 7; sync; sleep 7
    held=$(fstat -f $M 2>/dev/null | grep -c sleep)
    echo "armed: held_open=$held victims=$((N+5)) hooked=$hooked legacy_live=$linos"
}

verify() {
    diskinfo -s $DEV | grep -q '^atrium-scratch$' || { echo REFUSING_ident; exit 2; }
    [ -e $PART ] || { echo "verify: no $PART"; exit 3; }
    sysctl kern.tessera.legacy_orphan_sweep=2 >/dev/null
    f0=$(S legacy_orphans_freed); k0=$(S legacy_nlink0_kept); r0=$(S unlinked_reaped); a0=$(S legacy_sweep_aborted)
    mkdir -p $M
    dmesg -c >/dev/null 2>&1
    mount -t tessera $PART $M || { echo "mount_ok=0"; return; }
    freed=$(( $(S legacy_orphans_freed) - f0 )); kept=$(( $(S legacy_nlink0_kept) - k0 ))
    reaped=$(( $(S unlinked_reaped) - r0 )); aborted=$(( $(S legacy_sweep_aborted) - a0 ))
    keep_ok=0; [ "$( (cd $M/keep && sha256 -q k* legacyfile legacydir/sub/f) 2>/dev/null)" = "$(cat /root/lorph.keep.sha)" ] && keep_ok=1
    left=$(( $(ls $M/gone | wc -l) + $(ls $M/gonedir | wc -l) ))
    echo "mount_ok=1 legacy_freed=$freed legacy_kept=$kept flagged_reaped=$reaped aborted=$aborted keep_ok=$keep_ok gone_left=$left"
    dmesg | grep -E "legacy orphan sweep" | tail -2 | cut -c1-220 | sed 's/^/  dmesg: /'
    # Commit, then prove the feature bit persisted: a remount must not sweep.
    sync; sleep 7
    cd /
    timeout 180 umount $M || { echo "fsck_problems=SKIPPED_MOUNTED"; return; }
    dmesg -c >/dev/null 2>&1
    mount -t tessera $PART $M && { resweep=$(dmesg | grep -c "legacy orphan sweep"); cd /; timeout 180 umount $M; }
    echo "resweep=${resweep:-X}"
    tessera-fsck $PART > /tmp/lorph.fsck 2>&1
    # The two live legacy records legitimately draw nlink complaints.
    pat=$(tr ' ' '\n' < /root/lorph.legacy.inos | grep . | sed 's/.*/inode &[:( ]/' | paste -sd'|' -)
    probs=$(grep -iE 'dangling|orphan|nlink|leaked|overlap|missing|neither|corrupt' /tmp/lorph.fsck | grep -vE "$pat")
    echo "fsck_problems=$(printf '%s' "$probs" | grep -c .) legacy_lines=$(grep -cE "$pat" /tmp/lorph.fsck)"
    printf '%s\n' "$probs" | grep . | sed -E 's/[0-9]{3,}/N/g' | sort | uniq -c | sort -rn | head -4 | sed 's/^/  fsck: /'
    gpart destroy -F $DISK >/dev/null 2>&1
    rm -f /root/lorph.keep.sha /root/lorph.legacy.inos
}

case "${1:-}" in
arm)    arm ;;
verify) verify ;;
*)      echo "usage: $0 arm|verify"; exit 2 ;;
esac
