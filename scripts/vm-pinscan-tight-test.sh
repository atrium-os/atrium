#!/bin/sh
# A/B test: the pinscan duty-cycle bypass for a TIGHT metadata reserve.
#
# THE MECHANISM UNDER TEST
#
#   Pinscan is the only thing that releases meta_pending sectors back to
#   meta_free, and its duty-cycle throttle (kern.tessera.pinscan_duty_pct)
#   buys a quiet period of last_scan_ms x (100/pct - 1) after every scan. On
#   the dev root that quiet (21 s after a 1.1 s mount scan) outlasted a commit
#   burst that moved the whole free list into meta_pending, and creates failed
#   ENOSPC until it ran out. c4af77fd let pressure kicks skip the quiet while
#   tessera_fs_meta_tight() holds (available < pending, or < 1/4 of the soft
#   reserve).
#
#   The boot that verified c4af77fd never took that path — the extent-flush
#   fix alone kept the root healthy — so the bypass had never run. This test
#   makes it run, and measures what it changes, by switching it with
#   kern.tessera.pinscan_tight_bypass on the same workload.
#
# HOW IT GETS THERE, on the scratch disk only:
#   1. fresh mkfs, 400k files in 800 directories: a pinscan of ~40 ms, so
#      pinscan_duty_pct=1 buys ~4 s of quiet per scan,
#   2. churn from 4 workers: every round dirties one inode in each of the 800
#      directories (COWing leaves across the whole inode tree), creates 800
#      files, removes the previous round's 800, and syncs (a commit),
#   3. sample kern.tessera.mounts at 5 Hz.
#
# ★ THROUGHPUT IS `rounds`, NOT `commits`. Commits are not work here:
#   tessera_fs_mark_dirty runs a SYNCHRONOUS flush on every mutation while
#   (bump - meta_free) * 2 >= reserve length, and meta_pending counts as used,
#   so a reserve short on recycled sectors turns each create/remove into its
#   own commit. The first version of this test reported commits and read the
#   bypass arm's ~30% fewer as a cost; it was doing 2.8x the work (dtrace: 37%
#   of mutations flushed synchronously with the bypass off, 10.5% with it on).
#   `sync_flush_samples` counts the samples in that per-mutation-flush state.
#
# ★ DEAD-ARM GUARDS. The bypass arm must show pinscan_duty_bypass_tight > 0,
#   and the control arm pinscan_duty_tight_honoured > 0 (the reserve was
#   tight and the quiet was honoured). A run where neither counter moved never
#   reached a tight reserve and says nothing about the bypass either way.
#   Both counters are global, so the root contributes too; the root is
#   healthy and idle here, and the per-run deltas are reported regardless.
#
# ARMS are BYPASS:TRIGGER pairs — kern.tessera.pinscan_tight_bypass and
# kern.tessera.mark_dirty_meta_trigger (1 = staged-need trigger, 0 = the legacy
# "(bump - meta_free) >= half the reserve" one). Default: the full 2x2.
#
#   sh scripts/vm-pinscan-tight-test.sh              # 3 runs per arm, 60 s churn
#   REPS=1 SECS=30 ARMS="1:0 1:1" sh scripts/vm-pinscan-tight-test.sh
set -u
BSD="$(cd "$(dirname "$0")/.." && pwd)"
REPS=${REPS:-3}; SECS=${SECS:-60}; ARMS=${ARMS:-"0:0 1:0 0:1 1:1"}
VSSH="$BSD/scripts/vssh"

KO="$BSD/atrium-tessera/kmod/tessera_fs.ko"
[ -f "$KO" ] || { echo "no $KO — build it first"; exit 1; }
KMOD=$(shasum -a 256 "$KO" | cut -c1-16)
. "$BSD/scripts/lib/guest-ident.sh"   # GUEST_KMOD_HASH: the LOADED module, only on Laminar/RLC/tessera root
k=$($VSSH "$GUEST_KMOD_HASH" 2>/dev/null | tr -d '\r')
[ "$k" = "$KMOD" ] || { echo "ABORT: guest module [$k] != tree [$KMOD]"; exit 1; }
$VSSH "sysctl -n kern.tessera.pinscan_tight_bypass kern.tessera.mark_dirty_meta_trigger" >/dev/null 2>&1 \
    || { echo "ABORT: guest kmod lacks pinscan_tight_bypass / mark_dirty_meta_trigger"; exit 1; }

echo "=== PINSCAN TIGHT-BYPASS / MARK_DIRTY TRIGGER A/B $(date) kmod=$KMOD reps=$REPS secs=$SECS arms=$ARMS ==="

