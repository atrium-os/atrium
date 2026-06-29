#!/bin/sh
# Head-to-head: extract base.txz (~30k files, ~600MB) onto ZFS vs UFS vs
# Tessera, same VM, same local source. Metadata-heavy create workload —
# the one where Tessera was orders of magnitude slow. Durations via the
# guest monotonic clock (deltas, robust to any host-clock offset).
set -u
BIN=/mnt/host/atrium-tessera/rs/target/aarch64-unknown-freebsd/release
SRC=/root/base.txz
KMOD=/mnt/host/atrium-tessera/kmod/tessera_fs.ko
R=/tmp/bench.txt; : > "$R"

run() { # $1=label $2=dst
  sync; T0=$(date +%s)
  tar -xpf "$SRC" -C "$2" 2>/dev/null
  sync; T1=$(date +%s)
  n=$(find "$2" 2>/dev/null | wc -l | tr -d ' ')
  echo "$1 : $((T1 - T0))s  ($n files)" >> "$R"
}

# ZFS on the spare 16G disk
zpool destroy bench 2>/dev/null
if zpool create -f bench /dev/vtbd1 2>/dev/null; then
  run "ZFS    " /bench
  zpool destroy bench 2>/dev/null
fi

# UFS+SU on the same disk
if newfs -U /dev/vtbd1 >/dev/null 2>&1; then
  mkdir -p /mnt/ufs && mount /dev/vtbd1 /mnt/ufs && run "UFS+SU " /mnt/ufs && umount /mnt/ufs
fi

# Tessera on vtbd2
umount /mnt/troot 2>/dev/null; umount -f /mnt/troot 2>/dev/null
kldload "$KMOD" 2>/dev/null
"$BIN/mkfs-tessera" -j 8192 --create -s 3072 /dev/vtbd2 >/dev/null 2>&1
mkdir -p /mnt/troot && mount -t tessera /dev/vtbd2 /mnt/troot && run "Tessera" /mnt/troot && umount /mnt/troot

echo "BENCH_DONE" >> "$R"
cat "$R"
