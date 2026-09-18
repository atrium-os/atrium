#!/bin/sh
# Guest half of scripts/vm-wedge-probe-test.sh — runs ON the dev VM.
#
# WHAT IT PINS DOWN: what a volume does when its METADATA RESERVE is full.
# Churn until the reserve is exhausted (admit_avail=0, writes refused with a
# large staged batch), stop the workload, then ask three questions the
# ordinary tests never reach:
#
#   1. does the staged backlog drain once the writers are gone, or is the
#      volume wedged? (before the flush-retry fix in 84586b60 nothing even
#      retried: gen, band refusals and drain failures all stood still)
#   2. can the volume still be EMPTIED — does rm free anything? (today it
#      reports success and frees nothing: the deletes are staged and then
#      discarded at unmount)
#   3. is the on-disk state consistent afterwards? (fsck, and note that fsck
#      UNDERSTATES snapshot damage — see kern.tessera.pinscan_late_snaps)
#
# STARVE=1 forces the state deterministically (background reclaim at 1% duty,
# pressure kicks off, flush preflight left on) — the natural workload only
# wedges about half the time, and a run that never wedges is not a result.
#
#   STARVE=1 PART_MB=128 NDEL=2000 sh guest-wedge-probe.sh
set -u
M=/mnt/rxp
S() { sysctl -n kern.tessera.$1 2>/dev/null || echo 0; }
mnt() { sysctl -n kern.tessera.mounts | awk -v m=$M '$1==m'; }
f() { mnt | sed -n "s/.* $1=\([0-9]*\).*/\1/p"; }
row() { printf '%-9s dirty=%-6s pending=%-5s free=%-5s avail=%-6s need=%-5s gen=%-8s band=%-9s drainf=%-6s cef=%s\n' \
    "$1" "$(f dirty)" "$(f pending)" "$(mnt | sed -n 's/.* free=\([0-9]*\)\/.*/\1/p')" \
    "$(f admit_avail)" "$(f admit_need)" "$(f gen)" "$(S meta_band_refusals)" "$(S flush_drain_failed)" "$(S commit_extent_failed)"
  printf '          fail_mft=%s fail_ino=%s first_chunk_fail=%s ino_written=%s retry_armed=%s preflight=%s freed=%s\n' \
    "$(S drain_fail_manifests)" "$(S drain_fail_inodes)" "$(S drain_fail_first_chunk)" "$(S drain_inodes_written)" \
    "$(S flush_retry_armed)" "$(S preflight_scans)" "$(S preflight_freed)"; }

PART_MB=${PART_MB:-256} STARVE=${STARVE:-0} PREFLIGHT=1 SECS=900 sh /root/guest-reserve-exhaustion.sh crash-arm | tail -1
# Wait until the reserve has no headroom, then probe whatever state exists.
# ★ Do NOT gate the probe on a cleverer "is it wedged" predicate: two earlier
# versions (dirty>=5000, then "avail==0 AND new drain failures") both reported
# stuck_reached=0 on runs whose drains were failing every second. This is a
# diagnostic, so it reports the state it found and lets the reader judge.
i=0
while [ $i -lt ${WAIT_S:-600} ]; do
    a=$(f admit_avail)
    [ -n "$a" ] && [ "$a" -le 64 ] 2>/dev/null && break
    sleep 5; i=$((i+5))
done
echo "after ${i}s: admit_avail=$(f admit_avail) dirty=$(f dirty) drain_fails=$(S drain_fail_inodes) (avail 0 = no headroom; drain_fails rising = flushes failing)"
row at-stuck
echo "--- stopping the workload"
pkill -f rxworker; sleep 2; pkill -9 -f rxworker 2>/dev/null
touch /root/rx.stop 2>/dev/null
t=0
while [ $t -lt 120 ]; do sleep 30; t=$((t+30)); row "quiet+$t"; done
echo "--- DELETE probe: can a full volume still be emptied?"
n=0
for d in $M/d*; do
    [ -d "$d" ] || continue
    for f in $d/*; do rm -f "$f" 2>/dev/null && n=$((n+1)); [ $n -ge ${NDEL:-2000} ] && break; done
    [ $n -ge ${NDEL:-2000} ] && break
done
echo "deleted=$n"
sync; sleep 10; row post-rm
sleep 30; row post-rm+30
sleep 60; row post-rm+90
sleep 60; row post-rm+150
echo "files_left=$(find $M -type f 2>/dev/null | wc -l | tr -d ' ')"
echo "--- sync"
t0=$(date +%s); sync; echo "sync rc=$? took $(( $(date +%s) - t0 ))s"; sleep 5; row post-sync
echo "--- write probe"
echo probe > $M/probe.$$ 2>&1 && echo "write_ok=1" || echo "write_ok=0"
row post-probe
echo "--- umount"
cd /; t0=$(date +%s); timeout 300 umount $M && echo "umount_ok=1 took $(( $(date +%s) - t0 ))s" || echo "umount_ok=0"
tessera-fsck /dev/vtbd2p1 > /tmp/stuck.fsck 2>&1; echo "fsck_problems=$(grep -ciE 'dangling|orphan|nlink|leaked|overlap|missing|neither|corrupt|problem' /tmp/stuck.fsck)"
grep -E "result:" /tmp/stuck.fsck | head -2 | sed 's/^/  /'
sh /root/guest-reserve-exhaustion.sh cleanup >/dev/null 2>&1; echo cleaned
