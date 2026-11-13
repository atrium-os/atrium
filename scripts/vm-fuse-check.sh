#!/bin/sh
# Removable storage D5b gate (docs/spec/device-assignment.md §4.6, §6.4): a
# FAT volume is served by the jailed Rust FUSE server atrium-capsa-fuse, with
# jaild opening the partition and /dev/fuse and mounting fusefs from outside
# the jail. Runs on the HOST of the cloud amd64 VM (scripts/cloud-vm.sh) with
# the Portcullis stack deployed (scripts/deploy-portcullis.sh):
#
#   sh scripts/vm-fuse-check.sh
#
# It cross-builds jaild, the jclient smoke CLI and atrium-capsa-fuse, installs
# them, and for the test only: a 80 MiB FAT32 volume on md(4) unit 99 (a
# stand-in for a USB stick's daNpM), devfs ruleset 30 (basics only) and a
# jaild pin for it (atrium-capsa-*, uid/gid 65070, fuse_devices md#, mounts
# under /media). Then it checks:
#
#   - jaild mounts fusefs on /media/capsa-md99 and the server answers: a file
#     written beforehand reads back; create, append, mkdir, rename, unlink,
#     rmdir and a 2 MiB file all work through the mount, owned by the session
#     uid it was given;
#   - the server's jail sees neither md99 nor fuse in its /dev, and the
#     jailed server process holds exactly those two as fd 3 and fd 4;
#   - killing the server and RemoveJail leaves no mount; fsck_msdosfs finds
#     the volume clean and a kernel msdosfs mount (host-side check only) reads
#     what was written;
#   - read-only: writes fail with EROFS;
#   - jaild refuses a device the pin does not name (the VM's own disk) and a
#     mountpoint outside /media;
#   - dispensatord: a `disk` rule (test file 90-fusetest.toml, md partitions)
#     serves a GPT partition on a new md(4) at /media/md98p1 as soon as GEOM
#     creates it, and destroying the md takes the server and the mount away.
#
# Ends "fuse: N failed".
set -u
ROOT=$(cd "$(dirname "$0")/.." && pwd)
TRIPLE=x86_64-unknown-freebsd
K="-i $HOME/.ssh/fresco_bsd_ed25519 -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o LogLevel=ERROR"
V() { "$ROOT/scripts/vssh" "$@"; }
fails=0
ok() { echo "  ok $1"; }
bad() { echo "  FAIL $1"; fails=$((fails+1)); }

V true >/dev/null 2>&1 || { echo "the VM does not answer over ssh (scripts/cloud-vm.sh up)"; echo "fuse: 1 failed"; exit 1; }

echo "== cross-build"
CROSS_RUSTFLAGS=$(cd "$ROOT" && ARCH=amd64 sh -c '. ./scripts/cloud-vm.sh >/dev/null; printf %s "$CROSS_RUSTFLAGS"' sh env)
( cd "$ROOT/portcullis" && RUSTFLAGS="$CROSS_RUSTFLAGS" cargo build -q --release --target $TRIPLE -p jaild -p portcullisd --bin atrium-jaild --bin atrium-portcullisd-jclient ) || { echo "portcullis build failed"; exit 1; }
( cd "$ROOT/dispensator" && RUSTFLAGS="$CROSS_RUSTFLAGS" cargo build -q --release --target $TRIPLE --bins ) || { echo "dispensator build failed"; exit 1; }
( cd "$ROOT/capsa" && RUSTFLAGS="$CROSS_RUSTFLAGS" cargo build -q --release --target $TRIPLE -p capsa-fuse ) || { echo "capsa build failed"; exit 1; }

echo "== copy"
V 'rm -rf /root/fz && mkdir -p /root/fz'
scp -q $K -P 2222 "$ROOT/portcullis/target/$TRIPLE/release/atrium-jaild" \
  "$ROOT/portcullis/target/$TRIPLE/release/atrium-portcullisd-jclient" \
  "$ROOT/capsa/target/$TRIPLE/release/atrium-capsa-fuse" \
  "$ROOT/dispensator/target/$TRIPLE/release/atrium-dispensatord" root@localhost:/root/fz/

