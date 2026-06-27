#!/bin/sh
# Dual-superblock CRC-fallback test. Tessera keeps two SB copies (sector 0
# = SB-A, sector 1 = SB-B). A torn/corrupt copy must be detected (CRC/HMAC)
# and the other used; if BOTH are bad, mount must refuse rather than mount
# garbage. Pure userspace corruption — no kmod change, no power-cut needed.
# fsck confirms the mounted-from-the-good-copy FS is fully consistent.
set -u
BIN=/mnt/host/atrium-tessera/rs/target/aarch64-unknown-freebsd/release
umount /mnt/tessera 2>/dev/null || true
mdconfig -d -u 0 2>/dev/null || true
kldunload tessera_fs 2>/dev/null || true
kldload /mnt/host/atrium-tessera/kmod/tessera_fs.ko

$BIN/mkfs-tessera --create -s 64 /tmp/sb.img >/dev/null
MD=$(mdconfig -a -t vnode -f /tmp/sb.img); mount -t tessera /dev/$MD /mnt/tessera
echo seed > /mnt/tessera/seed
dd if=/dev/random of=/mnt/tessera/big bs=4096 count=20 2>/dev/null
umount /mnt/tessera; mdconfig -d -u $MD

chk() {  # $1 = image path, $2 = expectation note
    M=$(mdconfig -a -t vnode -f "$1")
    if mount -t tessera /dev/$M /mnt/tessera 2>/dev/null; then
        s=$(cat /mnt/tessera/seed 2>/dev/null)
        umount /mnt/tessera; mdconfig -d -u $M
        c=$($BIN/tessera-fsck "$1" 2>&1 | grep -c CLEAN)
        echo "$2: mounted seed=[$s] fsck-clean=$c"
    else
        mdconfig -d -u $M 2>/dev/null
        echo "$2: MOUNT REFUSED"
    fi
}

# flip a CRC-covered byte (offset 100; magic at 0 stays intact so this
# exercises CRC/HMAC validation, not just a magic check)
cp /tmp/sb.img /tmp/sbA.img;    printf '\xff' | dd of=/tmp/sbA.img    bs=1 seek=100  count=1 conv=notrunc 2>/dev/null
cp /tmp/sb.img /tmp/sbB.img;    printf '\xff' | dd of=/tmp/sbB.img    bs=1 seek=4196 count=1 conv=notrunc 2>/dev/null
cp /tmp/sb.img /tmp/sbBoth.img; printf '\xff' | dd of=/tmp/sbBoth.img bs=1 seek=100  count=1 conv=notrunc 2>/dev/null
                                printf '\xff' | dd of=/tmp/sbBoth.img bs=1 seek=4196 count=1 conv=notrunc 2>/dev/null

echo "--- dual-SB CRC fallback ---"
chk /tmp/sbA.img    "SB-A corrupt (expect use SB-B)"
chk /tmp/sbB.img    "SB-B corrupt (expect use SB-A)"
chk /tmp/sbBoth.img "both corrupt (expect REFUSED)"
rm -f /tmp/sb.img /tmp/sbA.img /tmp/sbB.img /tmp/sbBoth.img
