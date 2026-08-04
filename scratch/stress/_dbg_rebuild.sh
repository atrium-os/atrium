#!/bin/sh
set -e
test -e /mnt/host/atrium-tessera || { kldload p9fs 2>/dev/null; mount -t p9fs -o trans=virtio bsd_share /mnt/host; }
# add debug define to the tessera CFLAGS (once)
python3 - <<'PY'
p="/usr/src/stand/libsa/Makefile"
s=open(p).read()
if "TESSERA_LOADER_DEBUG" not in s:
    s=s.replace("-DTESSERA_STAND","-DTESSERA_STAND -DTESSERA_LOADER_DEBUG",1)
    open(p,"w").write(s); print("debug flag added")
else: print("debug flag present")
PY
cd /usr/src/stand && make -j4 >/tmp/ldrbuild.log 2>&1 && echo "build OK" || { echo "BUILD FAIL"; grep -iE "error" /tmp/ldrbuild.log | tail; exit 1; }
NEW=$(find /usr/obj -name loader_lua.efi | head -1)
mkdir -p /mnt/esp
mount -t msdosfs /dev/vtbd3p1 /mnt/esp
cp "$NEW" /mnt/esp/EFI/BOOT/BOOTAA64.EFI
sync; umount /mnt/esp
echo "reinstalled debug loader ($(ls -l $NEW | awk '{print $5}') bytes)"
