#!/bin/sh

# ★ A kill -9'd writer stops matching pgrep BEFORE it releases its vnode
# references, so umount fails fast with EBUSY for a few seconds afterwards.
# Measured: 4 of 8 immediate attempts failed "Device busy", and every one
# succeeded after a 5 s grace. One attempt therefore DISCARDS GOOD RUNS (7 of
# 25 in one campaign). Retry before believing the mount is stuck.
tess_umount() {
    _m=$1; _i=0
    while [ $_i -lt 6 ]; do
        timeout 300 umount "$_m" 2>/dev/null && return 0
        sleep 5; _i=$((_i + 1))
    done
    timeout 300 umount "$_m"   # last attempt, let the error show
}

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
    trap 'sysctl kern.tessera.fault_legacy_sweep_pause=0 kern.tessera.fault_legacy_sweep_nobarrier=0 kern.tessera.legacy_sweep_batch=4096 >/dev/null 2>&1' EXIT
    sysctl kern.tessera.legacy_orphan_sweep=2 kern.tessera.legacy_sweep_batch=7 \
        kern.tessera.fault_legacy_sweep_pause=1 kern.tessera.fault_legacy_sweep_nobarrier=${NOBARRIER:-0} >/dev/null
    f0=$(S legacy_orphans_freed); k0=$(S legacy_nlink0_kept); r0=$(S unlinked_reaped)
    a0=$(S legacy_sweep_aborted); d0=$(S legacy_sweeps_done); n0=$(S legacy_barrier_notes)
    mkdir -p $M
    dmesg -c >/dev/null 2>&1
    mount -t tessera $PART $M || { echo "mount_ok=0"; return; }
    mounted_during_sweep=$([ "$(S legacy_sweeps_done)" = "$d0" ] && echo 1 || echo 0)
    # The sweep has walked / only; everything below it is unread. Move the
    # live legacy records from an unread directory into /, which it has read.
    i=0; while [ $i -lt 600 ] && [ "$(S legacy_sweep_paused)" != 1 ]; do sleep 0.1; i=$((i+1)); done
    paused=$(S legacy_sweep_paused)
    mv $M/keep/legacyfile $M/moved-legacyfile && mv $M/keep/legacydir $M/moved-legacydir
    moved=$?
    # Publish the move (dirent-log checkpoint) so keep/'s manifest no longer
    # names them — otherwise the walk still finds them at the old location
    # and the race is not exercised.
    sync; sleep 7; sync
    sp=$(S legacy_sweep_paused)
    sysctl kern.tessera.fault_legacy_sweep_pause=0 >/dev/null
    i=0; while [ $i -lt 1200 ] && [ "$(S legacy_sweeps_done)" = "$d0" ]; do sleep 0.1; i=$((i+1)); done
    done_ok=$([ "$(S legacy_sweeps_done)" != "$d0" ] && echo 1 || echo 0)
    freed=$(( $(S legacy_orphans_freed) - f0 )); kept=$(( $(S legacy_nlink0_kept) - k0 ))
    reaped=$(( $(S unlinked_reaped) - r0 )); aborted=$(( $(S legacy_sweep_aborted) - a0 ))
    notes=$(( $(S legacy_barrier_notes) - n0 ))
    keep_ok=0; [ "$( (cd $M/keep && sha256 -q k*; sha256 -q $M/moved-legacyfile $M/moved-legacydir/sub/f) 2>/dev/null)" = "$(cat /root/lorph.keep.sha)" ] && keep_ok=1
    left=$(( $(ls $M/gone | wc -l) + $(ls $M/gonedir | wc -l) ))
    echo "mount_ok=1 mounted_during_sweep=$mounted_during_sweep paused=$paused moved_rc=$moved done=$done_ok barrier_notes=$notes"
    echo "still_paused_after_sync=$sp"
    echo "legacy_freed=$freed legacy_kept=$kept flagged_reaped=$reaped aborted=$aborted keep_ok=$keep_ok gone_left=$left"
    dmesg | grep -E "legacy orphan sweep" | tail -2 | cut -c1-240 | sed 's/^/  dmesg: /'
    sync; sleep 7
    cd /
    tess_umount $M || { echo "fsck_problems=SKIPPED_MOUNTED"; return; }
    d1=$(S legacy_sweeps_done)
    _remounted=0
    mount -t tessera $PART $M && { _remounted=1; sleep 3; cd /; tess_umount $M && _remounted=0; }
    echo "resweep=$(( $(S legacy_sweeps_done) - d1 ))"
    # ★ Never fsck while still mounted — it fails toward FALSE POSITIVES.
    [ $_remounted -eq 0 ] || { echo "fsck_problems=SKIPPED_MOUNTED"; return; }
    tessera-fsck $PART > /tmp/lorph.fsck 2>&1
    pat=$(tr ' ' '\n' < /root/lorph.legacy.inos | grep . | sed 's/.*/inode &[:( ]/' | paste -sd'|' -)
    probs=$(grep -iE 'dangling|orphan|nlink|leaked|overlap|missing|neither|corrupt' /tmp/lorph.fsck | grep -vE "$pat")
    echo "fsck_problems=$(printf '%s' "$probs" | grep -c .) legacy_lines=$(grep -cE "$pat" /tmp/lorph.fsck)"
    printf '%s\n' "$probs" | grep . | sed -E 's/[0-9]{3,}/N/g' | sort | uniq -c | sort -rn | head -4 | sed 's/^/  fsck: /'
    gpart destroy -F $DISK >/dev/null 2>&1
    rm -f /root/lorph.keep.sha /root/lorph.legacy.inos
}

