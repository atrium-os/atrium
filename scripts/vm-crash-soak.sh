#!/bin/sh
# Faithful power-cut crash soak — DELETE-HEAVY workload, on the SCRATCH disk.
#
# Runs from the macOS HOST. Per cycle: mount vtbd2 (a REPLAY mount from cycle 2
# on), run a dirty workload — reflink copies + appends, `cp -R` of a source tree
# and `rm -rf` of the previous one — under a continuous on-demand GC loop so the
# epoch sweep, the publish mark and the pinscan swap all fire, then leave the
# volume MOUNTED AND DIRTY and cut power with an abrupt QMP `quit` + relaunch.
# The devroot is cut too, so "did it boot" is part of every cycle. Then remount,
# unmount and run tessera-fsck as the oracle.
#
# WHAT THIS GUARDS (all found by this harness):
#   - unlink crash-atomicity (43b4e26d): vop_remove/vop_rmdir's DIR_REMOVE and
#     inode removal straddled the flush's checkpoint->drain boundary, so a cut
#     between the two commits left a DANGLING DIRENT (deleted name -> vanished
#     inode); the mirror ordering left an orphan.
#   - the reader epoch / publish mark (2a7c08b7): freed metadata recycled under
#     an in-flight descent. `retry` and `abandoned` staying 0 is the signal.
#   - journal ring replay (b2417692): mkfs formats the ring BODY now, so stale
#     records from a previous epoch cannot re-materialise deleted dirents. (An
#     earlier version of this script wiped the ring by hand to prove that; it
#     is no longer needed.)
#
# PASS = every cycle boots, replay-mounts, and fscks clean.
#
# Destructive, and ONLY on the scratch disk: every guest phase re-checks the
# GEOM ident (#129) before touching the device.
#
#   CYCLES=50 PER=8 sh scripts/vm-crash-soak.sh
set -u
BSD="$(cd "$(dirname "$0")/.." && pwd)"
VSSH="$BSD/scripts/vssh"; SOCK=/tmp/qmp.sock
CYCLES=${CYCLES:-30}; PER=${PER:-8}
DEV=/dev/vtbd2; M=/mnt/scratch; TREE=/usr/include

# ★ Gate on the module built from THIS tree unless told otherwise — a soak that
# silently measures a stale guest module proves nothing (see the "verify WHICH
# kernel you measured" lesson).
if [ -z "${KMOD:-}" ]; then
    KO="$BSD/atrium-tessera/kmod/tessera_fs.ko"
    [ -f "$KO" ] || { echo "no $KO — build it first, or pass KMOD=<hash>"; exit 1; }
    KMOD=$(shasum -a 256 "$KO" | cut -c1-16)
fi

wait_ready(){ i=0; while [ $i -lt 150 ]; do $VSSH 'echo ready' >/dev/null 2>&1 && return 0; i=$((i+1)); sleep 3; done; echo "FATAL: VM never ready"; return 1; }
relaunch(){
  echo quit | nc -U -w2 $SOCK >/dev/null 2>&1
  i=0; while [ $i -lt 15 ]; do pgrep -f qemu-system-aarch64 >/dev/null || break; i=$((i+1)); sleep 2; done
  pgrep -f qemu-system-aarch64 >/dev/null && pkill -9 -f qemu-system-aarch64; sleep 2
  # ★ Redirect the SUBSHELL's own fds, not just run-vm.sh's. `( cmd & )`
  # forks this script, and that fork inherits OUR stdout — which, when a
  # suite driver pipes us, is the pipe. The fork then lives as long as
  # QEMU and holds the write end open, so `tee` never sees EOF and the
  # driver hangs AFTER we have already exited. (That is what stalled the
  # suite between phases; `exit 0` alone could not fix it, because the
  # process holding the pipe was never this one.) setsid would also do
  # it, but macOS has no setsid.
  ( cd "$BSD" && ./scripts/run-vm.sh >/tmp/soak-vmboot.log 2>&1 </dev/null & ) >/dev/null 2>&1 </dev/null
  sleep 1
  wait_ready
}
GATE="diskinfo -s $DEV | grep -q '^atrium-scratch\$' || { echo REFUSING_ident; exit 2; }"

wait_ready || exit 1
k=$($VSSH 'sha256 -q /boot/kernel/tessera_fs.ko | cut -c1-16' | tr -d '\r')
[ "$k" = "$KMOD" ] || { echo "ABORT: guest module [$k] != expected [$KMOD]"; exit 1; }
echo "=== SOAK $(date) kmod=$KMOD cycles=$CYCLES per=$PER kernel=$($VSSH 'uname -i; sysctl -n kern.sched.name' | tr '\n' '/') ==="

