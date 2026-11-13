#!/bin/sh
# Device assignment D4 groundwork gate (docs/spec/device-assignment.md §6.3):
# a `usb` rule gives one USB interface to a jailed driver that gets the
# device's two nodes (ugenB.A and usb/B.A.0) through jaild's devfs_grants,
# and nothing else. Runs on the HOST of the cloud amd64 VM
# (scripts/cloud-vm.sh) with the Portcullis stack deployed and a USB
# controller on the machine:
#
#   ARCH=amd64 QEMU_EXTRA="-device qemu-xhci" sh scripts/cloud-vm.sh up
#   sh scripts/vm-usb-check.sh
#
# It cross-builds jaild, atrium-dispensatord and a stand-in driver
# (dispensator/examples/usb-probe.rs), installs them, and for the test only:
# devfs ruleset 29 (basics only), a jaild pin for it (atrium-usbprobe-*,
# grant_nodes ugen#.# and usb/#.#.0) and a devices.d file with two rules.
# Then it hot-plugs through the QEMU monitor:
#
#   - usb-serial (FTDI 0403:6001, which uftdi(4) claims): the rule has
#     detach_kernel, so dispensatord detaches uftdi and starts the probe;
#   - usb-ccid (class 0x0B, no kernel driver) when this QEMU has it: the
#     nomatch path.
#
# For each: the probe's jail is up and stays up (the probe exits, ending its
# jail, unless it can open both nodes, read the device's ids, and sees no
# other USB node); its /dev holds exactly ugenB.A and usb/B.A.0, the latter
# owned by the probe's uid, 0600; after unplugging the jail is gone. The
# test ruleset, pin and rules are removed at the end.
#
# Ends "usb: N failed".
set -u
ROOT=$(cd "$(dirname "$0")/.." && pwd)
VMDIR=${VMDIR:-$HOME/atrium-vm-amd64}
TRIPLE=x86_64-unknown-freebsd
K="-i $HOME/.ssh/fresco_bsd_ed25519 -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o LogLevel=ERROR"
V() { "$ROOT/scripts/vssh" "$@"; }
# Monitor commands; prints QEMU's answers.
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

V true >/dev/null 2>&1 || { echo "the VM does not answer over ssh (scripts/cloud-vm.sh up)"; echo "usb: 1 failed"; exit 1; }
V 'usbconfig list >/dev/null 2>&1' || { echo "the VM has no USB controller (QEMU_EXTRA=\"-device qemu-xhci\")"; echo "usb: 1 failed"; exit 1; }

echo "== cross-build"
CROSS_RUSTFLAGS=$(cd "$ROOT" && ARCH=amd64 sh -c '. ./scripts/cloud-vm.sh >/dev/null; printf %s "$CROSS_RUSTFLAGS"' sh env)
( cd "$ROOT/dispensator" && RUSTFLAGS="$CROSS_RUSTFLAGS" cargo build -q --release --target $TRIPLE --bins --example usb-probe ) || { echo "dispensator build failed"; exit 1; }
( cd "$ROOT/portcullis" && RUSTFLAGS="$CROSS_RUSTFLAGS" cargo build -q --release --target $TRIPLE -p jaild ) || { echo "jaild build failed"; exit 1; }
D=$ROOT/dispensator/target/$TRIPLE/release

echo "== copy"
V 'rm -rf /root/u && mkdir -p /root/u'
scp -q $K -P 2222 "$D/atrium-dispensatord" "$D/examples/usb-probe" "$ROOT/portcullis/target/$TRIPLE/release/atrium-jaild" root@localhost:/root/u/

echo "== install (test ruleset 29, pin, rules)"
V 'sh -s' <<'EOF'
P=/etc/atrium/jaild.policy.toml
cd /root/u
service atrium-dispensatord stop >/dev/null 2>&1
service atrium-jaild stop >/dev/null 2>&1
install -m 0755 atrium-jaild /usr/local/bin/atrium-jaild
install -m 0755 atrium-dispensatord /usr/local/bin/atrium-dispensatord
install -m 0755 usb-probe /usr/local/bin/atrium-usbprobe
id _usbprobe >/dev/null 2>&1 || pw useradd _usbprobe -u 65060 -d /nonexistent -s /usr/sbin/nologin -c "usb gate probe"
devfs rule -s 29 delset 2>/dev/null
devfs rule -s 29 add include 1 && devfs rule -s 29 add include 2
[ -f $P.usbtest-bak ] || cp $P $P.usbtest-bak
cp $P.usbtest-bak $P
sed -i '' 's/^allowed_ids = \[\(.*\)\]/allowed_ids = [\1, 29]/' $P
sed -i '' 's/^allowed_system_uids = \[/allowed_system_uids = [ 65060,/' $P
cat >> $P <<'EOP'

