#!/bin/sh
# Guest half of scripts/vm-reserve-exhaustion-test.sh — runs ON the dev VM.
#
#   guest-reserve-exhaustion.sh cleanup     kill leftovers, restore tunables,
#                                           unmount, drop the test partition
#   guest-reserve-exhaustion.sh run         one exhaustion run (env below)
#
# ★ A SMALL VOLUME, NOT GLOBAL TUNABLES. The first harness reached the band on
# the 4 GiB scratch disk by starving reclaim with kern.tessera.* knobs — which
# are GLOBAL, so the dev ROOT was starved too: it ran out of metadata, refused
# creates for 37 minutes after the run, and even the harness's own stop file
# (on /root) failed with ENOSPC. A 256 MiB GPT partition of the scratch disk
# has a 4,096-sector reserve (soft 3,584), small enough to exhaust with
# ordinary load and default tunables. (NOT md over the root: that deadlocks —
# see vm-meta-exhaustion-test.sh.) STARVE=1 still applies the old knobs for an
# experiment that needs them; they are restored by every cleanup.
#
# ★ SELF-CLEANING. The first harness ran its body inline over ssh; when a run
# outlived the host's timeout the guest shell kept going and the next run
# started on top of it — three generations of workers, each run's `rm` of the
# shared stop file cancelling the last one's stop. So: every run (and the host,
# before and after) calls `cleanup`; workers carry a marker and a hard deadline;
# stop files and logs live in tmpfs (/tmp), independent of the root's space.
#
# Env for `run`: SECS PART_MB DIRS PER_DIR WORKLOAD STARVE SLOW_RECLAIM TRIGGER
set -u
DISK=vtbd2; DEV=/dev/vtbd2; PART=/dev/vtbd2p1; M=/mnt/rxp
MARK=rxworker
RUN=/tmp/rx                    # tmpfs: stop files, worker errors
DEFAULTS=/root/rx.defaults     # tunables as found before the first STARVE run
KNOBS="pinscan_duty_pct pinscan_tight_bypass meta_pressure_pct meta_pressure_pending preflight mark_dirty_meta_trigger"

S() { sysctl -n kern.tessera.$1 2>/dev/null || echo 0; }

restore_defaults() {
    [ -f $DEFAULTS ] || return 0
    while read -r k v; do sysctl "kern.tessera.$k=$v" >/dev/null 2>&1; done < $DEFAULTS
}

cleanup() {
    mkdir -p $RUN; touch $RUN/stop.all
    pkill -f "$MARK" 2>/dev/null
    i=0; while pgrep -f "$MARK" >/dev/null 2>&1 && [ $i -lt 30 ]; do sleep 1; i=$((i+1)); done
    pkill -9 -f "$MARK" 2>/dev/null
    rm -f $RUN/stop.all
    restore_defaults
    if mount | grep -q " $M "; then
        timeout 120 umount $M 2>/dev/null || echo "cleanup: $M would not unmount"
    fi
    mount | grep -q " $M " || gpart destroy -F $DISK >/dev/null 2>&1
    echo "cleanup: done (leftover workers: $(pgrep -f "$MARK" | wc -l | tr -d ' '))"
}