echo "== install (md99 volume, test ruleset 30, pin)"
V 'sh -s' <<'EOF'
P=/etc/atrium/jaild.policy.toml
cd /root/fz
service atrium-jaild stop >/dev/null 2>&1
install -m 0755 atrium-jaild /usr/local/bin/atrium-jaild
install -m 0755 atrium-capsa-fuse /usr/local/bin/atrium-capsa-fuse
id _capsatest >/dev/null 2>&1 || pw useradd _capsatest -u 65070 -d /nonexistent -s /usr/sbin/nologin -c "fuse gate server"
# The volume: FAT32 on md99, with one file written by the kernel's msdosfs
# beforehand (this is the test's own image, not removable media).
pkill -f atrium-portcullisd-jclient; umount -f /media/capsa-md99 2>/dev/null; umount -f /media/md98p1 2>/dev/null
umount /root/fz/m 2>/dev/null; mdconfig -d -u 99 -o force 2>/dev/null
truncate -s 80m /root/fz/vol.img && mdconfig -a -t vnode -f /root/fz/vol.img -u 99 \
  && newfs_msdos -F 32 -c 1 -L CAPSA /dev/md99 >/dev/null 2>&1 \
  && mkdir -p /root/fz/m && mount_msdosfs /dev/md99 /root/fz/m \
  && echo "written before the server ran" > /root/fz/m/hello.txt && umount /root/fz/m \
  && echo "  ok md99 holds a FAT32 volume with hello.txt" || echo "  FAIL could not make the md99 volume"
devfs rule -s 30 delset 2>/dev/null
devfs rule -s 30 add include 1 && devfs rule -s 30 add include 2
[ -f $P.fusetest-bak ] || cp $P $P.fusetest-bak
cp $P.fusetest-bak $P
sed -i '' 's/^allowed_ids = \[\(.*\)\]/allowed_ids = [\1, 30]/' $P
sed -i '' 's/^allowed_system_uids = \[/allowed_system_uids = [ 65070,/' $P
sed -i '' 's/^allowed_system_gids = \[/allowed_system_gids = [ 65070,/' $P
cat >> $P <<'EOP'

# vm-fuse-check.sh only (removed at the end).
[[devfs_rulesets.pinned]]
id                = 30
jail_name         = "atrium-capsa-"
name_prefix       = true
exec_path         = "/usr/local/bin/atrium-capsa-fuse"
uid               = 65070
fuse_devices      = ["md#", "md#p#"]
fuse_mount_prefix = "/media"
EOP
grep -q '^allowed_ids = .*30\]' $P && grep -q '^allowed_system_gids = \[ 65070,' $P \
  && echo "  ok test policy installed" || echo "  FAIL could not edit the policy"
grep -q 'sysfil.profiles.fuse-server' $P || cat >> $P <<'EOP'

[sysfil.profiles.fuse-server]
include = ["base"]
ioctls  = ["0x40046480", "0x40086481"]
EOP
service atrium-jaild start >/dev/null 2>&1; sleep 2
pgrep -qf atrium-jaild && echo "  ok jaild running" || echo "  FAIL jaild not running"
EOF

S=/var/run/atrium/jaild.sock
JC=/root/fz/atrium-portcullisd-jclient
MNT=/media/capsa-md99
J=atrium-capsa-md99
R=/var/lib/atrium/jails/capsa-md99
SESSION_UID=1001
# start NAME FUSE-SPEC: the server, held by a background jclient.
# jclient's jails are not dispensatord's: nothing unmounts their ro mounts.
unmount_root() { V "for m in \$(mount -p | awk '{print \$2}' | grep '^$R/' | sort -r); do umount -f \$m; done"; }
start() {
  unmount_root
  V "cd /root/fz; ATRIUM_JCLIENT_HOLD=600 ATRIUM_JCLIENT_DEVFS=30 ATRIUM_JCLIENT_UID=65070 ATRIUM_JCLIENT_GID=65070 \
     ATRIUM_JCLIENT_SYSFIL=fuse-server ATRIUM_JCLIENT_MOUNTS_RO=/libexec,/lib,/usr/lib,/usr/local/bin ATRIUM_JCLIENT_FUSE='$2' \
     daemon -p /root/fz/jc.pid -o /root/fz/jc-$1.log $JC $S exec $1 $R /usr/local/bin/atrium-capsa-fuse \
       --dev-fd 3 --fuse-fd 4 --uid $SESSION_UID --gid $SESSION_UID ${3:-} </dev/null >/dev/null 2>&1"
  sleep 4
}
# stop NAME: drop the procdesc (kills the server), then RemoveJail.
stop() {
  V "kill \$(cat /root/fz/jc.pid) 2>/dev/null; sleep 2; $JC $S remove $1 >/dev/null 2>&1"
  sleep 1
  unmount_root
}

