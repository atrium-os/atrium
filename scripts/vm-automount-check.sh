#!/bin/sh
# Auto-mount gate (docs/spec/device-assignment.md §4.6, §6.5): a USB stick
# plugged into the cloud amd64 VM (scripts/cloud-vm.sh, with a USB
# controller) is served under /media for the logged-in user only, with the
# shipped rule, pin and ruleset. Runs on the HOST with the Portcullis stack
# deployed:
#
#   ARCH=amd64 QEMU_EXTRA="-device qemu-xhci" sh scripts/cloud-vm.sh up
#   sh scripts/vm-automount-check.sh
#
# It cross-builds jaild, atrium-dispensatord and atrium-capsa-fuse, installs
# them with etc/atrium.devfs.rules, jaild pin 31 (as etc/jaild.policy.toml has
# it) and scripts/capsa-service.sh, makes users alice (1011) and bob (1012),
# and builds an MBR stick image with one FAT32 partition here. It plays the
# session by writing ostiarius's seat and lock files, and plugs the stick
# through the QEMU monitor (usb-storage). Checks:
#
#   - nobody logged in: the partition is not mounted;
#   - alice logs in: /media/daNs1 appears, alice's alone (0700), the file on
#     the stick reads back, alice can write, bob cannot even list it;
#   - the screen locks: unmounted, the server's jail gone; unlocked: back;
#   - Aqueduct media service (§6.6): alice lists and subscribes, bob is
#     refused; the subscriber hears added and ejected; lock and logout
#     end it.
#   - a stick with FAT straight on the disk is served whole (/media/daN).
#   - an exFAT stick takes alice's writes; fsck.exfat and Linux agree.
#   - eject: refused for bob, refused while alice has a file open ("busy"),
#     then done for alice; stays down across a relogin, back after replug;
#   - switch to bob: served as bob; logout: unmounted;
#   - after unplugging, fsck.fat finds the stick clean and holds alice's file.
#
# Ends "automount: N failed".
set -u
ROOT=$(cd "$(dirname "$0")/.." && pwd)
VMDIR=${VMDIR:-$HOME/atrium-vm-amd64}
TRIPLE=x86_64-unknown-freebsd
K="-i $HOME/.ssh/fresco_bsd_ed25519 -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o LogLevel=ERROR"
V() { "$ROOT/scripts/vssh" "$@"; }
mon() { python3 -I -c '
import socket,sys,time
s=socket.socket(socket.AF_UNIX); s.connect(sys.argv[1]); s.settimeout(0.5)
def drain():
    out=b""
    try:
        while True:
            b=s.recv(4096)
            if not b: break
            out+=b
    except socket.timeout: pass
    return out
drain()
for c in sys.argv[2:]:
    s.sendall(c.encode()+b"\n"); time.sleep(1)
    sys.stdout.write(drain().decode(errors="replace"))
' "$VMDIR/mon.sock" "$@"; }
fails=0
ok() { echo "  ok $1"; }
bad() { echo "  FAIL $1"; fails=$((fails+1)); }

V true >/dev/null 2>&1 || { echo "the VM does not answer over ssh (scripts/cloud-vm.sh up)"; echo "automount: 1 failed"; exit 1; }
V 'usbconfig list >/dev/null 2>&1' || { echo "the VM has no USB controller (QEMU_EXTRA=\"-device qemu-xhci\")"; echo "automount: 1 failed"; exit 1; }

echo "== cross-build"
CROSS_RUSTFLAGS=$(cd "$ROOT" && ARCH=amd64 sh -c '. ./scripts/cloud-vm.sh >/dev/null; printf %s "$CROSS_RUSTFLAGS"' sh env)
( cd "$ROOT/portcullis" && RUSTFLAGS="$CROSS_RUSTFLAGS" cargo build -q --release --target $TRIPLE -p jaild ) || { echo "jaild build failed"; exit 1; }
( cd "$ROOT/dispensator" && RUSTFLAGS="$CROSS_RUSTFLAGS" cargo build -q --release --target $TRIPLE --bins ) || { echo "dispensator build failed"; exit 1; }
( cd "$ROOT/capsa" && RUSTFLAGS="$CROSS_RUSTFLAGS" cargo build -q --release --target $TRIPLE -p capsa-fuse ) || { echo "capsa build failed"; exit 1; }

echo "== the stick (MBR, one FAT32 partition from 1 MiB)"
IMG=$VMDIR/stick.img
rm -f "$IMG"; truncate -s 96M "$IMG"
python3 -I - "$IMG" <<'PY'
import struct,sys
f=open(sys.argv[1],"r+b"); size=96*1024*1024//512
e=struct.pack("<B3sB3sII",0,b"\0\2\0",0x0C,b"\xfe\xff\xff",2048,size-2048)
f.seek(446); f.write(e); f.seek(510); f.write(b"\x55\xaa")
PY
mkfs.fat -F 32 --offset 2048 -n STICK "$IMG" $(( (96*1024*1024 - 1048576) / 1024 )) >/dev/null \
  && printf 'on the stick\n' > /tmp/automount-hello.txt && MTOOLS_SKIP_CHECK=1 mcopy -i "$IMG@@1M" /tmp/automount-hello.txt ::hello.txt \
  && echo "  stick image ready" || { echo "  could not build the stick image"; exit 1; }
# An exFAT stick (no partition table, as many come).
IMG3=$VMDIR/exfat.img
rm -f "$IMG3"; truncate -s 128M "$IMG3"; mkfs.exfat -L XSTICK "$IMG3" >/dev/null && echo "  exFAT stick image ready" || { echo "  could not build the exFAT image"; exit 1; }
# And one with FAT straight on the disk ("superfloppy").
IMG2=$VMDIR/bare.img
rm -f "$IMG2"; mkfs.fat -C -F 32 -n BARE "$IMG2" $((80*1024)) >/dev/null \
  && printf 'no table\n' > /tmp/automount-bare.txt && MTOOLS_SKIP_CHECK=1 mcopy -i "$IMG2" /tmp/automount-bare.txt ::bare.txt \
  && echo "  bare stick image ready" || { echo "  could not build the bare image"; exit 1; }

echo "== copy and install"
V 'rm -rf /root/am && mkdir -p /root/am/scripts /root/am/capsa/etc/devices.d'
scp -q $K -P 2222 "$ROOT/portcullis/target/$TRIPLE/release/atrium-jaild" \
  "$ROOT/dispensator/target/$TRIPLE/release/atrium-dispensatord" \
  "$ROOT/capsa/target/$TRIPLE/release/atrium-capsa-fuse" \
  "$ROOT/etc/atrium.devfs.rules" root@localhost:/root/am/
scp -q $K -P 2222 "$ROOT/scripts/capsa-service.sh" root@localhost:/root/am/scripts/
scp -q $K -P 2222 "$ROOT/capsa/etc/devices.d/50-capsa.toml" root@localhost:/root/am/capsa/etc/devices.d/
V 'sh -s' <<'EOF'
P=/etc/atrium/jaild.policy.toml
cd /root/am
service atrium-dispensatord stop >/dev/null 2>&1
service atrium-jaild stop >/dev/null 2>&1
install -m 0755 atrium-jaild /usr/local/bin/atrium-jaild
install -m 0755 atrium-dispensatord /usr/local/bin/atrium-dispensatord
install -m 0644 atrium.devfs.rules /usr/local/etc/atrium/devfs.rules
service devfs restart >/dev/null 2>&1
devfs rule -s 31 show | grep -q . && echo "  ok ruleset 31 (atrium_capsa) loaded" || echo "  FAIL ruleset 31 not loaded"
# The shipped pin, uid and profile (deploy-portcullis.sh writes its own policy).
[ -f $P.automount-bak ] || cp $P $P.automount-bak
cp $P.automount-bak $P
sed -i '' 's/^allowed_ids = \[\(.*\)\]/allowed_ids = [\1, 31]/' $P
sed -i '' 's/^allowed_system_uids = \[/allowed_system_uids = [ 65031,/' $P
sed -i '' 's/^allowed_system_gids = \[/allowed_system_gids = [ 65031,/' $P
cat >> $P <<'EOP'

[[devfs_rulesets.pinned]]
id                = 31
jail_name         = "atrium-capsa-"
name_prefix       = true
exec_path         = "/usr/local/bin/atrium-capsa-fuse"
uid               = 65031
fuse_devices      = ["da#p#", "da#s#", "da#"]
fuse_mount_prefix = "/media"
EOP
grep -q 'sysfil.profiles.fuse-server' $P || printf '\n[sysfil.profiles.fuse-server]\ninclude = ["base"]\nioctls  = ["0x40046480", "0x40086481"]\n' >> $P
for u in alice:1011 bob:1012; do id ${u%:*} >/dev/null 2>&1 || pw useradd ${u%:*} -u ${u#*:} -m -s /bin/sh; done
install -d -m 0755 /atrium/sockets/ostiarius
: > /atrium/sockets/ostiarius/active-session; : > /atrium/sockets/ostiarius/session-locked
service atrium-jaild start >/dev/null 2>&1; sleep 2
sh scripts/capsa-service.sh /root/am/atrium-capsa-fuse | sed 's/^/  /'
: > /var/log/atrium/dispensatord.log
service atrium-dispensatord restart >/dev/null 2>&1; sleep 3
pgrep -qf atrium-jaild && pgrep -qf atrium-dispensatord && echo "  ok jaild and dispensatord running" || echo "  FAIL a daemon is not running"
EOF

SEAT=/atrium/sockets/ostiarius/active-session
LOCK=/atrium/sockets/ostiarius/session-locked
DD=/usr/local/bin/atrium-dispensatord
RUN=$$
login() { V "printf '%s\n' '$1' > $SEAT"; sleep 6; }
lock() { V "printf '%s\n' '$1' > $LOCK"; sleep 6; }
mounted() { V "mount -p | tr -s '\t' ' ' | grep -q ' $M fusefs'"; }
plug() {
  before=$(V "sysctl -n kern.disks")
  mon "drive_add 0 if=none,id=stick$1,file=${2:-$IMG},format=raw" "device_add usb-storage,id=st$1,drive=stick$1" | tr -d '\r' | grep -iE "error|fail" | sed 's/^/  qemu: /'
  sleep 10
  DA=$(V "sysctl -n kern.disks" | tr ' ' '\n' | grep '^da' | while read d; do echo " $before " | grep -q " $d " || echo $d; done | head -1)
  PART=${DA}s1; M=/media/$PART
}
unplug() { mon "device_del st$1" >/dev/null; sleep 8; }

echo "== plugged in at the login screen"
plug 1$RUN
[ -n "$DA" ] && V "test -e /dev/$PART" && ok "the stick is $DA, partition $PART" || { bad "no new da disk with s1 ($(V 'sysctl -n kern.disks; ls /dev | grep ^da | tr "\n" " "'))"; echo "automount: $fails failed"; exit $fails; }
mounted && bad "$M mounted with nobody logged in" || ok "nobody logged in: not mounted"

echo "== alice logs in"
login alice
if mounted; then
  ok "$M mounted for alice"
  [ "$(V "stat -f '%Su %Lp' $M")" = "alice 700" ] && ok "alice's alone (0700)" || bad "root dir is '$(V "stat -f '%Su %Lp' $M")'"
  [ "$(V "su -m alice -c 'cat $M/hello.txt'")" = "on the stick" ] && ok "alice reads the file on the stick" || bad "alice cannot read hello.txt"
  V "su -m alice -c 'echo from alice > $M/alice.txt'" && ok "alice writes" || bad "alice cannot write"
  V "su -m bob -c 'ls $M' >/dev/null 2>&1" && bad "bob can list alice's stick" || ok "bob cannot list it"
  V "jls -n name | grep -q 'name=atrium-capsa-$PART'" && ok "served from jail atrium-capsa-$PART" || bad "no jail atrium-capsa-$PART"
  sleep 3
  out=$(V "su -m alice -c '$DD --media'" | tr '\n' ';')
  echo "$out" | grep -q "^ok;$PART $M " && ok "alice lists it over Aqueduct ($out)" || bad "alice's list: '$out'"
  out=$(V "su -m bob -c '$DD --media' 2>&1")
  echo "$out" | grep -q "status 2" && ok "bob may not list it ($out)" || bad "bob's list: '$out'"
  # What Forum's bar hears: a subscription, as alice.
  V "rm -f /tmp/mw.$RUN; daemon -f -o /tmp/mw.$RUN su -m alice -c '$DD --media-watch'"; sleep 2
  V "grep -qx 'added $PART $M' /tmp/mw.$RUN" && ok "the subscriber hears it added" || bad "subscriber: '$(V "tr '\n' ';' < /tmp/mw.$RUN")'"
else
  bad "not mounted for alice ($(V "tail -4 /var/log/atrium/dispensatord.log | tr '\n' ';'"))"
fi

echo "== lock and unlock"
lock alice
mounted && bad "still mounted while locked" || ok "locked: unmounted"
V "jls -n name | grep -q 'name=atrium-capsa-$PART'" && bad "jail still up while locked" || ok "and the server's jail is gone"
V "tail -1 /tmp/mw.$RUN | grep -qx end" && ok "locking ends the subscription" || bad "subscriber while locked: '$(V "tr '\n' ';' < /tmp/mw.$RUN")'"
lock ""
mounted && [ "$(V "su -m alice -c 'cat $M/alice.txt'")" = "from alice" ] && ok "unlocked: back, with alice's file" || bad "not back after unlock"
# Subscribe again, as the bar does.
V "rm -f /tmp/mw.$RUN; daemon -f -o /tmp/mw.$RUN su -m alice -c '$DD --media-watch'"; sleep 2

echo "== eject"
out=$(V "su -m bob -c '$DD --eject $PART' 2>&1")
echo "$out" | grep -q "^error only the session's user may eject (status 2)" && ok "bob may not eject ($out)" || bad "bob's eject: '$out'"
V "daemon -f su -m alice -c 'sleep 20 < $M/alice.txt'"; sleep 2
out=$(V "su -m alice -c '$DD --eject $PART' 2>&1")
echo "$out" | grep -q "busy.*(status 4)" && mounted && ok "refused while a file is open ($out)" || bad "eject with a file open: '$out'"
V "pkill -u alice sleep"; sleep 1
out=$(V "su -m alice -c '$DD --eject $PART' 2>&1")
echo "$out" | grep -q "^ok" && ok "alice ejects ($out)" || bad "alice's eject: '$out'"
sleep 2
mounted && bad "still mounted after eject" || ok "unmounted"
V "grep -qx 'removed $PART ejected' /tmp/mw.$RUN" && ok "the subscriber hears it ejected" || bad "subscriber: '$(V "tr '\n' ';' < /tmp/mw.$RUN")'"
login ""
V "tail -1 /tmp/mw.$RUN | grep -qx end" && ok "logout ends the subscription" || bad "subscriber after logout: '$(V "tr '\n' ';' < /tmp/mw.$RUN")'"
login alice
mounted && bad "an ejected stick came back on relogin" || ok "stays ejected across a relogin"
unplug 1$RUN
plug 2$RUN
login alice
mounted && ok "replugged: served again ($M)" || bad "not served after replug"

echo "== switch user, log out"
login bob
mounted && [ "$(V "stat -f '%Su' $M")" = "bob" ] && ok "switched to bob: served as bob" || bad "not served as bob ($(V "stat -f '%Su' $M 2>&1"))"
login ""
mounted && bad "still mounted after logout" || ok "logged out: unmounted"
V "$DD --media" | sed 's/^/  media: /'
unplug 2$RUN

echo "== a stick with no partition table"
login alice
plug 3$RUN "$IMG2"
PART=$DA; M=/media/$DA
if [ -n "$DA" ] && mounted; then
  ok "served whole at $M"
  [ "$(V "su -m alice -c 'cat $M/bare.txt'")" = "no table" ] && ok "alice reads it" || bad "alice cannot read bare.txt"
  V "ls /dev | grep -q '^${DA}[ps]'" && bad "GEOM made partitions on it" || ok "no partition nodes"
else
  bad "not served whole ($(V "tail -3 /var/log/atrium/dispensatord.log | tr '\n' ';'"))"
fi
unplug 3$RUN
mounted && bad "still mounted after unplug" || ok "unplugged: unmounted"

echo "== an exFAT stick, written"
plug 4$RUN "$IMG3"
PART=$DA; M=/media/$DA
if [ -n "$DA" ] && mounted; then
  ok "exFAT served at $M"
  V "su -m alice -c 'mkdir -p \"$M/Photos 2026\" && dd if=/dev/random of=\"$M/Photos 2026/big.bin\" bs=1m count=20 2>/dev/null && echo exfat from alice > $M/Ünïcödé.txt && sha256 -q \"$M/Photos 2026/big.bin\" > /tmp/exfat-sum.$RUN'" && ok "alice writes 20 MB and a Unicode name" || bad "alice cannot write on exFAT"
  out=$(V "su -m alice -c '$DD --eject $PART' 2>&1")
  echo "$out" | grep -q "^ok" && ok "ejected ($out)" || bad "eject: '$out'"
  SUM=$(V "cat /tmp/exfat-sum.$RUN")
else
  bad "exFAT not served ($(V "tail -3 /var/log/atrium/dispensatord.log | tr '\n' ';'"))"
fi
unplug 4$RUN
if fsck.exfat -n "$IMG3" >/tmp/automount-fsck-x 2>&1; then ok "fsck.exfat finds it clean"; else bad "fsck.exfat: $(tr '\n' ';' < /tmp/automount-fsck-x)"; fi
L=$(losetup -f --show "$IMG3" 2>/dev/null)
if [ -n "$L" ] && mkdir -p /tmp/automount-x && mount.exfat-fuse -o ro "$L" /tmp/automount-x 2>/dev/null; then
  [ "$(cat /tmp/automount-x/Ünïcödé.txt)" = "exfat from alice" ] && ok "Linux reads alice's Unicode file" || bad "Ünïcödé.txt: '$(cat /tmp/automount-x/Ünïcödé.txt 2>&1)'"
  [ "$(sha256sum "/tmp/automount-x/Photos 2026/big.bin" | cut -d' ' -f1)" = "$SUM" ] && ok "and the 20 MB file, byte for byte" || bad "big.bin differs"
  umount /tmp/automount-x
else
  echo "  (no exfat-fuse here: cross-read skipped)"
fi
[ -n "$L" ] && losetup -d "$L"
login ""
V "grep -E 'capsa|session|eject' /var/log/atrium/dispensatord.log | tail -8" | sed 's/^/  log: /'

echo "== the stick, checked here"
L=$(losetup -f --show -o 1048576 "$IMG" 2>/dev/null)
if [ -n "$L" ]; then
  fsck.fat -n "$L" >/tmp/automount-fsck 2>&1 && ok "fsck.fat finds it clean" || bad "fsck.fat: $(tr '\n' ';' < /tmp/automount-fsck)"
  losetup -d "$L"
else
  echo "  (no loop device here: fsck skipped)"
fi
[ "$(MTOOLS_SKIP_CHECK=1 mtype -i "$IMG@@1M" ::alice.txt 2>/dev/null)" = "from alice" ] && ok "alice's file is on the stick" || bad "alice.txt not on the stick"

V 'sh -s' <<'EOF'
P=/etc/atrium/jaild.policy.toml
[ -f $P.automount-bak ] && mv $P.automount-bak $P
: > /atrium/sockets/ostiarius/active-session
service atrium-jaild restart >/dev/null 2>&1
EOF
echo "automount: $fails failed"
exit $fails