run() {
    SECS=${SECS:-120}; PART_MB=${PART_MB:-256}
    # PART_MB=0: the whole scratch disk (4 GiB, 65,536-sector reserve) with
    # 400k files — the configuration that corrupted a volume organically with
    # TRIGGER=1 on 2026-09-14. Otherwise a small partition with 20k files.
    if [ "$PART_MB" = 0 ]; then
        DIRS=${DIRS:-800}; PER_DIR=${PER_DIR:-500}
    else
        DIRS=${DIRS:-40}; PER_DIR=${PER_DIR:-500}
    fi
    diskinfo -s $DEV | grep -q '^atrium-scratch$' || { echo REFUSING_ident; exit 2; }
    cleanup >/dev/null
    mount | grep -q "/dev/$DISK" && umount -f /dev/$DISK 2>/dev/null
    if [ "$PART_MB" = 0 ]; then
        PART=$DEV
    else
        gpart create -s gpt $DISK >/dev/null && gpart add -t freebsd-ufs -s ${PART_MB}M -i 1 $DISK >/dev/null \
            || { echo PART_FAIL; exit 3; }
    fi
    mkdir -p $M $RUN
    mkfs-tessera $PART >/dev/null 2>&1 && mount -t tessera $PART $M || { echo MKFS_FAIL; exit 3; }
    rm -f $RUN/err; : > $RUN/err
    RUNID=$$.$(date +%s); STOP=$RUN/stop.$RUNID

    dmesg -c >/dev/null 2>&1
    B0=$(S meta_band_refusals); CE0=$(S commit_extent_failed); AR0=$(S meta_admit_refusals)
    if [ "${STARVE:-0}" = 1 ] || [ "${SLOW_RECLAIM:-0}" = 1 ] || [ "${TRIGGER:-0}" = 1 ]; then
        [ -f $DEFAULTS ] || for k in $KNOBS; do v=$(sysctl -n kern.tessera.$k 2>/dev/null) && echo "$k $v"; done > $DEFAULTS
    fi
    if [ "${TRIGGER:-0}" = 1 ]; then
        sysctl kern.tessera.mark_dirty_meta_trigger=1 >/dev/null 2>&1 || { echo NO_TRIGGER_KNOB; cleanup >/dev/null; exit 4; }
    fi

    d=0; created=0
    while [ $d -lt $DIRS ]; do
        mkdir $M/d$d 2>/dev/null || break
        (cd $M/d$d && jot $PER_DIR | xargs touch 2>/dev/null)
        d=$((d+1))
    done
    sync; sleep 3
    populated=$(find $M -type f 2>/dev/null | wc -l | tr -d ' ')

    # STARVE=1: reclaim triggers OFF (pressure kicks, preflight) — this froze
    #   the dev root once; use only when that is the point.
    # SLOW_RECLAIM=1: duty cycle at its minimum and no tight bypass, triggers
    #   left on. Milder, and what the corrupting run on 2026-09-14 used.
    if [ "${STARVE:-0}" = 1 ]; then
        sysctl kern.tessera.meta_pressure_pct=0 kern.tessera.meta_pressure_pending=0 kern.tessera.preflight=0 >/dev/null
    fi
    if [ "${STARVE:-0}" = 1 ] || [ "${SLOW_RECLAIM:-0}" = 1 ]; then
        sysctl kern.tessera.pinscan_duty_pct=1 >/dev/null
        sysctl kern.tessera.pinscan_tight_bypass=0 >/dev/null 2>&1
    fi

    DEADLINE=$(( $(date +%s) + SECS ))
    per=$(( DIRS / 4 ))
    w=0; while [ $w -lt 4 ]; do
        # WORKLOAD=bulk (default): dirty every inode in the worker's
        #   directories, create 50, remove the previous 50, sync.
        # WORKLOAD=spread: one inode per directory, a create and a remove in
        #   every directory, sync — many small commits; the shape of the run
        #   that corrupted a volume on 2026-09-14.
        sh -c 'w=$1; STOP=$2; DEADLINE=$3; per=$4; PER=$5; WL=$6; M=/mnt/rxp; RUN=/tmp/rx; r=0
          while [ ! -f "$STOP" ] && [ ! -f $RUN/stop.all ] && [ $(date +%s) -lt $DEADLINE ]; do
            d=$(( w * per )); e=$(( d + per ))
            if [ "$WL" = spread ]; then
              files=""; news=""
              while [ $d -lt $e ]; do files="$files $M/d$d/$(( (r % PER) + 1 ))"; news="$news $M/d$d/n$w.$r"; d=$((d+1)); done
              touch -c $files 2>/dev/null; touch $news 2>/dev/null
              [ $r -gt 0 ] && { d=$(( w * per )); while [ $d -lt $e ]; do rm -f $M/d$d/n$w.$((r-1)) 2>>$RUN/err; d=$((d+1)); done; }
            else
              while [ $d -lt $e ]; do (cd $M/d$d 2>/dev/null && jot $PER | xargs touch -c 2>/dev/null); d=$((d+1)); done
              d=$(( w * per ))
              j=0; while [ $j -lt 50 ]; do touch $M/d$d/n$w.$r.$j 2>/dev/null; j=$((j+1)); done
              [ $r -gt 0 ] && { j=0; while [ $j -lt 50 ]; do rm -f $M/d$d/n$w.$((r-1)).$j 2>>$RUN/err; j=$((j+1)); done; }
            fi
            sync; r=$((r+1)); echo $r > $RUN/rounds.$w
          done' $MARK $w $STOP $DEADLINE $per $PER_DIR ${WORKLOAD:-bulk} 2>>$RUN/err &
        w=$((w+1))
    done
    sleep $SECS
    touch $STOP
    i=0; while pgrep -f "$MARK" >/dev/null 2>&1 && [ $i -lt 120 ]; do sleep 1; i=$((i+1)); done
    STUCK=$(pgrep -f "$MARK" | wc -l | tr -d ' ')
    BAND=$(( $(S meta_band_refusals) - B0 ))
    ROUNDS=$(cat $RUN/rounds.* 2>/dev/null | awk '{s+=$1} END{print s+0}'); rm -f $RUN/rounds.*
    restore_defaults

    # Recovery with default tunables, in two phases.
    #   passive: ONE create attempt (refused if the reserve is still short —
    #            which must itself schedule reclaim), then no sync, no further
    #            I/O: the volume has to come back on its own. This is the
    #            livelock that left the dev root refusing for 37 minutes.
    #   active:  sync + probe, as any later workload would.
    touch $M/.recovery-probe 2>/dev/null && rm -f $M/.recovery-probe
    t=0; recovered_passive=0
    while [ $t -lt 45 ]; do
        sysctl -n kern.tessera.mounts | grep -q "^$M .* admitting" && { recovered_passive=1; break; }
        sleep 3; t=$((t+3))
    done
    t=0; recovered=$recovered_passive
    while [ $recovered = 0 ] && [ $t -lt 90 ]; do
        sync
        sysctl -n kern.tessera.mounts | grep -q "^$M .* admitting" && { recovered=1; break; }
        touch $M/.recovery-probe 2>/dev/null && rm -f $M/.recovery-probe
        sleep 3; t=$((t+3))
    done
    sleep 5; sync
    echo "populated=$populated rounds=$ROUNDS band_refusals=$BAND commit_extent_failed=$(( $(S commit_extent_failed) - CE0 )) admit_refusals=$(( $(S meta_admit_refusals) - AR0 )) stale=$(dmesg | grep -c STALE) pinscan_aborts=$(dmesg | grep -c 'pinscan aborted') drain_failed=$(dmesg | grep -c 'drain failed') inode_enoent=$(dmesg | grep -c 'INODE-ENOENT') commit_sb_failed=$(dmesg | grep -c 'commit_sb failed') worker_errs=$(wc -l < $RUN/err | tr -d ' ') stuck_workers=$STUCK recovered_passive=$recovered_passive recovered=$recovered admit_flush_kicks=$(S meta_admit_flush_kicks)"
    sysctl -n kern.tessera.mounts | grep "^$M" | sed 's/^/  /'
    miss=0; d=0; while [ $d -lt $DIRS ]; do [ -e $M/d$d/$PER_DIR ] || miss=$((miss+1)); d=$((d+1)); done
    echo "survivor_missing=$miss"
    pkill -9 -f "$MARK" 2>/dev/null
    cd /; sync
    if timeout 180 umount $M 2>/dev/null; then echo umount_ok=1; else echo umount_ok=0; fi
    if mount | grep -q " $M "; then
        echo "fsck_problems=SKIPPED_MOUNTED"   # never fsck a mounted volume
    else
        tessera-fsck $PART > $RUN/fsck 2>&1
        echo "fsck_problems=$(grep -ciE 'dangling|orphan|nlink|leaked|overlap|missing|neither|corrupt|problem' $RUN/fsck)"
        grep -iE 'dangling|orphan|nlink|leaked|overlap|missing|neither|corrupt|problem' $RUN/fsck | sed -E 's/[0-9]{3,}/N/g' | sort | uniq -c | sort -rn | head -5 | sed 's/^/  fsck: /'
    fi
    rm -f $STOP
}