echo "== serve md99 read-write"
start $J "md99:$MNT"
V "cat /root/fz/jc-$J.log | grep -E 'error|Error|denied' | head -3 | sed 's/^/  jclient: /'"
if ! V "mount -p | tr -s '\t' ' ' | grep -q ' $MNT fusefs'"; then
  bad "no fusefs on $MNT ($(V "cat /root/fz/jc-$J.log | tr '\n' ' ' | cut -c1-300"); $(V "tail -3 /var/log/atrium/jaild.log 2>/dev/null | tr '\n' ';'"))"
else
  ok "jaild mounted fusefs on $MNT ($(V "mount -p | tr -s '\t' ' ' | grep ' $MNT ' | awk '{print \$1, \$4}'"))"
  got=$(V "cat $MNT/HELLO.TXT 2>&1; cat $MNT/hello.txt 2>&1" | head -1)
  [ "$got" = "written before the server ran" ] && ok "reads the file msdosfs wrote" || bad "hello.txt reads '$got'"
  V "sh -c 'cd $MNT && echo one > new.txt && echo two >> new.txt && mkdir -p dir/sub && mv new.txt dir/sub/renamed.txt \
      && dd if=/dev/random of=big.bin bs=64k count=32 2>/dev/null && sha256 -q big.bin > /root/fz/big.sha \
      && touch gone && rm gone && mkdir empty && rmdir empty && echo \"Grüße\" > \"Grüße.txt\"'" \
    && ok "create, append, mkdir, rename, a 2 MiB write, unlink, rmdir, a Unicode name" || bad "a write through the mount failed"
  [ "$(V "cat $MNT/dir/sub/renamed.txt | tr '\n' ' '")" = "one two " ] && ok "renamed file reads back" || bad "renamed file reads '$(V "cat $MNT/dir/sub/renamed.txt")'"
  [ "$(V "stat -f '%u %Lp' $MNT/dir")" = "$SESSION_UID 755" ] && ok "owned by the session uid $SESSION_UID" || bad "dir is '$(V "stat -f '%u %Lp' $MNT/dir")'"
  V "df $MNT | tail -1" | sed 's/^/  df: /'
  pid=$(V "pgrep -f '^/usr/local/bin/atrium-capsa-fuse|^atrium-capsa-fuse' | head -1")
  if [ -n "$pid" ]; then
    fds=$(V "procstat -f $pid | awk '\$3 == 3 || \$3 == 4 {print \$3, \$NF}' | tr '\n' ';'")
    echo "$fds" | grep -q "3 /dev/md99" && echo "$fds" | grep -q "4 /dev/fuse" \
      && ok "the server holds md99 as fd 3 and fuse as fd 4" || bad "server fds: $fds ($(V "procstat -f $pid | tail -6 | tr '\n' ';'"))"
    V "procstat -f $pid | awk 'NR>1 && \$3 ~ /^[0-9]+$/ && \$3 > 4' | grep -q ." && bad "the server holds more descriptors: $(V "procstat -f $pid | awk 'NR>1 && \$3 > 4' | tr '\n' ';'")" || ok "and no other descriptor past stdio"
    [ "$(V "ps -o jid= -p $pid | tr -d ' '")" != "0" ] && ok "server runs in a jail" || bad "server is not jailed"
  else
    bad "no atrium-capsa-fuse process"
  fi
  V "ls $R/dev | grep -qE '^(md|fuse)'" && bad "the jail's /dev shows $(V "ls $R/dev | grep -E '^(md|fuse)' | tr '\n' ' '")" || ok "the jail's /dev shows neither md99 nor fuse"
