#!/bin/sh
# Crash-consistency test: power loss while the metadata reserve is exhausted
# and flushes are failing must not leave a volume that replay cannot trust.
#
# THE GAP THIS PINS DOWN
#
#   After a FAILED flush the in-memory roots run ahead of the durable ones
#   (the on-disk superblock, or a journal ROOT_UPDATE that replay would roll
#   forward to). The drains that already ran copy-on-write freed nodes those
#   durable roots still reference, into meta_pending. tessera_fs_meta_pending
#   _drain then releases pending sectors that the latest pinscan did not pin —
#   and it is called from the flush PREFLIGHT and the allocator's emergency
#   path, not only after a durable commit. Pinscan pins the in-memory roots and
#   retained snapshot records, and a snapshot record covers only the inode,
#   pack-registry and free-extent trees. So a durable blob-index, snapshots,
#   quota or dead-extent node could be recycled while the only superblock a
#   crash would find still points at it.
#
# HOW: guest-reserve-exhaustion.sh crash-arm builds a small volume, commits a
# set of checksummed files, starves reclaim (STARVE=1 by default) and starts
# churn; this script polls the flush counters and cuts power (QMP quit) only
# while flushes are failing (see the loop below), relaunches
# the VM, and crash-verify mounts (journal replay), walks the tree, re-reads
# the checksummed files, and fscks. Repeats CUTS times and STOPS AT THE FIRST
# BAD CUT (fail fast).
#
# PASS per cut: mounted, no STALE reads, no pinscan aborts, checksummed files
# intact, no walk errors, fsck clean. Dead-arm guard: the cut must have landed
# while flushes were failing (drain_failed or band_refusals > 0 before the cut).
#
#   sh scripts/vm-exhaustion-crash-test.sh   # CUTS=5 WARMUP=20 POLL=3 MAX_WAIT=600 MAX_SKIPS=$CUTS
set -u
BSD="$(cd "$(dirname "$0")/.." && pwd)"
CUTS=${CUTS:-5}; WARMUP=${WARMUP:-20}; POLL=${POLL:-3}; MAX_WAIT=${MAX_WAIT:-600}; MAX_SKIPS=${MAX_SKIPS:-$CUTS}; skipped=0
VSSH="$BSD/scripts/vssh"; SOCK=/tmp/qmp.sock
KEY="$HOME/.ssh/fresco_bsd_ed25519"
SSHO="-i $KEY -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o LogLevel=ERROR"
ENVS="PART_MB=${PART_MB:-256} STARVE=${STARVE:-1} SLOW_RECLAIM=${SLOW_RECLAIM:-0} PREFLIGHT=${PREFLIGHT:-1}"

wait_ready() { i=0; while [ $i -lt 60 ]; do timeout 15 $VSSH 'echo ready' 2>/dev/null | grep -q ready && return 0; i=$((i+1)); sleep 5; done; return 1; }
power_cut_and_relaunch() {
    echo quit | nc -U -w2 $SOCK >/dev/null 2>&1
    i=0; while [ $i -lt 15 ]; do pgrep -f qemu-system-aarch64 >/dev/null || break; i=$((i+1)); sleep 2; done
    pgrep -f qemu-system-aarch64 >/dev/null && pkill -9 -f qemu-system-aarch64; sleep 2
    ( cd "$BSD" && ./scripts/run-vm.sh >/tmp/exh-crash-boot.log 2>&1 </dev/null & ) >/dev/null 2>&1 </dev/null
    sleep 20
    wait_ready
}

KO="$BSD/atrium-tessera/kmod/tessera_fs.ko"
KMOD=$(shasum -a 256 "$KO" | cut -c1-16)
wait_ready || { echo "FAIL — VM not reachable"; exit 1; }
. "$BSD/scripts/lib/guest-ident.sh"   # GUEST_KMOD_HASH: the LOADED module, only on Laminar/RLC/tessera root
k=$(timeout 20 $VSSH "$GUEST_KMOD_HASH" 2>/dev/null | tr -d '\r')
echo "=== EXHAUSTION CRASH TEST $(date) guest_kmod=$k tree_kmod=$KMOD cuts=$CUTS warmup=${WARMUP}s poll=${POLL}s max_wait=${MAX_WAIT}s $ENVS ==="
[ "$k" = "$KMOD" ] || echo "NOTE: guest module differs from the tree (baseline run against an older kmod?)"