# ── crash variant ─────────────────────────────────────────────────────────
# crash-arm:    volume + population + load tunables + DETACHED workers, then
#               return — the host cuts power mid-churn (QMP quit).
# crash-verify: after the reboot — mount (journal replay), walk the tree, count
#               STALE reads and replay refusals, unmount, fsck, drop the
#               partition. Tunables need no restore: the reboot reset them.
crash_arm() {
    SECS=${SECS:-600}; PART_MB=${PART_MB:-256}
    DIRS=${DIRS:-40}; PER_DIR=${PER_DIR:-500}
    diskinfo -s $DEV | grep -q '^atrium-scratch$' || { echo REFUSING_ident; exit 2; }
    cleanup >/dev/null
    gpart create -s gpt $DISK >/dev/null && gpart add -t freebsd-ufs -s ${PART_MB}M -i 1 $DISK >/dev/null \
        || { echo PART_FAIL; exit 3; }
    mkdir -p $M $RUN
    mkfs-tessera $PART >/dev/null 2>&1 && mount -t tessera $PART $M || { echo MKFS_FAIL; exit 3; }
    d=0; while [ $d -lt $DIRS ]; do mkdir $M/d$d; (cd $M/d$d && jot $PER_DIR | xargs touch 2>/dev/null); d=$((d+1)); done
    # Content with a known checksum, committed before any stress: after the
    # crash it must read back byte-exact (a recycled blob-index or registry
    # node would lose or misdirect it).
    mkdir -p $M/keep; i=0; while [ $i -lt 20 ]; do dd if=/dev/random of=$M/keep/f$i bs=65536 count=4 2>/dev/null; i=$((i+1)); done
    sync; sleep 3
    (cd $M/keep && sha256 -q f* ) > $RUN/keep.sha 2>/dev/null
    cp $RUN/keep.sha /root/rx.keep.sha
    # ★ The flush PREFLIGHT stays ON here even under STARVE: its pinscan +
    # meta_pending_drain after a failed flush is the path under test, and
    # `run`'s STARVE turned it off. Only the pressure kicks go.
    if [ "${STARVE:-0}" = 1 ]; then
        sysctl kern.tessera.meta_pressure_pct=0 kern.tessera.meta_pressure_pending=0 >/dev/null
        sysctl kern.tessera.preflight=${PREFLIGHT:-1} >/dev/null
    fi
    if [ "${STARVE:-0}" = 1 ] || [ "${SLOW_RECLAIM:-0}" = 1 ]; then
        sysctl kern.tessera.pinscan_duty_pct=1 >/dev/null
        sysctl kern.tessera.pinscan_tight_bypass=0 >/dev/null 2>&1
    fi
    B0=$(S meta_band_refusals); echo "$B0" > $RUN/b0
    dmesg -c >/dev/null 2>&1
    DEADLINE=$(( $(date +%s) + SECS )); per=$(( DIRS / 4 ))
    w=0; while [ $w -lt 4 ]; do
        daemon -f sh -c 'w=$1; DEADLINE=$2; per=$3; PER=$4; M=/mnt/rxp; r=0
          while [ $(date +%s) -lt $DEADLINE ]; do
            d=$(( w * per )); e=$(( d + per ))
            while [ $d -lt $e ]; do (cd $M/d$d 2>/dev/null && jot $PER | xargs touch -c 2>/dev/null); d=$((d+1)); done
            d=$(( w * per ))
            j=0; while [ $j -lt 50 ]; do echo "$w.$r.$j" > $M/d$d/n$w.$r.$j 2>/dev/null; j=$((j+1)); done
            [ $r -gt 0 ] && { j=0; while [ $j -lt 50 ]; do rm -f $M/d$d/n$w.$((r-1)).$j; j=$((j+1)); done; }
            sync; r=$((r+1))
          done' $MARK $w $DEADLINE $per $PER_DIR
        w=$((w+1))
    done
    echo "armed: $(pgrep -f $MARK | wc -l | tr -d ' ') workers"
}