# vm-usb-check.sh only (removed at the end).
[[devfs_rulesets.pinned]]
id          = 29
jail_name   = "atrium-usbprobe-"
name_prefix = true
exec_path   = "/usr/local/bin/atrium-usbprobe"
uid         = 65060
grant_nodes = ["ugen#.#", "usb/#.#.0"]
EOP
grep -q '^allowed_ids = .*29\]' $P && echo "  ok policy allows ruleset 29" || echo "  FAIL could not add 29 to allowed_ids"
grep -q '^allowed_system_uids = \[ 65060,' $P && echo "  ok policy allows uid 65060" || echo "  FAIL could not add 65060 to allowed_system_uids"
cat > /usr/local/etc/atrium/devices.d/90-usbtest.toml <<'EOR'
[[rule]]
name          = "usbtest-serial"
class         = "usb"
usb_ids       = ["0403:6001"]
detach_kernel = true
nodes         = ["ugen{bus}.{addr}", "usb/{bus}.{addr}.0"]
all_nodes     = true
owner         = "driver:usbtest-serial"

[[rule]]
name          = "usbtest-ccid"
class         = "usb"
usb_interface = [0x0B]
nodes         = ["ugen{bus}.{addr}", "usb/{bus}.{addr}.0"]
all_nodes     = true
owner         = "driver:usbtest-ccid"

[driver.usbtest-serial]
exec          = "/usr/local/bin/atrium-usbprobe"
argv          = ["atrium-usbprobe", "/dev/{node}", "/dev/usb/{bus}.{addr}.0", "0403:6001"]
uid           = 65060
gid           = 65060
jail          = "atrium-usbprobe-s-{usb}"
root          = "/var/lib/atrium/jails/usbprobe-s-{usb}"
devfs_ruleset = 29
sysfil        = "device"
mounts_ro     = ["/libexec", "/lib", "/usr/lib", "/usr/local/bin"]

[driver.usbtest-ccid]
exec          = "/usr/local/bin/atrium-usbprobe"
argv          = ["atrium-usbprobe", "/dev/{node}", "/dev/usb/{bus}.{addr}.0", "08e6:4433"]
uid           = 65060
gid           = 65060
jail          = "atrium-usbprobe-c-{usb}"
root          = "/var/lib/atrium/jails/usbprobe-c-{usb}"
devfs_ruleset = 29
sysfil        = "device"
mounts_ro     = ["/libexec", "/lib", "/usr/lib", "/usr/local/bin"]
EOR
chown root:wheel /usr/local/etc/atrium/devices.d/90-usbtest.toml; chmod 0644 /usr/local/etc/atrium/devices.d/90-usbtest.toml
/usr/local/bin/atrium-dispensatord --check | grep usbtest | sed 's/^/  rules: /'
kldload -n uftdi 2>/dev/null
service atrium-jaild start >/dev/null 2>&1; sleep 2
pgrep -qf atrium-jaild && echo "  ok jaild running" || echo "  FAIL jaild not running"
: > /var/log/atrium/dispensatord.log
service atrium-dispensatord start >/dev/null 2>&1; sleep 5
pgrep -qf atrium-dispensatord && echo "  ok dispensatord running" || { echo "  FAIL dispensatord not running"; tail -5 /var/log/atrium/dispensatord.log; }
EOF

