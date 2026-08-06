#!/bin/sh
# The decisive arm for #110. The microbenchmark is stable at idle (1.42-1.44s
# user, 1.4%). Now sample it WHILE the guest runs the same buildkernel whose
# time swings.
#
#   user stays ~1.43  -> instruction rate is constant even under load. The
#                        guest is simply descheduled (real inflates, CPU does
#                        not), so buildkernel's user+sys variation is genuine
#                        extra work, NOT the host slowing execution.
#   user rises        -> the same instruction stream costs more CPU seconds
#                        under load. Host/VM contention inflates guest CPU
#                        time, and every timing number in this VM inherits it.
set -u
umount /mnt/obj 2>/dev/null; mdconfig -d -u 9 2>/dev/null
rm -f /usr/obj/tess-fw.img
truncate -s 6g /usr/obj/tess-fw.img || exit 2
mdconfig -a -t vnode -f /usr/obj/tess-fw.img -u 9 || exit 2
/root/mkfs-tessera /dev/md9 >/dev/null 2>&1 || exit 2
mkdir -p /mnt/obj; mount -t tessera /dev/md9 /mnt/obj || exit 2

echo "  baseline (idle, from the previous run): user ~1.43 s"
cd /usr/src
env MAKEOBJDIRPREFIX=/mnt/obj make -j4 buildkernel KERNCONF=GENERIC \
    > /tmp/fwload.log 2>&1 &
BP=$!
sleep 45                       # let it reach steady compilation
echo "SAMPLE  USER_S   REAL_S   BUILD_ALIVE"
n=1
while [ $n -le 8 ]; do
    kill -0 $BP 2>/dev/null && alive=yes || alive=NO
    [ "$alive" = "NO" ] && { echo "  build exited early — remaining samples are idle-guest, not loaded"; break; }
    /usr/bin/time -p cpuset -l 0 /root/fixedwork 2>/tmp/fw.t >/dev/null
    printf "%-7s %-8s %-8s %s\n" "$n" \
      "$(awk '/^user/{print $2}' /tmp/fw.t)" \
      "$(awk '/^real/{print $2}' /tmp/fw.t)" "$alive"
    sleep 45
    n=$((n+1))
done
kill -9 $BP 2>/dev/null; pkill -9 make 2>/dev/null; pkill -9 cc 2>/dev/null
sleep 2; umount /mnt/obj 2>/dev/null
mdconfig -d -u 9; rm -f /usr/obj/tess-fw.img
