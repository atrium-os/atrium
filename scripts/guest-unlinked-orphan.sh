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

# Guest half of scripts/vm-unlinked-orphan-crash-test.sh — runs ON the dev VM.
#
#   guest-unlinked-orphan.sh arm      files held open, then unlinked, then the
#                                     unlink committed; leaves them open for
#                                     the power cut
#   guest-unlinked-orphan.sh verify   after the reboot: mount (replay + reap),
#                                     check survivors, unmount, fsck
set -u
DISK=vtbd2; DEV=/dev/vtbd2; PART=/dev/vtbd2p1; M=/mnt/uorph
N=${N:-40}
S() { sysctl -n kern.tessera.$1 2>/dev/null || echo 0; }
gen() { sysctl -n kern.tessera.mounts | awk -v m="$M" '$1==m { for (i=1;i<=NF;i++) if ($i ~ /^gen=/) { sub("gen=","",$i); print $i } }'; }

arm() {
    diskinfo -s $DEV | grep -q '^atrium-scratch$' || { echo REFUSING_ident; exit 2; }
    mount | grep -q " $M " && timeout 120 umount -f $M
    gpart destroy -F $DISK >/dev/null 2>&1
    gpart create -s gpt $DISK >/dev/null && gpart add -t freebsd-ufs -s 256M -i 1 $DISK >/dev/null || { echo PART_FAIL; exit 3; }
    mkdir -p $M
    mkfs-tessera $PART >/dev/null 2>&1 && mount -t tessera $PART $M || { echo MKFS_FAIL; exit 3; }

    # Survivors: must still be there, byte-exact, after the reap.
    mkdir -p $M/keep $M/gone $M/gonedir
    i=0; while [ $i -lt 10 ]; do dd if=/dev/random of=$M/keep/k$i bs=16384 count=3 2>/dev/null; i=$((i+1)); done
    # A hardlinked file whose other name is removed must survive too.
    echo hardlink-survivor > $M/keep/hl; ln $M/keep/hl $M/gone/hl2
    # Victims: held open by a sleeper each, then unlinked.
    i=0; while [ $i -lt $N ]; do dd if=/dev/random of=$M/gone/v$i bs=8192 count=2 2>/dev/null; i=$((i+1)); done
    i=0; while [ $i -lt 5 ]; do mkdir $M/gonedir/d$i; i=$((i+1)); done
    sync; sleep 7
    (cd $M/keep && sha256 -q k* hl) > /root/uorph.keep.sha
    i=0; while [ $i -lt $N ]; do (exec 3<$M/gone/v$i; sleep 100000) >/dev/null 2>&1 & i=$((i+1)); done
    i=0; while [ $i -lt 5 ]; do (cd $M/gonedir/d$i && exec sleep 100000) >/dev/null 2>&1 & i=$((i+1)); done
    sleep 2
    g0=$(gen)
    rm -f $M/gone/v* $M/gone/hl2
    rmdir $M/gonedir/d*
    sync; sleep 7; sync; sleep 7
    held=$(fstat -f $M 2>/dev/null | grep -c sleep)
    echo "armed: gen $g0 -> $(gen) held_open=$held victims=$N dirs=5"
}

verify() {
    diskinfo -s $DEV | grep -q '^atrium-scratch$' || { echo REFUSING_ident; exit 2; }
    [ -e $PART ] || { echo "verify: no $PART"; exit 3; }
    r0=$(S unlinked_reaped)
    mkdir -p $M
    mount -t tessera $PART $M || { echo "mount_ok=0"; return; }
    reaped=$(( $(S unlinked_reaped) - r0 ))
    keep_ok=0; [ "$( (cd $M/keep && sha256 -q k* hl) 2>/dev/null)" = "$(cat /root/uorph.keep.sha)" ] && keep_ok=1
    left=$(ls $M/gone | wc -l | tr -d ' ')
    hl_nlink=$(stat -f %l $M/keep/hl 2>/dev/null)
    echo "mount_ok=1 reaped=$reaped reap_ms=$(S unlinked_reap_ms) keep_ok=$keep_ok gone_left=$left hl_nlink=$hl_nlink"
    dmesg | grep -E "freed [0-9]+ unlinked" | tail -2 | sed 's/^/  dmesg: /'
    cd /
    if tess_umount $M; then
        tessera-fsck $PART > /tmp/uorph.fsck 2>&1
        echo "fsck_problems=$(grep -ciE 'dangling|orphan|nlink|leaked|overlap|missing|neither|corrupt|problem' /tmp/uorph.fsck)"
        grep -iE 'dangling|orphan|nlink|leaked|overlap|missing|neither|corrupt|problem' /tmp/uorph.fsck | sed -E 's/[0-9]{3,}/N/g' | sort | uniq -c | sort -rn | head -4 | sed 's/^/  fsck: /'
    else
        echo "fsck_problems=SKIPPED_MOUNTED"
    fi
    gpart destroy -F $DISK >/dev/null 2>&1
    rm -f /root/uorph.keep.sha
}

case "${1:-}" in
arm)    arm ;;
verify) verify ;;
*)      echo "usage: $0 arm|verify"; exit 2 ;;
esac
