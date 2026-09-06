#!/bin/sh
# #133 A/B — GC pass 2: header+index prefix read vs whole-pack read.
# Run DETACHED inside the guest: nohup sh vm-gc-pass2-ab.sh > /root/ab133.out &
#
# MEASURED 2026-09-06 on the dev root (14 GB used, ~128k packs), host idle:
#
#   arm                 pass-2 sectors   ≈GiB   whole-pass wall   reclaimed
#   whole-pack CONTROL   3357585/2886571  12.8    236s / 190s      7656 / 6
#   header+index PREFIX   242794/243072   0.93     55s /  50s         9 / 147
#
#   13.8x fewer pass-2 sectors, 0 fallbacks, 0 aborts. The ~3 GiB rd_MiB left
#   in EVERY arm is pass 1 (the liveness walk) — now the dominant cost.
#
#   Correctness: the follow-up pass reclaimed 9 (prefix after control) and 6
#   (control after prefix). Three consecutive CONTROL passes on a quiescent
#   volume reclaimed 279, 5, 3 — so 3-9 per pass is the volume's own churn
#   (GC's registry rewrites + cron/syslog superseding packs), not a
#   prefix-specific false-dead, which would be large and one-sided.
#
#   The scans_delta assertion read 0 in all four arms: the old gc_now never
#   incremented gc_scans, so the dead-arm check was itself dead. Fixed with
#   the gc_now rewrite; the mechanism evidence for THIS run is that
#   full_sectors / prefix_sectors moved only in the matching arm.
S(){ sysctl -n kern.tessera.$1 2>/dev/null || echo 0; }

seed() {  # $1 = label, ~256 MiB of UNIQUE content, then delete it
	mkdir -p /root/gcseed && cd /root/gcseed || exit 1
	i=0; while [ $i -lt 64 ]; do dd if=/dev/random of=s$i bs=1m count=4 2>/dev/null; i=$((i+1)); done
	sync; sleep 2; cd /root; rm -rf /root/gcseed; sync; sleep 2
	echo "seeded+deleted $1: 64 x 4 MiB unique"
}

arm() {   # $1 = arm label, $2 = prefix knob value
	sysctl kern.tessera.gc_pack_prefix_read=$2 >/dev/null
	ps0=$(S gc_pack_prefix_sectors); fs0=$(S gc_pack_full_sectors); fb0=$(S gc_pack_prefix_fallback)
	rc0=$(S gc_reclaimed); ab0=$(S gc_aborts); sc0=$(S gc_scans); ro0=$(S disk_rd_ops); rb0=$(S disk_rd_bytes); ms0=$(S gc_scan_ms)
	t0=$(date +%s); out=$(/root/tq / 2>&1); t1=$(date +%s)
	echo "ARM $1 (prefix=$2): $out wall=$((t1-t0))s"
	echo "   scans_delta=$(( $(S gc_scans)-sc0 ))  <- MUST be 1 or the arm is dead"
	echo "   pass2: prefix_sectors=$(( $(S gc_pack_prefix_sectors)-ps0 )) full_sectors=$(( $(S gc_pack_full_sectors)-fs0 )) fallback=$(( $(S gc_pack_prefix_fallback)-fb0 ))"
	echo "   reclaimed_delta=$(( $(S gc_reclaimed)-rc0 )) aborts_delta=$(( $(S gc_aborts)-ab0 )) scan_ms=$(( $(S gc_scan_ms)-ms0 ))"
	echo "   whole-pass: rd_ops=$(( $(S disk_rd_ops)-ro0 )) rd_MiB=$(( ($(S disk_rd_bytes)-rb0)/1048576 ))"
}

echo "=== #133 A/B start $(date) kmod=$(sha256 -q /boot/kernel/tessera_fs.ko | cut -c1-16) ==="
echo "volume: $(df -h / | tail -1)"
echo "--- Phase A: false-dead check (control first) ---"
seed A
arm CONTROL 0
arm PREFIX  1
echo "   => PREFIX reclaimed_delta above MUST be 0"
echo "--- Phase B: missed-dead check (prefix first) ---"
seed B
arm PREFIX  1
arm CONTROL 0
echo "   => CONTROL reclaimed_delta above MUST be 0"
sysctl kern.tessera.gc_pack_prefix_read=1 >/dev/null
echo "commit_failed=$(S commit_failed) pinscan_aborts=$(S pinscan_aborts)"
echo "=== DONE $(date) ==="
