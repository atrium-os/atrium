#!/bin/sh
# #110: is the buildkernel-on-Tessera time swing the GUEST doing more work, or
# the guest STALLING on something outside itself?
#
# The discriminator is wall vs CPU. Three identical runs, same volume, same
# source, nothing else changed:
#   real swings, user+sys flat  -> the guest is waiting (host contention / IO)
#   real and user+sys swing together -> the guest is genuinely doing more work
#
# Cheaper than the 3-runs-per-arm A/B in the memory note, and it answers the
# specific open question rather than re-measuring the spread.
#
# Gates: every run must exit 0, and regov_inserted must move (df lies on
# Tessera; the registry insert count is the honest workload proxy).
set -u
sysc() { sysctl -n "kern.tessera.$1" 2>/dev/null || echo 0; }
umount /mnt/obj 2>/dev/null; mdconfig -d -u 9 2>/dev/null
rm -f /usr/obj/tess-swing.img
truncate -s 6g /usr/obj/tess-swing.img || exit 2
mdconfig -a -t vnode -f /usr/obj/tess-swing.img -u 9 || exit 2

printf '%-4s %-9s %-9s %-9s %-9s %-10s %s\n' RUN REAL_S USER_S SYS_S CPU_S REGOV_INS EXIT
n=1
while [ $n -le 3 ]; do
    umount /mnt/obj 2>/dev/null
    /root/mkfs-tessera /dev/md9 >/dev/null 2>&1 || exit 2
    mkdir -p /mnt/obj; mount -t tessera /dev/md9 /mnt/obj || exit 2
    r0=$(sysc regov_inserted)
    cd /usr/src
    # ★ The redirections must sit on MAKE, not on the whole command, or
    # /usr/bin/time's own stderr lands in the build log instead of the timing
    # file. The first run of this script lost every timing number that way —
    # recoverable only because buildkernel echoes "real/user/sys" into its log
    # at the end anyway.
    /usr/bin/time -p sh -c "env MAKEOBJDIRPREFIX=/mnt/obj make -j4 buildkernel \
        KERNCONF=GENERIC > /tmp/sw$n.log 2>&1" 2>/tmp/sw$n.time
    rc=$?
    real=$(awk '/^real/{print $2}' /tmp/sw$n.time)
    user=$(awk '/^user/{print $2}' /tmp/sw$n.time)
    sys=$(awk '/^sys/{print $2}'  /tmp/sw$n.time)
    r1=$(sysc regov_inserted)
    cpu=$(echo "$user + $sys" | bc 2>/dev/null)
    printf '%-4s %-9s %-9s %-9s %-9s %-10s %s\n' "$n" "$real" "$user" "$sys" "$cpu" "$((r1-r0))" "${rc:-?}"
    n=$((n+1))
done
umount /mnt/obj 2>/dev/null
mdconfig -d -u 9; rm -f /usr/obj/tess-swing.img