fi
stop $J
V "mount -p | tr -s '\t' ' ' | grep -q ' $MNT '" && bad "$MNT still mounted after RemoveJail" || ok "RemoveJail left no mount"
V "fsck_msdosfs -n /dev/md99 2>&1 | grep -qiE 'fix\?|truncate\?|clear\?|reconnect\?|bad |error|invalid|out of range'" && bad "fsck_msdosfs: $(V "fsck_msdosfs -n /dev/md99 2>&1 | tr '\n' ';'")" || ok "fsck_msdosfs finds the volume clean"
V "mount_msdosfs /dev/md99 /root/fz/m && cat /root/fz/m/dir/sub/renamed.txt | tr '\n' ' ' && sha256 -q /root/fz/m/big.bin | cmp -s - /root/fz/big.sha && echo sha-ok; ls /root/fz/m | grep -c 'Grüße'; umount /root/fz/m" > /tmp/fuse-kcheck 2>&1
grep -q "one two" /tmp/fuse-kcheck && grep -q "sha-ok" /tmp/fuse-kcheck && ok "msdosfs reads what the server wrote (text, 2 MiB checksum)" || bad "msdosfs check: $(tr '\n' ';' < /tmp/fuse-kcheck)"

echo "== read-only"
start $J "md99:$MNT:ro" --ro
if V "mount -p | tr -s '\t' ' ' | grep ' $MNT fusefs' | grep -qE ' ro[, ]'"; then
  ok "mounted read-only"
  err=$(V "sh -c 'echo x > $MNT/ro.txt' 2>&1")
  echo "$err" | grep -qi "read-only" && ok "a write fails: read-only file system" || bad "a write gave '$err'"
  [ "$(V "cat $MNT/dir/sub/renamed.txt | tr '\n' ' '")" = "one two " ] && ok "reads still work" || bad "read failed read-only"
else
  bad "no read-only mount ($(V "cat /root/fz/jc-$J.log | tr '\n' ' ' | cut -c1-300"))"
fi
stop $J

echo "== refusals"
DISK=$(V "sysctl -n kern.disks | tr ' ' '\n' | grep -v '^md' | head -1")
V "ATRIUM_JCLIENT_DEVFS=30 ATRIUM_JCLIENT_UID=65070 ATRIUM_JCLIENT_GID=65070 ATRIUM_JCLIENT_FUSE='$DISK:$MNT' $JC $S exec $J $R /usr/local/bin/atrium-capsa-fuse 2>&1" > /tmp/fuse-refuse 2>&1
grep -qi "pinned\|policy" /tmp/fuse-refuse && ok "refuses the VM's own disk $DISK" || bad "the disk $DISK: $(tr '\n' ' ' < /tmp/fuse-refuse | cut -c1-200)"
V "ATRIUM_JCLIENT_DEVFS=30 ATRIUM_JCLIENT_UID=65070 ATRIUM_JCLIENT_GID=65070 ATRIUM_JCLIENT_FUSE='md99:/mnt/capsa' $JC $S exec $J $R /usr/local/bin/atrium-capsa-fuse 2>&1" > /tmp/fuse-refuse 2>&1
grep -qi "pinned\|policy" /tmp/fuse-refuse && ok "refuses a mountpoint outside /media" || bad "/mnt/capsa: $(tr '\n' ' ' < /tmp/fuse-refuse | cut -c1-200)"
V "mount -p | tr -s '\t' ' ' | grep -q fusefs" && bad "a fusefs mount is left: $(V 'mount -p | grep fusefs')" || ok "no fusefs mount left"

echo "== dispensatord: a disk rule serves partitions as GEOM makes them"
V 'sh -s' <<'EOF'
service atrium-dispensatord stop >/dev/null 2>&1
install -m 0755 /root/fz/atrium-dispensatord /usr/local/bin/atrium-dispensatord
install -d -m 0755 /usr/local/etc/atrium/devices.d
cat > /usr/local/etc/atrium/devices.d/90-fusetest.toml <<'EOR'
[[rule]]
name  = "fusetest-md"
class = "disk"
disk  = ["md"]
nodes = ["{part}"]
owner = "driver:fusetest-capsa"