# ── churn: the barrier under a real concurrent namespace load ────────────
#
#   churn-arm     1200-dir tree; 30 live legacy files + 10 live legacy dirs
#                 (nlink=0-as-unset), 20 old-kmod orphans held open; cut next
#   churn-verify  mount with the sweep slowed per directory while a churner
#                 keeps renaming the live legacy records between random dirs
#                 (publishing with sync), plus create/unlink/rename noise
churn_arm() {
    diskinfo -s $DEV | grep -q '^atrium-scratch$' || { echo REFUSING_ident; exit 2; }
    sysctl -n kern.tessera.fault_legacy_nlink0 >/dev/null 2>&1 || { echo NO_HOOK; exit 4; }
    mount | grep -q " $M " && timeout 120 umount -f $M
    gpart destroy -F $DISK >/dev/null 2>&1
    gpart create -s gpt $DISK >/dev/null && gpart add -t freebsd-ufs -s 256M -i 1 $DISK >/dev/null || { echo PART_FAIL; exit 3; }
    mkdir -p $M
    mkfs-tessera $PART >/dev/null 2>&1 && mount -t tessera $PART $M || { echo MKFS_FAIL; exit 3; }
    rm -rf /root/lorph.c; mkdir -p /root/lorph.c
    d=0; while [ $d -lt 300 ]; do
        mkdir -p $M/t/d$d/s0 $M/t/d$d/s1 $M/t/d$d/s2
        for sd in s0 s1 s2; do jot 4 | while read f; do echo "n $d $sd $f" > $M/t/d$d/$sd/f$f; done; done
        d=$((d+1))
    done
    k=0; while [ $k -lt 30 ]; do
        p=$M/t/d$((k*7 % 300))/s$((k % 3))/legacy$k
        echo "legacy file $k" > $p; echo $p > /root/lorph.c/f$k; k=$((k+1))
    done
    k=0; while [ $k -lt 10 ]; do
        p=$M/t/d$((k*13 % 300))/ldir$k
        mkdir -p $p; echo "inside legacy dir $k" > $p/inner; echo $p > /root/lorph.c/d$k; k=$((k+1))
    done
    mkdir -p $M/gone
    i=0; while [ $i -lt 20 ]; do echo "orphan $i" > $M/gone/v$i; i=$((i+1)); done
    sync; sleep 7
    linos=$(for f in /root/lorph.c/f* /root/lorph.c/d*; do stat -f %i $(cat $f); done | tr '\n' ' ')
    echo "$linos" > /root/lorph.legacy.inos
    vino=$(stat -f %i $M/gone/v* | tr '\n' ' ')
    i=0; while [ $i -lt 20 ]; do (exec 3<$M/gone/v$i; sleep 100000) >/dev/null 2>&1 & i=$((i+1)); done
    sleep 2; rm -f $M/gone/v*
    hooked=0
    for n in $vino $linos; do sysctl kern.tessera.fault_legacy_nlink0=$n >/dev/null 2>&1 && hooked=$((hooked+1)); done
    sync; sleep 7; sync; sleep 7
    echo "armed: dirs=$(find $M -type d | wc -l | tr -d ' ') hooked=$hooked"
}

churner() {  # runs until /tmp/lorph.stop exists (bounded by the caller)
    it=0
    while [ ! -e /tmp/lorph.stop ]; do
        for k in $(jot -r 4 0 29); do
            src=$(cat /root/lorph.c/f$k); dst=$M/t/d$(jot -r 1 0 299)/s$(jot -r 1 0 2)/legacy$k
            [ "$src" = "$dst" ] || { mv $src $dst 2>/dev/null && echo $dst > /root/lorph.c/f$k; }
        done
        k=$(jot -r 1 0 9); src=$(cat /root/lorph.c/d$k); dst=$M/t/d$(jot -r 1 0 299)/ldir$k
        [ "$src" = "$dst" ] || { mv $src $dst 2>/dev/null && echo $dst > /root/lorph.c/d$k; }
        n=$M/t/d$(jot -r 1 0 299)/s$(jot -r 1 0 2)
        echo "noise $it" > $n/x$it; mv $n/x$it $M/t/d$(jot -r 1 0 299)/y$it 2>/dev/null
        rm -f $M/t/d$(jot -r 1 0 299)/y$(jot -r 1 0 $it) 2>/dev/null
        it=$((it+1))
        [ $((it % 15)) = 0 ] && sync
    done
    echo $it > /tmp/lorph.iters
}

