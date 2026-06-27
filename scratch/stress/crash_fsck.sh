#!/bin/sh
# In-guest crash-injection campaign with tessera-fsck as the oracle.
#
# Runs ENTIRELY in the VM — no host power-cut, so it sidesteps the
# macOS/HVF storage-fidelity problem (see project_tessera_mmap_coherence).
# The "crash" is software-injected: kern.tessera.skip_next_sb=1 makes the
# next commit_sb journal+checkpoint the records but SKIP writing the SB
# sectors — exactly a power-loss between the journal landing and the SB
# becoming durable. Remount replay must roll the records forward.
#
# After every cycle we run tessera-fsck on the (unmounted) image: a much
# stronger check than "did my files reappear" — it validates SB, pack
# registry + blob content hashes, inode tree, and full blob reachability.
# Any cycle that leaves the FS fsck-dirty is a real recovery bug.
#
# Env: CYCLES (default 30).
set -u
CYCLES=${CYCLES:-30}
BIN=/mnt/host/atrium-tessera/rs/target/aarch64-unknown-freebsd/release
IMG=/tmp/ct.img

umount /mnt/tessera 2>/dev/null || true
mdconfig -d -u 0 2>/dev/null || true
kldunload tessera_fs 2>/dev/null || true
kldload /mnt/host/atrium-tessera/kmod/tessera_fs.ko

$BIN/mkfs-tessera --create -s 64 /tmp/ct.img >/dev/null
MD=$(mdconfig -a -t vnode -f $IMG)
mount -t tessera /dev/$MD /mnt/tessera
echo seed > /mnt/tessera/seed
# a chunked file + a dir, so fsck exercises reachability across cycles
dd if=/dev/random of=/mnt/tessera/big bs=4096 count=20 2>/dev/null
mkdir /mnt/tessera/d
umount /mnt/tessera
mdconfig -d -u $MD

echo "--- baseline fsck ---"
$BIN/tessera-fsck $IMG | grep result
fail=0
i=1
while [ $i -le $CYCLES ]; do
    MD=$(mdconfig -a -t vnode -f $IMG)
    mount -t tessera /dev/$MD /mnt/tessera
    sysctl kern.tessera.skip_next_sb=1 >/dev/null
    case $((i % 5)) in
    0) echo "iter $i" > /mnt/tessera/log$i ;;
    1) echo "more-$i" >> /mnt/tessera/big ;;
    2) mkdir -p /mnt/tessera/d/sub$i ;;
    3) cp /mnt/tessera/big /mnt/tessera/rl$i ;;        # reflink
    4) [ -f /mnt/tessera/log$((i-5)) ] && rm /mnt/tessera/log$((i-5)) ;;
    esac
    umount /mnt/tessera          # crash-inject: SB write skipped
    mdconfig -d -u $MD
    # remount → replay rolls the journaled record forward, commits clean
    MD=$(mdconfig -a -t vnode -f $IMG)
    mount -t tessera /dev/$MD /mnt/tessera
    umount /mnt/tessera
    mdconfig -d -u $MD
    # ORACLE
    if $BIN/tessera-fsck $IMG >/tmp/fsck.out 2>&1; then
        :
    else
        echo "=== cycle $i: FSCK DIRTY after crash+replay ==="
        cat /tmp/fsck.out
        fail=$((fail+1))
        [ $fail -ge 3 ] && { echo "stopping after 3 failures"; break; }
    fi
    i=$((i+1))
done

echo "--- final fsck ---"
$BIN/tessera-fsck $IMG
rm -f $IMG
echo "=== crash+fsck campaign: $fail dirty cycle(s) of $CYCLES ==="