# check RULE PREFIX VENDOR:PRODUCT
check() {
  rule=$1 pfx=$2
  line=$(V "grep 'rule=$rule ' /var/run/atrium/dispensator.state")
  [ -n "$line" ] || { V 'echo "  usbconfig:"; usbconfig list | sed "s/^/    /"; echo "  devinfo:"; devinfo -v | grep -E "ugen=" | sed "s/^/    /"; echo "  devd (last USB lines in messages):"; grep -E "USB|uftdi|ugen" /var/log/messages | tail -6 | sed "s/^/    /"; echo "  the planner on devinfo -v now:"; devinfo -v > /tmp/usbgate.di; : > /tmp/usbgate.empty; /usr/local/bin/atrium-dispensatord --plan /tmp/usbgate.empty --devinfo /tmp/usbgate.di | grep -iE "ugen|usbtest|detach" | sed "s/^/    /"'; bad "$rule: no device assigned (state: $(V 'cat /var/run/atrium/dispensator.state | tr "\n" ";"'); log: $(V 'tail -5 /var/log/atrium/dispensatord.log | tr "\n" ";"'))"; return 1; }
  key=${line%% *}; ugen=${key%:*}; i=${key#*:}; ba=${ugen#ugen}; b=${ba%.*}; a=${ba#*.}
  inst="$b-$a-$i"; J=atrium-usbprobe-$pfx-$inst; R=/var/lib/atrium/jails/usbprobe-$pfx-$inst/dev
  ok "$rule: $key assigned ($line)"
  sleep 8
  V "jls -n name | grep -qx 'name=$J'" && ok "$rule: jail $J up 8 s later (the probe opened both nodes, read the ids, saw no other USB node)" \
    || { bad "$rule: jail $J not running (probe exited, or never started): $(V "grep -E '$key|$J' /var/log/atrium/dispensatord.log | tail -4 | tr '\n' ';'")"; return 1; }
  got=$(V "ls $R | grep -E '^(ugen|usb)' | tr '\n' ' '; printf '|'; ls $R/usb | tr '\n' ' '")
  [ "$got" = "$ugen usb |$ba.0 " ] && ok "$rule: its /dev has exactly $ugen and usb/$ba.0" || bad "$rule: its /dev shows '$got'"
  V "test -L $R/$ugen" && ok "$rule: $ugen is the symlink" || bad "$rule: $ugen is not a symlink there"
  m=$(V "stat -f '%u %Lp' $R/usb/$ba.0")
  [ "$m" = "65060 600" ] && ok "$rule: usb/$ba.0 is uid 65060, 0600" || bad "$rule: usb/$ba.0 is '$m'"
  V "ls /dev | grep -c '^ugen'" | { read n; [ "$n" -gt 1 ] && ok "$rule: the host has $n ugen devices, the jail one" || echo "  (only one USB device on the host: isolation shown by the probe alone)"; }
  echo "$J $key" > /tmp/usb-last
}

gone() {
  read J key < /tmp/usb-last
  sleep 6
  V "jls -n name | grep -qx 'name=$J'" && bad "$J still running after unplug" || ok "$J gone after unplug"
  V "grep -q '^$key ' /var/run/atrium/dispensator.state" && bad "$key still in the state file" || ok "$key released"
}

echo "== usb-serial (uftdi claims it; detach_kernel)"
mon "chardev-add null,id=usbgate-nul" "device_add usb-serial,chardev=usbgate-nul,id=usbgate-ser" | tr -d '\r' | grep -vE '^\(qemu\)|^QEMU|^$|chardev-add|device_add' | sed 's/^/  qemu: /'
sleep 6
if check usbtest-serial s; then
  V "grep -q 'kernel driver detached' /var/log/atrium/dispensatord.log" && ok "uftdi was detached first" || echo "  (no kernel driver had it: the nomatch path ran instead; is uftdi.ko there?)"
  mon "device_del usbgate-ser" >/dev/null; gone
fi
mon "chardev-remove usbgate-nul" >/dev/null

echo "== usb-ccid (no kernel driver)"
out=$(mon "device_add usb-ccid,id=usbgate-ccid")
if echo "$out" | grep -qiE "not a valid device|error"; then
  echo "  skip: this QEMU has no usb-ccid ($(echo "$out" | tr -d '\r' | grep -iE 'error|valid' | head -1))"
else
  sleep 6
  check usbtest-ccid c && { mon "device_del usbgate-ccid" >/dev/null; gone; }
fi

echo "== clean up"
V 'sh -s' <<'EOF'
P=/etc/atrium/jaild.policy.toml
service atrium-dispensatord stop >/dev/null 2>&1
rm -f /usr/local/etc/atrium/devices.d/90-usbtest.toml
[ -f $P.usbtest-bak ] && mv $P.usbtest-bak $P
service atrium-jaild restart >/dev/null 2>&1; sleep 2
devfs rule -s 29 delset 2>/dev/null
service atrium-dispensatord start >/dev/null 2>&1
echo "  test ruleset, pin and rules removed"
EOF
echo "usb: $fails failed"
exit $fails