crash_stale_lines() {
    dmesg | grep -B2 STALE | head -${1:-12}
}

crash_status() {
    # Inodes whose tombstone deletes were failing just before the cut —
    # compared with the orphans fsck finds after it.
    dmesg | sed -n 's/.*btree_delete inode_no=\([0-9]*\) failed.*/\1/p' | sort -un > /root/rx.tomb_failed
    cp /root/rx.tomb_failed /root/rx.tomb_failed.keep 2>/dev/null

    echo "band_refusals=$(( $(S meta_band_refusals) - $(cat $RUN/b0 2>/dev/null || echo 0) )) drain_failed=$(dmesg | grep -c 'drain failed') commit_extent_failed=$(S commit_extent_failed) preflight_scans=$(dmesg | grep -c 'preflight') stale_live=$(dmesg | grep -c STALE)"
}

crash_verify() {
    PART_MB=${PART_MB:-256}; DIRS=${DIRS:-40}; PER_DIR=${PER_DIR:-500}
    diskinfo -s $DEV | grep -q '^atrium-scratch$' || { echo REFUSING_ident; exit 2; }
    [ -e $PART ] || { echo "verify: no $PART after reboot"; exit 3; }
    mkdir -p $M $RUN
    if ! mount -t tessera $PART $M; then
        echo "mount_ok=0"
        tessera-fsck $PART > $RUN/fsck 2>&1
        echo "fsck_problems=$(grep -ciE 'dangling|orphan|nlink|leaked|overlap|missing|neither|corrupt|problem|stale|invalid|bad' $RUN/fsck)"
        return
    fi
    echo "mount_ok=1 unlinked_reaped=$(S unlinked_reaped)"
    walk_err=$(find $M -type f 2>&1 >/dev/null | wc -l | tr -d ' ')
    files=$(find $M -type f 2>/dev/null | wc -l | tr -d ' ')
    keep_bad=0
    if [ -f /root/rx.keep.sha ]; then
        (cd $M/keep 2>/dev/null && sha256 -q f* ) > $RUN/keep.after 2>/dev/null
        cmp -s /root/rx.keep.sha $RUN/keep.after || keep_bad=$(diff /root/rx.keep.sha $RUN/keep.after | grep -c '^<')
    fi
    miss=0; d=0; while [ $d -lt $DIRS ]; do [ -e $M/d$d/$PER_DIR ] || miss=$((miss+1)); d=$((d+1)); done
    sleep 5; sync
    echo "files=$files walk_errors=$walk_err keep_bad=$keep_bad survivor_missing=$miss stale=$(dmesg | grep -c STALE) pinscan_aborts=$(dmesg | grep -c 'pinscan aborted') replay_refused=$(dmesg | grep -c 'REFUSED') replay=\"$(dmesg | grep 'journal replay (full)' | tail -1 | sed 's/.*applied //')\""
    cd /; if timeout 180 umount $M 2>/dev/null; then echo umount_ok=1; else echo umount_ok=0; fi
    if mount | grep -q " $M "; then
        echo "fsck_problems=SKIPPED_MOUNTED"
    else
        tessera-fsck $PART > $RUN/fsck 2>&1
        echo "fsck_problems=$(grep -ciE 'dangling|orphan|nlink|leaked|overlap|missing|neither|corrupt|problem' $RUN/fsck)"
        grep -iE 'dangling|orphan|nlink|leaked|overlap|missing|neither|corrupt|problem' $RUN/fsck | sed -E 's/[0-9]{3,}/N/g' | sort | uniq -c | sort -rn | head -5 | sed 's/^/  fsck: /'
        orph=$(sed -n 's/.*orphan inode \([0-9]*\).*/\1/p' $RUN/fsck | sort -un | tr '\n' ' ')
        if [ -n "$orph" ]; then
            hit=0; for o in $orph; do grep -qx "$o" /root/rx.tomb_failed.keep 2>/dev/null && hit=$((hit+1)); done
            echo "orphans=$orph tomb_failed_before_cut=$(wc -l < /root/rx.tomb_failed.keep 2>/dev/null | tr -d ' ') orphans_with_failed_tombstone=$hit"
            grep -E "inode ($(echo $orph | tr ' ' '|'))\b|orphan inode" $RUN/fsck | head -6 | sed 's/^/  fsck-detail: /'
        fi
    fi
    gpart destroy -F $DISK >/dev/null 2>&1; rm -f /root/rx.keep.sha /root/rx.tomb_failed.keep
}

case "${1:-}" in
cleanup)      cleanup ;;
crash-stale)  crash_stale_lines 40 ;;
run)          run ;;
crash-arm)    crash_arm ;;
crash-status) crash_status ;;
crash-verify) crash_verify ;;
*)            echo "usage: $0 cleanup|run|crash-arm|crash-status|crash-verify"; exit 2 ;;
esac
