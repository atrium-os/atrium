#!/bin/sh
# Faithful power-cut crash test for the #40 reflink crash-recovery bug.
# Runs from the macOS HOST. Each cycle: build accumulated reflink/append state
# on the persistent vtbd0 crashtest disk (directsync = faithful power cut),
# then ABRUPT `quit`+relaunch WHILE THE VOLUME IS MOUNTED AND DIRTY (real power
# loss — no clean umount, no skip_next_sb, no async-consumes-skip confound),
# then remount + fsck. Volume persists across cuts (accumulates), matching the
# in-guest repro's state. fsck-dirty after any cut = the bug, under faithful loss.
set -u
BSD=/Users/girivs/src/bsd
CYCLES=${CYCLES:-25}
PER=${PER:-6}          # reflink/append ops per cycle before the cut
SOCK=/tmp/qmp.sock; VSSH="$BSD/scripts/vssh"
BIN=/mnt/host/atrium-tessera/rs/target/aarch64-unknown-freebsd/release

wait_ready(){ i=0; while [ $i -lt 90 ]; do $VSSH 'echo ready' >/dev/null 2>&1 && return 0; i=$((i+1)); sleep 3; done; echo "FATAL: VM never ready"; exit 1; }
relaunch(){ echo quit | nc -U -w2 $SOCK >/dev/null 2>&1; i=0; while [ $i -lt 15 ]; do pgrep -f qemu-system-aarch64 >/dev/null || break; i=$((i+1)); sleep 2; done; pgrep -f qemu-system-aarch64 >/dev/null && pkill -9 -f qemu-system-aarch64; sleep 2; ( $BSD/scripts/run-vm.sh --gpusim >/tmp/pc-vmboot.log 2>&1 & ); wait_ready; }
gsetup(){ $VSSH 'kldload p9fs 2>/dev/null; mount -t p9fs -o trans=virtio bsd_share /mnt/host 2>/dev/null; umount /mnt/tessera 2>/dev/null; kldunload tessera_fs 2>/dev/null; kldload /mnt/host/atrium-tessera/kmod/tessera_fs.ko; mkdir -p /mnt/tessera' >/dev/null 2>&1; }

wait_ready; gsetup
id=$($VSSH 'diskinfo -s /dev/vtbd0' 2>/dev/null | tr -d '\r')
[ "$id" = "tessera-crashtest" ] || { echo "ABORT: vtbd0 ident=[$id] not tessera-crashtest"; exit 1; }
# fresh base volume with big + a dir
$VSSH "$BIN/mkfs-tessera --create -s 256 /dev/vtbd0 >/dev/null
  mount -t tessera /dev/vtbd0 /mnt/tessera
  echo seed > /mnt/tessera/seed
  dd if=/dev/random of=/mnt/tessera/big bs=4096 count=20 2>/dev/null
  mkdir /mnt/tessera/d; sync; umount /mnt/tessera" >/dev/null 2>&1

fail=0
c=1; while [ $c -le $CYCLES ]; do
  # build dirty accumulated state, then leave MOUNTED+DIRTY for the cut
  $VSSH "mount -t tessera /dev/vtbd0 /mnt/tessera 2>/dev/null
    j=1; while [ \$j -le $PER ]; do
      cp /mnt/tessera/big /mnt/tessera/rl_${c}_\$j 2>/dev/null
      echo m-${c}-\$j >> /mnt/tessera/big
      j=\$((j+1))
    done" >/dev/null 2>&1
  echo "cycle $c: $PER reflink+append ops done — POWER CUT (mounted+dirty)"
  relaunch; gsetup
  res=$($VSSH "mount -t tessera /dev/vtbd0 /mnt/tessera 2>/dev/null; umount /mnt/tessera 2>/dev/null
    $BIN/tessera-fsck /dev/vtbd0 2>&1 | grep -cE 'dangling|orphan|nlink|leaked|overlap|missing|neither'" 2>/dev/null | tail -1)
  if [ "${res:-0}" != "0" ]; then
    echo "  cycle $c: FSCK-DIRTY ($res problem-lines) after faithful power cut"
    $VSSH "$BIN/tessera-fsck /dev/vtbd0 2>&1 | grep -E 'result|dangling|orphan|leaked' | head -4" 2>/dev/null
    fail=$((fail+1))
    [ $fail -ge 2 ] && { echo "stopping after 2 fsck-dirty cuts"; break; }
  else
    echo "  cycle $c: fsck CLEAN"
  fi
  c=$((c+1))
done
echo "=== FAITHFUL POWERCUT: $fail fsck-dirty of $((c-1)) cuts ==="