run_arm() {  # $1 = bypass (0|1), $2 = mark_dirty trigger (0 legacy | 1)
    timeout $((SECS + 600)) $VSSH "
M=/mnt/scratch; DEV=/dev/vtbd2
diskinfo -s \$DEV | grep -q '^atrium-scratch\$' || { echo REFUSING_ident; exit 2; }
mount | grep -q \" \$M \" && umount \$M
mkdir -p \$M
mkfs-tessera \$DEV >/dev/null 2>&1 && mount -t tessera \$DEV \$M || { echo MKFS_FAIL; exit 3; }
i=0; while [ \$i -lt 800 ]; do mkdir \$M/d\$i; (cd \$M/d\$i && jot 500 | xargs touch); i=\$((i+1)); done
sync; sleep 8
S(){ sysctl -n kern.tessera.\$1 2>/dev/null || echo 0; }
OLD_DUTY=\$(S pinscan_duty_pct)
sysctl kern.tessera.pinscan_duty_pct=1 kern.tessera.pinscan_tight_bypass=$1 kern.tessera.mark_dirty_meta_trigger=$2 >/dev/null
B0=\$(S pinscan_duty_bypass_tight); H0=\$(S pinscan_duty_tight_honoured)
R0=\$(S meta_admit_refusals); K0=\$(S pinscan_kicks); D0=\$(S pinscan_skips_duty)
C0=\$(S sb_commits); MF0=\$(S mark_dirty_meta_flushes)

rm -f /root/pt.stop /root/pt.samples /root/pt.rounds.*; : > /root/pt.err
sh -c 'while [ ! -f /root/pt.stop ]; do sysctl -n kern.tessera.mounts | grep \"^/mnt/scratch \" >> /root/pt.samples; sleep 0.2; done' &
w=0; while [ \$w -lt 4 ]; do
  sh -c 'w=\$1; r=0; M=/mnt/scratch
    while [ ! -f /root/pt.stop ]; do
      i=\$(( (w * 200) )); e=\$(( i + 200 )); files=\"\"; news=\"\"
      while [ \$i -lt \$e ]; do files=\"\$files \$M/d\$i/\$(( (r % 500) + 1 ))\"; news=\"\$news \$M/d\$i/n\$w.\$r\"; i=\$((i+1)); done
      touch -c \$files; touch \$news
      [ \$r -gt 0 ] && { i=\$(( w * 200 )); while [ \$i -lt \$e ]; do rm -f \$M/d\$i/n\$w.\$((r-1)); i=\$((i+1)); done; }
      sync; r=\$((r+1)); echo \$r > /root/pt.rounds.\$w
    done' _ \$w 2>>/root/pt.err &
  w=\$((w+1))
done
sleep $SECS
touch /root/pt.stop; sleep 3; wait 2>/dev/null
sysctl kern.tessera.pinscan_duty_pct=\$OLD_DUTY kern.tessera.pinscan_tight_bypass=1 kern.tessera.mark_dirty_meta_trigger=1 >/dev/null

MINAV=\$(sed -n 's/.*admit_avail=\([0-9]*\).*/\1/p' /root/pt.samples | sort -n | head -1)
MAXPEND=\$(sed -n 's/.* pending=\([0-9]*\).*/\1/p' /root/pt.samples | sort -n | tail -1)
REFS=\$(grep -c REFUSING /root/pt.samples)
ROUNDS=\$(cat /root/pt.rounds.* 2>/dev/null | awk '{s+=\$1} END{print s+0}')
# tessera_fs_mark_dirty runs a SYNCHRONOUS flush on every mutation while
# (bump - meta_free) * 2 >= reserve length. Count the samples in that state.
SYNCF=\$(awk '{for(i=1;i<=NF;i++){split(\$i,a,\"=\"); v[a[1]]=a[2]} split(v[\"free\"],f,\"/\"); if ((v[\"bump\"] - f[1]) * 2 >= v[\"len\"]) n++} END{print n+0}' /root/pt.samples)
TIGHT=\$(awk '{for(i=1;i<=NF;i++){split(\$i,a,\"=\"); v[a[1]]=a[2]} av=v[\"admit_avail\"]; if (av < v[\"pending\"] || av < v[\"soft\"]/4) t++} END{print t+0}' /root/pt.samples)
echo \"arm=$1:$2 bypass=$1 trigger=$2 rounds=\$ROUNDS meta_flushes=\$(( \$(S mark_dirty_meta_flushes)-MF0 )) samples=\$(wc -l < /root/pt.samples | tr -d ' ') sync_flush_samples=\$SYNCF tight_samples=\$TIGHT refusing_samples=\$REFS min_avail=\$MINAV max_pending=\$MAXPEND bypass_tight=\$(( \$(S pinscan_duty_bypass_tight)-B0 )) tight_honoured=\$(( \$(S pinscan_duty_tight_honoured)-H0 )) kicks=\$(( \$(S pinscan_kicks)-K0 )) skips_duty=\$(( \$(S pinscan_skips_duty)-D0 )) admit_refusals=\$(( \$(S meta_admit_refusals)-R0 )) commits=\$(( \$(S sb_commits)-C0 )) enospc=\$(grep -ci 'no space' /root/pt.err) errs=\$(wc -l < /root/pt.err | tr -d ' ')\"
sync; umount \$M 2>/dev/null || echo UMOUNT_FAIL
tessera-fsck \$DEV > /root/pt.fsck 2>&1
echo \"fsck_problems=\$(grep -ciE 'dangling|orphan|nlink|leaked|overlap|missing|neither|corrupt|problem' /root/pt.fsck)\"" 2>&1 | tr -d '\r'
}

