#!/bin/sh
# Host wrapper for crash_rorw_recovery.sh: phase 1 (build crash state) -> power
# cut (qmp quit + relaunch) -> phase 2 (ro recovery + rw upgrade). The crash
# drive /dev/vtbd0 (vm/crash-test.img) is cache=directsync so committed sectors
# survive the cut. Assumes the dev VM was launched with plain run-vm.sh.
set -u
BSD=/Users/girivs/src/bsd
SOCK=/tmp/qmp.sock; VSSH="$BSD/scripts/vssh"

wait_ready(){ i=0; while [ $i -lt 90 ]; do $VSSH 'echo ready' >/dev/null 2>&1 && return 0; i=$((i+1)); sleep 3; done; echo "FATAL: VM never ready"; exit 1; }
gsetup(){ $VSSH 'kldload p9fs 2>/dev/null; mount -t p9fs -o trans=virtio bsd_share /mnt/host 2>/dev/null; mkdir -p /mnt/tessera' >/dev/null 2>&1; }
powercut(){ echo quit | nc -U -w2 $SOCK >/dev/null 2>&1; i=0; while [ $i -lt 15 ]; do pgrep -f "qemu-system-aarch64.*vm.qcow2" >/dev/null || break; i=$((i+1)); sleep 2; done; pgrep -f "qemu-system-aarch64.*vm.qcow2" >/dev/null && pkill -9 -f "qemu-system-aarch64.*vm.qcow2"; sleep 2; ( $BSD/scripts/run-vm.sh >/tmp/crash-rorw-vmboot.log 2>&1 & ); wait_ready; }

wait_ready; gsetup
id=$($VSSH 'diskinfo -s /dev/vtbd0' 2>/dev/null | tr -d '\r')
[ "$id" = "tessera-crashtest" ] || { echo "ABORT: vtbd0 ident=[$id] not tessera-crashtest"; exit 1; }

echo "########## PHASE 1 (build crash state) ##########"
$VSSH 'PHASE=1 sh /mnt/host/scratch/stress/crash_rorw_recovery.sh' 2>&1

echo "########## POWER CUT (qmp quit + relaunch) ##########"
powercut
gsetup

echo "########## PHASE 2 (ro recovery + rw upgrade) ##########"
$VSSH 'PHASE=2 sh /mnt/host/scratch/stress/crash_rorw_recovery.sh' 2>&1
