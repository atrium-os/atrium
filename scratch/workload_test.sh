#!/bin/sh
set -eu
BIN=/mnt/host/atrium-tessera/rs/target/aarch64-unknown-freebsd/release
umount /mnt/tessera 2>/dev/null || true
mdconfig -d -u 0 2>/dev/null || true
kldload /mnt/host/atrium-tessera/kmod/tessera_fs.ko

# Use a 64 MiB image for headroom — extracting even a small tarball
# pushes past the 16 MiB used in earlier tests.
$BIN/mkfs-tessera --create -s 64 --seed-file hello --seed-inode 1000 \
    --seed-content "Hello, Tessera!" /tmp/test.img >/dev/null
MD=$(mdconfig -a -t vnode -f /tmp/test.img)
mount -t tessera /dev/$MD /mnt/tessera

echo "--- df before ---"
df -k /mnt/tessera

echo "--- create a directory tree ---"
mkdir -p /mnt/tessera/proj/src /mnt/tessera/proj/docs /mnt/tessera/proj/tests
echo 'fn main() { println!("hi"); }' > /mnt/tessera/proj/src/main.rs
echo '# Project' > /mnt/tessera/proj/docs/README.md
echo 'mod test_a;' > /mnt/tessera/proj/tests/test_a.rs
echo 'mod test_b;' > /mnt/tessera/proj/tests/test_b.rs
echo 'name = "demo"' > /mnt/tessera/proj/Cargo.toml

echo "--- find ---"
find /mnt/tessera/proj -type f | sort

echo "--- wc -l ---"
wc -l $(find /mnt/tessera/proj -type f) 2>&1 | tail -10

echo "--- df after ---"
df -k /mnt/tessera

echo "--- chmod -R 644 + verify ---"
chmod 0640 /mnt/tessera/proj/src/main.rs
ls -la /mnt/tessera/proj/src/main.rs

echo "--- read-as-non-root: vop_access permission check ---"
# Make a file root-only; verify a different uid can't read.
chmod 0600 /mnt/tessera/proj/secret 2>/dev/null || echo 'X' > /mnt/tessera/proj/secret
chmod 0600 /mnt/tessera/proj/secret
chown 0:0 /mnt/tessera/proj/secret
ls -la /mnt/tessera/proj/secret
su -m nobody -c 'cat /mnt/tessera/proj/secret' 2>&1 | head -1

echo "--- mv across dirs + remount ---"
mv /mnt/tessera/proj/tests /mnt/tessera/proj/test
ls /mnt/tessera/proj/test/

umount /mnt/tessera
mount -t tessera /dev/$MD /mnt/tessera
echo "--- after remount ---"
find /mnt/tessera -type f | sort
cat /mnt/tessera/proj/src/main.rs
df -k /mnt/tessera

echo "--- rm -rf the whole tree, verify ---"
rm -rf /mnt/tessera/proj
ls /mnt/tessera/

umount /mnt/tessera
mount -t tessera /dev/$MD /mnt/tessera
ls /mnt/tessera/
df -k /mnt/tessera

umount /mnt/tessera
mdconfig -d -u 0