# ★ FAIL FAST, AND LEAVE NOTHING BEHIND. This script drives the guest inline
# over ssh, so a host-side interrupt used to leave its workers running, the
# scratch volume mounted and the global tunables at duty 1% / no bypass. Record
# the tunables first and restore them — and stop everything — on ANY exit.
# FAIL_FAST=1 (default) stops at the first rep that fails instead of spending
# the rest of a ~40 min run on a result already known to be bad.
FAIL_FAST=${FAIL_FAST:-1}
ORIG=$($VSSH "echo \$(sysctl -n kern.tessera.pinscan_duty_pct) \$(sysctl -n kern.tessera.pinscan_tight_bypass) \$(sysctl -n kern.tessera.mark_dirty_meta_trigger)" 2>/dev/null | tr -d '\r')
set -- $ORIG
O_DUTY=${1:-5}; O_BYP=${2:-1}; O_TRIG=${3:-1}
guest_cleanup() {
    timeout 240 $VSSH "touch /root/pt.stop; pkill -f 'sh -c w='; sleep 3; pkill -9 -f 'sh -c w='; \
        sysctl kern.tessera.pinscan_duty_pct=$O_DUTY kern.tessera.pinscan_tight_bypass=$O_BYP kern.tessera.mark_dirty_meta_trigger=$O_TRIG >/dev/null 2>&1; \
        mount | grep -q ' /mnt/scratch ' && timeout 120 umount /mnt/scratch; true" >/dev/null 2>&1
}
trap guest_cleanup EXIT
trap 'exit 130' INT TERM

rc=0; LOG=""
r=1; while [ $r -le $REPS ]; do
    for arm in $ARMS; do
        OUT=$(run_arm ${arm%:*} ${arm#*:})
        echo "rep $r: $OUT" | tr '\n' ' '; echo
        LOG="$LOG
$OUT"
        case "$OUT" in *REFUSING_ident*|*MKFS_FAIL*|*UMOUNT_FAIL*) echo "FAIL — harness could not run"; exit 1;; esac
        echo "$OUT" | grep -q "^arm=" || { echo "FAIL — arm $arm rep $r produced no result (guest unreachable or run aborted)"; exit 1; }
        F=$(echo "$OUT" | sed -n 's/.*fsck_problems=\([0-9]*\).*/\1/p')
        [ "${F:-1}" = 0 ] || { echo "FAIL — fsck found ${F:-?} problems (arm $arm rep $r)"; rc=1; }
        [ $rc = 0 ] || [ "$FAIL_FAST" != 1 ] || { echo "FAIL_FAST — stopping after the first failing rep"; exit 1; }
    done
    r=$((r+1))
done

sum() {  # $1 arm, $2 field
    echo "$LOG" | grep "^arm=$1 " | sed -n "s/.* $2=\([0-9]*\).*/\1/p" | awk '{s+=$1} END{print s+0}'
}
echo "=== totals over $REPS rep(s) ==="
for arm in $ARMS; do
    echo "arm=$arm rounds=$(sum $arm rounds) meta_flushes=$(sum $arm meta_flushes) sync_flush_samples=$(sum $arm sync_flush_samples) tight_samples=$(sum $arm tight_samples) refusing_samples=$(sum $arm refusing_samples) bypass_tight=$(sum $arm bypass_tight) tight_honoured=$(sum $arm tight_honoured) kicks=$(sum $arm kicks) admit_refusals=$(sum $arm admit_refusals) enospc=$(sum $arm enospc) commits=$(sum $arm commits)"
done
for arm in $ARMS; do
    case $arm in
    1:*) [ "$(sum $arm bypass_tight)" -gt 0 ] || echo "WARNING — arm $arm: the bypass never fired, so this arm is not evidence about it";;
    0:*) [ "$(sum $arm tight_honoured)" -gt 0 ] || echo "WARNING — arm $arm: never honoured a quiet while tight, so this arm shows nothing about the bypass";;
    esac
done
exit $rc