[driver.fusetest-capsa]
exec          = "/usr/local/bin/atrium-capsa-fuse"
argv          = ["atrium-capsa-fuse", "--dev-fd", "3", "--fuse-fd", "4", "--uid", "1001", "--gid", "1001"]
uid           = 65070
gid           = 65070
jail          = "atrium-capsa-{part}"
root          = "/var/lib/atrium/jails/capsa-{part}"
devfs_ruleset = 30
sysfil        = "fuse-server"
mounts_ro     = ["/libexec", "/lib", "/usr/lib", "/usr/local/bin"]
fuse_mount    = "/media/{part}"
EOR
chown root:wheel /usr/local/etc/atrium/devices.d/90-fusetest.toml; chmod 0644 /usr/local/etc/atrium/devices.d/90-fusetest.toml
/usr/local/bin/atrium-dispensatord --check | grep fusetest | sed 's/^/  rules: /'
: > /var/log/atrium/dispensatord.log
service atrium-dispensatord start >/dev/null 2>&1; sleep 4
pgrep -qf atrium-dispensatord && echo "  ok dispensatord running" || echo "  FAIL dispensatord not running"
# A "stick": GPT with one FAT32 partition, on a file attached as md98.
mdconfig -d -u 98 -o force 2>/dev/null
truncate -s 80m /root/fz/stick.img && mdconfig -a -t vnode -f /root/fz/stick.img -u 98 \
  && gpart create -s gpt md98 >/dev/null && gpart add -t ms-basic-data md98 >/dev/null \
  && newfs_msdos -F 32 -c 1 -L STICK /dev/md98p1 >/dev/null 2>&1 && echo "  md98p1 made"
# Plug it in again, now with the filesystem on it.
mdconfig -d -u 98 -o force; sleep 3
mdconfig -a -t vnode -f /root/fz/stick.img -u 98
sleep 8
EOF
if V "mount -p | tr -s '\t' ' ' | grep -q ' /media/md98p1 fusefs'"; then
  ok "md98p1 served at /media/md98p1 by dispensatord's rule"
  V "sh -c 'echo stick > /media/md98p1/s.txt && cat /media/md98p1/s.txt'" | grep -qx stick && ok "the stick takes a write" || bad "write to the stick failed"
  [ "$(V "stat -f %u /media/md98p1/s.txt")" = "1001" ] && ok "files belong to the session uid" || bad "s.txt owner $(V "stat -f %u /media/md98p1/s.txt")"
else
  bad "no mount at /media/md98p1 ($(V "grep -E 'md98|capsa' /var/log/atrium/dispensatord.log | tail -4 | tr '\n' ';'"); state: $(V 'cat /var/run/atrium/dispensator.state | tr "\n" ";"'))"
fi
V 'mdconfig -d -u 98 -o force'; sleep 6
V "mount -p | tr -s '\t' ' ' | grep -q ' /media/md98p1 '" && bad "/media/md98p1 still mounted after the md went" || ok "md gone: mount gone"
V "jls -n name | grep -q 'name=atrium-capsa-md98p1'" && bad "its jail is still running" || ok "and its jail"
V "grep -E 'md98|capsa' /var/log/atrium/dispensatord.log | tail -4" | sed 's/^/  log: /'

echo "== clean up"
V 'sh -s' <<'EOF'
P=/etc/atrium/jaild.policy.toml
rm -f /usr/local/etc/atrium/devices.d/90-fusetest.toml
service atrium-dispensatord restart >/dev/null 2>&1
mdconfig -d -u 98 -o force 2>/dev/null
umount /root/fz/m 2>/dev/null; mdconfig -d -u 99 2>/dev/null
[ -f $P.fusetest-bak ] && mv $P.fusetest-bak $P
service atrium-jaild restart >/dev/null 2>&1; sleep 2
devfs rule -s 30 delset 2>/dev/null
echo "  test volume, ruleset and pin removed"
EOF
echo "fuse: $fails failed"
exit $fails