$VSSH "$GATE; mount | grep -q ' $M ' && umount $M; mkdir -p $M
  mkfs-tessera $DEV >/dev/null 2>&1 && mount -t tessera $DEV $M || { echo mkfs_fail; exit 3; }
  echo seed > $M/seed; dd if=/dev/random of=$M/big bs=4096 count=64 2>/dev/null; mkdir $M/d; sync; umount $M; echo seeded" | tr -d '\r' | grep -q seeded || { echo "ABORT: seed failed"; exit 1; }

fail=0; bootfail=0; recfail=0
c=1; while [ $c -le $CYCLES ]; do
  # ---- dirty phase: replay-mount, churn under GC passes, leave mounted+dirty
  out=$($VSSH "$GATE
    S(){ sysctl -n kern.tessera.\$1 2>/dev/null || echo 0; }
    mount -t tessera $DEV $M 2>/dev/null || { echo MOUNT_FAIL; exit 4; }
    ls $M/big >/dev/null 2>&1 || echo BIG_MISSING
    r0=\$(S inode_get_retry); s0=\$(S gc_scans); e0=\$(S inode_get_retry_enoent); d0=\$(S meta_rd_deferred); a0=\$(S meta_rd_drain_abandoned)
    touch /root/gcloop; ( while [ -f /root/gcloop ]; do /root/tq $M >/dev/null 2>&1; done ) & sleep 0.3
    : > /root/soak.err
    j=1; while [ \$j -le $PER ]; do
      cp $M/big $M/rl_${c}_\$j 2>>/root/soak.err
      echo m-${c}-\$j >> $M/big
      j=\$((j+1))
    done
    cp -R $TREE $M/t_$c 2>>/root/soak.err
    rm -rf $M/t_$((c-1)) 2>>/root/soak.err
    rm -f $M/rl_$((c-2))_* 2>>/root/soak.err
    rm -f /root/gcloop; wait
    echo \"dirty scans=\$(( \$(S gc_scans)-s0 )) retry=\$(( \$(S inode_get_retry)-r0 )) enoent=\$(( \$(S inode_get_retry_enoent)-e0 )) deferred=\$(( \$(S meta_rd_deferred)-d0 )) abandoned=\$(( \$(S meta_rd_drain_abandoned)-a0 )) errs=\$(wc -l < /root/soak.err | tr -d ' ') files=\$(ls $M | wc -l | tr -d ' ')\"" 2>/dev/null | tr -d '\r' | tr '\n' ' ')
  case "$out" in *MOUNT_FAIL*|*REFUSING*) echo "cycle $c: REPLAY-MOUNT FAILED [$out]"; recfail=$((recfail+1)); fail=$((fail+1));; esac
  echo "cycle $c: $out— POWER CUT (mounted+dirty)"
  # ---- the cut
  if ! relaunch; then echo "  cycle $c: BOOT FAILED after cut"; bootfail=$((bootfail+1)); fail=$((fail+1)); break; fi
  # ---- recovery + oracle
  res=$($VSSH "$GATE
    [ \$(sha256 -q /boot/kernel/tessera_fs.ko | cut -c1-16) = $KMOD ] || echo WRONG_KMOD
    mount -t tessera $DEV $M 2>/dev/null || echo MOUNT_FAIL
    n=\$(ls $M 2>/dev/null | wc -l | tr -d ' '); umount $M 2>/dev/null
    tessera-fsck $DEV > /root/soak.fsck 2>&1
    echo \"recovered_entries=\$n fsck_problem_lines=\$(grep -ciE 'dangling|orphan|leaked|overlap|missing|neither|corrupt|problem' /root/soak.fsck) root_commit_failed=\$(sysctl -n kern.tessera.commit_failed)\"" 2>/dev/null | tr -d '\r' | tr '\n' ' ')
  case "$res" in
    *WRONG_KMOD*|*REFUSING*) echo "  cycle $c: ABORT [$res]"; exit 1;;
  esac
  case "$res" in
    *MOUNT_FAIL*) echo "  cycle $c: RECOVERY MOUNT FAILED [$res]"; recfail=$((recfail+1)); fail=$((fail+1));;
    *"fsck_problem_lines=0 "*) echo "  cycle $c: fsck CLEAN $res";;
    *) echo "  cycle $c: FSCK-DIRTY $res"; $VSSH "grep -iE 'dangling|orphan|leaked|overlap|missing|neither|corrupt|problem|result' /root/soak.fsck | head -5" 2>/dev/null | sed 's/^/     /'; fail=$((fail+1)); [ $fail -ge 3 ] && { echo "stopping after 3 failures"; break; };;
  esac
  c=$((c+1))
done
echo "=== SOAK DONE $(date): $fail failures of $((c-1)) cuts (boot=$bootfail recovery=$recfail fsck-dirty=$((fail-bootfail-recfail))) ==="
[ $fail -eq 0 ] || exit 1
exit 0
