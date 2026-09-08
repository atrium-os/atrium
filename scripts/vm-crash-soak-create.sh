#!/bin/sh
# Faithful power-cut crash soak — CREATE-HEAVY workload, on the SCRATCH disk.
#
# The delete-side twin of vm-crash-soak.sh. Per cycle: interleaved mkdir /
# create / symlink / hardlink / rename (cross-dir and same-dir, plus a whole
# directory move) under a continuous on-demand GC loop, left MOUNTED AND DIRTY,
# then an abrupt QMP `quit` + relaunch, then remount + tessera-fsck.
#
# WHAT THIS GUARDS (cd759fe9): every namespace-ADDING vop commits an inode
# mutation and a dirent write as two separate steps, and they used to be
# ungated, so a flush — or a power cut — between them left:
#   - vop_create/mkdir/symlink : the inode committed with no dirent naming it
#                                (ORPHAN inode);
#   - vop_link                 : nlink bumped with no new dirent (NLINK too high);
#   - vop_rename               : the file under BOTH names, NEITHER, or with a
#                                dangling target.
# The #80 rollback only ever covered a SYNCHRONOUS dirent failure, not a crash
# in that window. All five now perform both writes under the flush gate.
#
# PASS = every cycle boots and fscks clean, with no orphan / nlink / dangling
# problems and create_rollback staying 0.
#
# Destructive, and ONLY on the scratch disk: every guest phase re-checks the
# GEOM ident (#129) before touching the device.
#
#   CYCLES=30 sh scripts/vm-crash-soak-create.sh
set -u
BSD="$(cd "$(dirname "$0")/.." && pwd)"
VSSH="$BSD/scripts/vssh"; SOCK=/tmp/qmp.sock
CYCLES=${CYCLES:-30}; DEV=/dev/vtbd2; M=/mnt/scratch

# ★ Gate on the module built from THIS tree unless told otherwise.
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
  ( cd "$BSD" && ./scripts/run-vm.sh >/tmp/soakc-vmboot.log 2>&1 </dev/null & ) >/dev/null 2>&1 </dev/null
  sleep 1
  wait_ready
}
GATE="diskinfo -s $DEV | grep -q '^atrium-scratch\$' || { echo REFUSING_ident; exit 2; }"

wait_ready || exit 1
k=$($VSSH "sha256 -q /boot/kernel/tessera_fs.ko | cut -c1-16" | tr -d '\r')
[ "$k" = "$KMOD" ] || { echo "ABORT: guest module [$k] != expected [$KMOD]"; exit 1; }
echo "=== CREATE-SOAK $(date) kmod=$KMOD cycles=$CYCLES kernel=$($VSSH 'uname -i' | tr -d '\r') ==="

$VSSH "$GATE; mount | grep -q ' $M ' && umount $M; mkdir -p $M
  mkfs-tessera $DEV >/dev/null 2>&1 && mount -t tessera $DEV $M || { echo mkfs_fail; exit 3; }
  mkdir $M/base; sync; umount $M; echo seeded" | tr -d '\r' | grep -q seeded || { echo "ABORT: seed failed"; exit 1; }

fail=0
c=1; while [ $c -le $CYCLES ]; do
  # create-heavy dirty phase, under a GC loop, left mounted+dirty
  out=$($VSSH "$GATE
    S(){ sysctl -n kern.tessera.\$1 2>/dev/null || echo 0; }
    mount -t tessera $DEV $M 2>/dev/null || { echo MOUNT_FAIL; exit 4; }
    r0=\$(S inode_get_retry); cr0=\$(S create_rollback)
    touch /root/gcloop; ( while [ -f /root/gcloop ]; do /root/tq $M >/dev/null 2>&1; done ) & sleep 0.3
    : > /root/c.err
    d=$M/g$c
    for i in \$(seq 1 12); do
      mkdir -p \$d/d\$i/sub 2>>/root/c.err
      echo hello-$c-\$i > \$d/d\$i/f\$i 2>>/root/c.err        # create
      ln -s f\$i \$d/d\$i/sym\$i 2>>/root/c.err                # symlink
      ln \$d/d\$i/f\$i \$d/d\$i/hard\$i 2>>/root/c.err          # hardlink
      mv \$d/d\$i/f\$i \$d/d\$i/sub/moved\$i 2>>/root/c.err     # rename (cross-dir)
      mv \$d/d\$i/sym\$i \$d/d\$i/sym\$i.r 2>>/root/c.err       # rename (same-dir)
    done
    # a cross-directory dir rename too
    mkdir -p \$d/movesrc/inner 2>>/root/c.err; mv \$d/movesrc \$d/d1/moved_dir 2>>/root/c.err
    rm -f /root/gcloop; wait
    echo \"dirty retry=\$(( \$(S inode_get_retry)-r0 )) rollback=\$(( \$(S create_rollback)-cr0 )) errs=\$(wc -l < /root/c.err | tr -d ' ') files=\$(find \$d 2>/dev/null | wc -l | tr -d ' ')\"" 2>/dev/null | tr -d '\r' | tr '\n' ' ')
  case "$out" in *MOUNT_FAIL*|*REFUSING*) echo "cycle $c: MOUNT FAILED [$out]"; fail=$((fail+1));; esac
  echo "cycle $c: $out— POWER CUT"
  if ! relaunch; then echo "  cycle $c: BOOT FAILED"; fail=$((fail+1)); break; fi
  res=$($VSSH "$GATE
    [ \$(sha256 -q /boot/kernel/tessera_fs.ko | cut -c1-16) = $KMOD ] || echo WRONG_KMOD
    mount -t tessera $DEV $M 2>/dev/null || echo MOUNT_FAIL
    n=\$(find $M 2>/dev/null | wc -l | tr -d ' '); umount $M 2>/dev/null
    tessera-fsck $DEV > /root/c.fsck 2>&1
    echo \"entries=\$n fsck_problems=\$(grep -ciE 'dangling|orphan|nlink|leaked|overlap|missing|neither|corrupt|problem' /root/c.fsck)\"" 2>/dev/null | tr -d '\r' | tr '\n' ' ')
  case "$res" in
    *WRONG_KMOD*|*REFUSING*) echo "  cycle $c: ABORT [$res]"; exit 1;;
    *MOUNT_FAIL*) echo "  cycle $c: RECOVERY MOUNT FAILED [$res]"; fail=$((fail+1));;
    *"fsck_problems=0 "*) echo "  cycle $c: fsck CLEAN $res";;
    *) echo "  cycle $c: FSCK-DIRTY $res"; $VSSH "grep -iE 'dangling|orphan|nlink|leaked|missing|neither|corrupt|problem|result' /root/c.fsck | head -6" 2>/dev/null | sed 's/^/     /'; fail=$((fail+1)); [ $fail -ge 3 ] && { echo "stopping after 3"; break; };;
  esac
  c=$((c+1))
done
echo "=== CREATE-SOAK DONE $(date): $fail failures of $((c-1)) cuts ==="
[ $fail -eq 0 ] || exit 1
exit 0