c=1
while [ $c -le $CUTS ]; do
    timeout 60 scp $SSHO -P 2222 "$BSD/scripts/guest-reserve-exhaustion.sh" root@localhost:/root/guest-reserve-exhaustion.sh >/dev/null \
        || { echo "FAIL — could not copy the guest script"; exit 1; }
    arm=$(timeout 900 $VSSH "$ENVS SECS=900 sh /root/guest-reserve-exhaustion.sh crash-arm" 2>&1 | tr -d '\r')
    case "$arm" in *armed:*) ;; *) echo "cut $c: FAIL — arm did not complete: $arm"; exit 1;; esac
    # Cut only WHILE flushes are failing. A fixed delay mostly landed in a
    # healthy window (PREFLIGHT=1 run: 3 of 5 cuts saw no failure at all),
    # so poll the counters and cut the moment one interval shows fresh
    # band refusals AND failed drains — plus, when the preflight is on, a
    # preflight scan within the last few intervals, so the release path is
    # live too. Never cut blind: if that state does not arrive within
    # MAX_WAIT the round FAILS as a dead arm.
    sleep $WARMUP
    waited=$WARMUP; prev=""; pre_hist="0 0 0"; armed_cut=0
    while [ $waited -lt $MAX_WAIT ]; do
        live=$(timeout 10 $VSSH "sh /root/guest-reserve-exhaustion.sh crash-live" 2>/dev/null | tr -d '\r' | grep '^live ')
        if [ -n "$live" ] && [ -n "$prev" ]; then
            g() { echo "$1" | sed -n "s/.* $2=\([0-9]*\).*/\1/p"; }
            db=$(( $(g "$live" band) - $(g "$prev" band) ))
            dd=$(( $(g "$live" drain) - $(g "$prev" drain) ))
            dp=$(( $(g "$live" pre) - $(g "$prev" pre) ))
            pre_hist="$(echo $pre_hist | cut -d' ' -f2-) $dp"
            pre_recent=$(echo $pre_hist | tr ' ' '+' | bc)
            if [ $db -gt 0 ] && [ $dd -gt 0 ] && { [ "${PREFLIGHT:-1}" = 0 ] || [ $pre_recent -gt 0 ]; }; then
                armed_cut=1; break
            fi
        fi
        [ -n "$live" ] && prev="$live"
        sleep $POLL; waited=$((waited + POLL))
    done
    st=$(timeout 30 $VSSH "sh /root/guest-reserve-exhaustion.sh crash-status" 2>/dev/null | tr -d '\r')
    if [ $armed_cut = 0 ]; then
        # Not a failure of the volume — the workload never reached the state
        # under test (with the preflight on, its rescans usually keep the
        # reserve alive). Re-arm instead of cutting blind; the run still
        # FAILS if it cannot land CUTS real cuts.
        skipped=$((skipped + 1))
        echo "cut $c: SKIPPED ($skipped/$MAX_SKIPS) — flushes never failed within ${MAX_WAIT}s (${st:-no status}); re-arming, no power cut"
        timeout 200 $VSSH "sh /root/guest-reserve-exhaustion.sh cleanup" >/dev/null 2>&1
        [ $skipped -le $MAX_SKIPS ] || { echo "FAIL — dead arm: only $((c - 1)) of $CUTS cuts landed while flushes were failing ($skipped attempts never got there)"; exit 1; }
        continue
    fi
    echo "cut $c: cutting at ${waited}s — last ${POLL}s interval: +$db refusals, +$dd failed drains; preflight scans in last 3 intervals: $pre_recent"
    power_cut_and_relaunch || { echo "cut $c: FAIL — VM did not come back after the power cut"; exit 1; }
    ver=$(timeout 1500 $VSSH "$ENVS sh /root/guest-reserve-exhaustion.sh crash-verify" 2>&1 | tr -d '\r')
    echo "cut $c: before cut: ${st:-status unavailable}"
    echo "$ver" | sed "s/^/cut $c: /"
    v() { echo "$ver" | sed -n "s/.*$1=\([0-9A-Z_]*\).*/\1/p" | head -1; }
    bad=""
    [ "$(v mount_ok)" = 1 ]         || bad="$bad mount-failed"
    [ "$(v stale)" = 0 ]            || bad="$bad stale=$(v stale)"
    [ "$(v pinscan_aborts)" = 0 ]   || bad="$bad pinscan_aborts=$(v pinscan_aborts)"
    [ "$(v keep_bad)" = 0 ]         || bad="$bad keep_bad=$(v keep_bad)"
    [ "$(v walk_errors)" = 0 ]      || bad="$bad walk_errors=$(v walk_errors)"
    [ "$(v fsck_problems)" = 0 ]    || bad="$bad fsck=$(v fsck_problems)"
    if [ -n "$bad" ]; then
        echo "FAIL — cut $c left a damaged volume:$bad"
        exit 1
    fi
    s_drain=$(echo "$st" | sed -n 's/.*drain_failed=\([0-9]*\).*/\1/p')
    s_band=$(echo "$st" | sed -n 's/.*band_refusals=\([0-9]*\).*/\1/p')
    s_pre=$(echo "$st" | sed -n 's/.*preflight_scans=\([0-9]*\).*/\1/p')
    if [ "${s_drain:-0}" = 0 ] && [ "${s_band:-0}" = 0 ]; then
        echo "cut $c: NOTE — flushes were not failing when power was cut; this cut does not test the gap"
    elif [ "${PREFLIGHT:-1}" = 1 ] && [ "${s_pre:-0}" = 0 ]; then
        echo "cut $c: NOTE — no preflight scan+drain ran during the failures; the main release path was not exercised"
    fi
    c=$((c+1))
done
echo "PASS — $CUTS power cuts, each landed while flushes were failing ($skipped attempt(s) skipped: never reached that state), every volume replayed clean"
