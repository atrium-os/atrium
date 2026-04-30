#!/bin/sh
# Minimal reproducer for the truncate-up zero-fill bug fsx caught.
#
# Scenario:
#   1. Write 0x7000 bytes of pattern at offset 0.
#   2. Truncate UP to 0x2b000 (extends file by 0x24000 bytes; new region
#      should be zero-filled per POSIX).
#   3. Write at offset 0x11000 (above old EOF, below new EOF). Doesn't
#      touch [0x7000, 0x11000).
#   4. Read [0xa000, 0xa800) — must be all zeros.
set -eu
BIN=/mnt/host/atrium-tessera/rs/target/aarch64-unknown-freebsd/release
umount /mnt/tessera 2>/dev/null || true
mdconfig -d -u 0 2>/dev/null || true
kldunload tessera_fs 2>/dev/null || true
kldload /mnt/host/atrium-tessera/kmod/tessera_fs.ko

$BIN/mkfs-tessera --create -s 64 /tmp/tu.img >/dev/null
MD=$(mdconfig -a -t vnode -f /tmp/tu.img)
mount -t tessera /dev/$MD /mnt/tessera

F=/mnt/tessera/x

echo "--- step 1: write 0x7000 bytes of 0x55 pattern ---"
yes A | dd of=$F bs=4096 count=7 2>/dev/null
ls -la $F | awk '{print "  size:", $5}'

echo "--- step 2: truncate up to 0x2b000 (176128) ---"
truncate -s 176128 $F
ls -la $F | awk '{print "  size:", $5}'

echo "--- step 3: read [0xa000, 0xa800) — should be all zeros ---"
dd if=$F of=/tmp/gap.bin bs=1 skip=40960 count=2048 2>/dev/null
HASH=$(sha256 -q /tmp/gap.bin)
ZERO_HASH=$(dd if=/dev/zero bs=1 count=2048 2>/dev/null | sha256 -q)
echo "  read hash:     $HASH"
echo "  expected zero: $ZERO_HASH"
[ "$HASH" = "$ZERO_HASH" ] || { echo "FAIL: post-truncate gap not zero"; umount /mnt/tessera; exit 1; }
echo "  zeros confirmed (pre-write step)"

echo "--- step 4: write at offset 0x11000 (69632), 0xde0b bytes (56843) ---"
# fsx's pattern is one pwrite() syscall per op, not per-byte. The bs=1
# variant we tried first was pathologically slow (each byte triggered
# a full INLINE manifest rewrite). Use a tiny C helper for a single
# pwrite at a non-aligned offset.
cat > /tmp/pwrite_at.c <<'CEOF'
#include <stdio.h>
#include <stdlib.h>
#include <unistd.h>
#include <fcntl.h>
#include <string.h>
int main(int ac, char **av) {
    if (ac != 4) return 2;
    int fd = open(av[1], O_WRONLY);
    if (fd < 0) { perror("open"); return 1; }
    long off = strtol(av[2], 0, 0);
    long len = strtol(av[3], 0, 0);
    char *buf = malloc(len);
    memset(buf, 'B', len);
    if (pwrite(fd, buf, len, off) != len) { perror("pwrite"); return 1; }
    close(fd);
    return 0;
}
CEOF
cc -O0 /tmp/pwrite_at.c -o /tmp/pwrite_at
/tmp/pwrite_at $F 69632 56843
ls -la $F | awk '{print "  size:", $5}'

echo "--- step 5: re-read [0xa000, 0xa800) — must STILL be all zeros ---"
dd if=$F of=/tmp/gap2.bin bs=1 skip=40960 count=2048 2>/dev/null
HASH2=$(sha256 -q /tmp/gap2.bin)
echo "  read hash:     $HASH2"
echo "  expected zero: $ZERO_HASH"
if [ "$HASH2" = "$ZERO_HASH" ]; then
    echo "  PASS — zero-fill preserved across post-truncate write"
else
    echo "  FAIL — gap is no longer zero after the post-truncate write"
    echo "  first non-zero bytes:"
    hexdump -C /tmp/gap2.bin | head -5 | sed 's/^/    /'
    umount /mnt/tessera; mdconfig -d -u 0
    exit 1
fi

umount /mnt/tessera
mdconfig -d -u 0
echo DONE