churn_verify() {
    diskinfo -s $DEV | grep -q '^atrium-scratch$' || { echo REFUSING_ident; exit 2; }
    [ -e $PART ] || { echo "verify: no $PART"; exit 3; }
    trap 'touch /tmp/lorph.stop; sysctl kern.tessera.fault_legacy_sweep_dir_delay_ms=0 kern.tessera.fault_legacy_sweep_nobarrier=0 kern.tessera.legacy_sweep_batch=4096 >/dev/null 2>&1' EXIT
    rm -f /tmp/lorph.stop /tmp/lorph.iters
    sysctl kern.tessera.legacy_orphan_sweep=2 kern.tessera.legacy_sweep_batch=50 \
        kern.tessera.fault_legacy_sweep_dir_delay_ms=${DIR_DELAY_MS:-10} \
        kern.tessera.fault_legacy_sweep_nobarrier=${NOBARRIER:-0} >/dev/null
    f0=$(S legacy_orphans_freed); k0=$(S legacy_nlink0_kept); a0=$(S legacy_sweep_aborted)
    d0=$(S legacy_sweeps_done); n0=$(S legacy_barrier_notes)
    mkdir -p $M
    dmesg -c >/dev/null 2>&1
    mount -t tessera $PART $M || { echo "mount_ok=0"; return; }
    churner & cpid=$!
    i=0; while [ $i -lt 1800 ] && [ "$(S legacy_sweeps_done)" = "$d0" ]; do sleep 0.1; i=$((i+1)); done
    done_ok=$([ "$(S legacy_sweeps_done)" != "$d0" ] && echo 1 || echo 0)
    touch /tmp/lorph.stop; wait $cpid
    freed=$(( $(S legacy_orphans_freed) - f0 )); kept=$(( $(S legacy_nlink0_kept) - k0 ))
    aborted=$(( $(S legacy_sweep_aborted) - a0 )); notes=$(( $(S legacy_barrier_notes) - n0 ))
    bad_legacy=0
    k=0; while [ $k -lt 30 ]; do [ "$(cat $(cat /root/lorph.c/f$k) 2>/dev/null)" = "legacy file $k" ] || bad_legacy=$((bad_legacy+1)); k=$((k+1)); done
    k=0; while [ $k -lt 10 ]; do [ "$(cat $(cat /root/lorph.c/d$k)/inner 2>/dev/null)" = "inside legacy dir $k" ] || bad_legacy=$((bad_legacy+1)); k=$((k+1)); done
    echo "mount_ok=1 done=$done_ok churn_iters=$(cat /tmp/lorph.iters 2>/dev/null) barrier_notes=$notes"
    echo "legacy_freed=$freed legacy_kept=$kept aborted=$aborted bad_legacy=$bad_legacy"
    dmesg | grep -E "legacy orphan sweep" | tail -2 | cut -c1-240 | sed 's/^/  dmesg: /'
    sync; sleep 7
    cd /
    timeout 180 umount $M || { echo "fsck_problems=SKIPPED_MOUNTED"; return; }
    tessera-fsck $PART > /tmp/lorph.fsck 2>&1
    pat=$(tr ' ' '\n' < /root/lorph.legacy.inos | grep . | sed 's/.*/inode &[:( ]/' | paste -sd'|' -)
    probs=$(grep -iE 'dangling|orphan|nlink|leaked|overlap|missing|neither|corrupt' /tmp/lorph.fsck | grep -vE "$pat")
    echo "fsck_problems=$(printf '%s' "$probs" | grep -c .) legacy_lines=$(grep -cE "$pat" /tmp/lorph.fsck)"
    printf '%s\n' "$probs" | grep . | sed -E 's/[0-9]{3,}/N/g' | sort | uniq -c | sort -rn | head -4 | sed 's/^/  fsck: /'
    gpart destroy -F $DISK >/dev/null 2>&1
    rm -rf /root/lorph.c /root/lorph.legacy.inos
}

case "${1:-}" in
arm)    arm ;;
verify) verify ;;
churn-arm)    churn_arm ;;
churn-verify) churn_verify ;;
*)      echo "usage: $0 arm|verify|churn-arm|churn-verify"; exit 2 ;;
esac
