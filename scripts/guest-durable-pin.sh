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

# Guest half of scripts/vm-durable-pin-crash-test.sh — runs ON the dev VM.
#
#   guest-durable-pin.sh arm      build, commit, then fail commits + release +
#                                 reuse, leaving the volume mounted for the cut
#   guest-durable-pin.sh verify   after the reboot: mount (replay), read the
#                                 committed data, count STALE reads, fsck
#   guest-durable-pin.sh cleanup  restore hooks, unmount, drop the partition
set -u
DISK=vtbd2; DEV=/dev/vtbd2; PART=/dev/vtbd2p1; M=/mnt/dpin
S() { sysctl -n kern.tessera.$1 2>/dev/null || echo 0; }
gen() { sysctl -n kern.tessera.mounts | awk -v m="$M" '$1==m { for (i=1;i<=NF;i++) if ($i ~ /^gen=/) { sub("gen=","",$i); print $i } }'; }

cleanup() {
    sysctl kern.tessera.fault_commit_fail=0 kern.tessera.commit_failed_clear=1 >/dev/null 2>&1
    mount | grep -q " $M " && timeout 120 umount $M 2>/dev/null
    mount | grep -q " $M " || gpart destroy -F $DISK >/dev/null 2>&1
    echo "cleanup: done"
}

files() {  # $1 dir, $2 count, $3 tag — distinct small content per file
    mkdir -p $1
    jot $2 | while read i; do echo "$3 $i $(date +%s%N)" > $1/f$i; done
}

arm() {
    diskinfo -s $DEV | grep -q '^atrium-scratch$' || { echo REFUSING_ident; exit 2; }
    sysctl -n kern.tessera.fault_pinscan_drain >/dev/null 2>&1 || { echo NO_HOOKS; exit 4; }
    cleanup >/dev/null
    gpart create -s gpt $DISK >/dev/null && gpart add -t freebsd-ufs -s 256M -i 1 $DISK >/dev/null || { echo PART_FAIL; exit 3; }
    mkdir -p $M
    mkfs-tessera $PART >/dev/null 2>&1 && mount -t tessera $PART $M || { echo MKFS_FAIL; exit 3; }

    # 1. Committed state: a blob index several leaves wide, plus checksummed
    #    files. Wait for the commit to land (generation moves) before going on.
    files $M/committed 3000 committed
    mkdir -p $M/keep; i=0; while [ $i -lt 10 ]; do dd if=/dev/random of=$M/keep/k$i bs=65536 count=2 2>/dev/null; i=$((i+1)); done
    g0=$(gen); sync; sleep 2; sync; sleep 7
    (cd $M/keep && sha256 -q k*) > /root/dpin.keep.sha
    (cd $M/committed && cat f* | sha256 -q) > /root/dpin.committed.sha
    gdur=$(gen)
    echo "committed: gen $g0 -> $gdur"

    # 2. Every commit on THIS volume now fails after the flush's drains have
    #    run: the in-memory blob index advances, the durable one stays at gdur.
    dmesg -c >/dev/null 2>&1
    cf0=$(S commit_failed)
    sysctl kern.tessera.fault_commit_fail=2 >/dev/null
    r=0; while [ $r -lt 6 ]; do
        files $M/round$r 500 round$r
        sync; sleep 7
        sysctl kern.tessera.commit_failed_clear=1 >/dev/null
        r=$((r+1))
    done
    pend=$(sysctl -n kern.tessera.mounts | awk -v m="$M" '$1==m' | sed -n 's/.* pending=\([0-9]*\).*/\1/p')
    free0=$(sysctl -n kern.tessera.mounts | awk -v m="$M" '$1==m' | sed -n 's/.* free=\([0-9]*\)\/.*/\1/p')

    # 3. The release a failed flush's preflight performs: scan + pending drain.
    sysctl kern.tessera.fault_pinscan_drain=1 >/dev/null
    free1=$(sysctl -n kern.tessera.mounts | awk -v m="$M" '$1==m' | sed -n 's/.* free=\([0-9]*\)\/.*/\1/p')

    # 4. Reuse: allocations now draw on whatever was released.
    r=0; while [ $r -lt 4 ]; do
        files $M/reuse$r 800 reuse$r
        sync; sleep 7
        sysctl kern.tessera.commit_failed_clear=1 >/dev/null
        r=$((r+1))
    done
    echo "armed: durable_gen=$gdur gen_now=$(gen) commit_failures=$(( $(S commit_failed) - cf0 )) pending_before_drain=$pend free_before_drain=$free0 free_after_drain=$free1 stale_live=$(dmesg | grep -c STALE)"
}

verify() {
    diskinfo -s $DEV | grep -q '^atrium-scratch$' || { echo REFUSING_ident; exit 2; }
    [ -e $PART ] || { echo "verify: no $PART"; exit 3; }
    mkdir -p $M
    mount -t tessera $PART $M || { echo "mount_ok=0"; tessera-fsck $PART 2>&1 | tail -5; return; }
    echo "mount_ok=1 gen_after_replay=$(gen)"
    read_err=$( (cd $M/committed && cat f* > /dev/null) 2>&1 | wc -l | tr -d ' ')
    csum=$( (cd $M/committed && cat f* 2>/dev/null | sha256 -q) )
    committed_ok=0; [ "$csum" = "$(cat /root/dpin.committed.sha)" ] && committed_ok=1
    keep_ok=0; [ "$( (cd $M/keep && sha256 -q k*) 2>/dev/null)" = "$(cat /root/dpin.keep.sha)" ] && keep_ok=1
    walk_err=$(find $M 2>&1 >/dev/null | wc -l | tr -d ' ')
    sleep 3
    echo "committed_ok=$committed_ok keep_ok=$keep_ok read_errors=$read_err walk_errors=$walk_err stale=$(dmesg | grep -c STALE) pinscan_aborts=$(dmesg | grep -c 'pinscan aborted')"
    dmesg | grep STALE | head -3 | cut -c1-160 | sed 's/^/  dmesg: /'
    cd /
    if tess_umount $M; then
        tessera-fsck $PART > /tmp/dpin.fsck 2>&1
        echo "fsck_problems=$(grep -ciE 'dangling|orphan|nlink|leaked|overlap|missing|neither|corrupt|problem|stale|kind' /tmp/dpin.fsck)"
        grep -iE 'dangling|orphan|nlink|leaked|overlap|missing|neither|corrupt|problem|stale|kind' /tmp/dpin.fsck | sed -E 's/[0-9]{3,}/N/g' | sort | uniq -c | sort -rn | head -4 | sed 's/^/  fsck: /'
    else
        echo "fsck_problems=SKIPPED_MOUNTED"
    fi
    gpart destroy -F $DISK >/dev/null 2>&1
    rm -f /root/dpin.keep.sha /root/dpin.committed.sha
}

case "${1:-}" in
arm)     arm ;;
verify)  verify ;;
cleanup) cleanup ;;
*)       echo "usage: $0 arm|verify|cleanup"; exit 2 ;;
esac
