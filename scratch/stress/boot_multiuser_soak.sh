#!/bin/sh
# Boot the full-userland Tessera root to MULTIUSER, then churn+read the LIVE
# root for a sustained window to soak the race-clean pin-bitmap scan under a
# realistic (big tree + retained snapshots) workload. Reports dmesg error
# counts + pinscan stats over serial, then leaves the guest for a crash-kill
# fsck. Dev VM must be down.
set -u
BSD_DIR="$(cd "$(dirname "$0")/../.." && pwd)"
QEMU_DIR="$BSD_DIR/external/qemu-build"
QEMU="$QEMU_DIR/build/qemu-system-aarch64"
EFI_PAD="$BSD_DIR/vm/edk2-aarch64-code.fd"
VARS="$BSD_DIR/vm/edk2-vars-mu.fd"
ROOT="$BSD_DIR/vm/tessera-root.img"
LOG="${1:-/tmp/mu-soak.log}"
ITERS="${ITERS:-800}"
FIFO="/tmp/mu-soak.in"

cp "$BSD_DIR/vm/edk2-vars-blank.fd" "$VARS"
rm -f "$FIFO"; mkfifo "$FIFO"; : > "$LOG"

"$QEMU" -L "$QEMU_DIR/pc-bios" \
    -accel hvf -cpu host -machine virt,gic-version=3 -smp 4 -m 4096 \
    -drive if=pflash,format=raw,unit=0,file="$EFI_PAD",readonly=on \
    -drive if=pflash,format=raw,unit=1,file="$VARS" \
    -drive file="$ROOT",format=raw,cache=directsync,if=none,id=troot \
    -device virtio-blk-pci,drive=troot,serial=tessera-root,config-wce=on \
    -nographic -serial mon:stdio <"$FIFO" >"$LOG" 2>&1 &
QPID=$!
exec 3>"$FIFO"

ok=0
for i in $(seq 1 90); do
    grep -q "login:" "$LOG" && { ok=1; break; }
    grep -qiE "panic|mountroot>|Enter full pathname" "$LOG" && break
    sleep 1
done
if [ "$ok" = "1" ]; then
    sleep 1; printf 'root\n' >&3; sleep 2
    # confirm rw root
    printf 'mount | grep " / "\n' >&3; sleep 1
    # concurrent reader over the base tree + churn loop on the live root
    printf 'reader() { while :; do ls -laR /etc /bin /sbin >/dev/null 2>&1; cat /etc/* >/dev/null 2>&1; done; }\n' >&3
    printf 'reader & RPID=$!\n' >&3
    printf 'i=0; while [ $i -lt %s ]; do echo soak-$i > /root/s$((i%%8)); rm -f /root/s$((i%%8)); sync; i=$((i+1)); done\n' "$ITERS" >&3
    printf 'kill $RPID 2>/dev/null\n' >&3
    printf 'echo SOAK_LOOP_DONE\n' >&3
    # wait for the loop to finish (poll the log for the marker)
    done=0
    for i in $(seq 1 300); do
        grep -q "SOAK_LOOP_DONE" "$LOG" && { done=1; break; }
        # meta_reserve EXHAUST is recovered by the preflight — let the full
        # loop run and read the faithful count from the sysctl afterward.
        # Only a panic aborts the soak.
        grep -qi "panic" "$LOG" && break
        sleep 1
    done
    sleep 1
    # report from the guest
    printf 'echo BTREE_ERR=$(dmesg | grep -c "load_node.*kind")\n' >&3
    printf 'echo SNAPWALK_ABORT=$(dmesg | grep -cE "snapwalk|pinscan aborted")\n' >&3
    # meta_reserve EXHAUST is now a faithful sysctl counter (the printf is
    # rate-limited to 1/s, so a dmesg grep undercounts).
    printf 'echo EXHAUST=$(sysctl -n kern.tessera.meta_exhaust)\n' >&3
    printf 'echo STALE_SKIPPED=$(sysctl -n kern.tessera.pinscan_stale_snap)\n' >&3
    printf 'echo PINSCAN_LOG=$(dmesg | grep -c "pinscan done")\n' >&3
    printf 'echo COMMITS=$(sysctl -n kern.tessera.sb_commits)\n' >&3
    printf 'sync\n' >&3
    printf 'echo SOAK_REPORT_DONE\n' >&3
    for i in $(seq 1 30); do grep -q "SOAK_REPORT_DONE" "$LOG" && break; sleep 1; done
    sleep 1
fi
kill "$QPID" 2>/dev/null || true      # crash-kill (directsync = durable)
exec 3>&- ; rm -f "$FIFO"
