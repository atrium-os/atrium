#!/bin/sh
# Device assignment D3 gate (docs/spec/device-assignment.md §6): sound cards
# and the PS/2 keyboard and pointer are shown to their services per device by
# atrium-dispensatord, and the rulesets no longer unhide them wholesale. Runs
# on the HOST of the cloud amd64 VM (scripts/cloud-vm.sh) with the Portcullis
# stack deployed (scripts/deploy-portcullis.sh). Best with a sound card and a
# USB controller:
#
#   ARCH=amd64 QEMU_EXTRA="-device qemu-xhci -device usb-kbd -device intel-hda -device hda-output" \
#     sh scripts/cloud-vm.sh up
#   sh scripts/vm-devices-check.sh
#
# It cross-builds atrium-dispensatord, installs it with every shipped
# devices.d rule and the current etc/atrium.devfs.rules, and checks:
#
#   - rulesets 24 and 26 no longer unhide dsp*, mixer*, atkbd* or bpsm*;
#   - lyrad's devfs (its jail's, or one this script mounts on ruleset 24 the
#     way jaild would) shows dspN and mixerN for every sound card, and
#     /dev/dsp resolves there through sound(4)'s clone handler;
#   - tactusd's devfs shows atkbd0 and bpsm0 when the machine has them;
#   - neither service sees the other's nodes;
#   - a USB sound card hot-plugged through the QEMU monitor is shown to lyrad
#     within seconds, and is gone after unplugging; the first card stays.
#
# Ends "devices: N failed".
set -u
ROOT=$(cd "$(dirname "$0")/.." && pwd)
VMDIR=${VMDIR:-$HOME/atrium-vm-amd64}
TRIPLE=x86_64-unknown-freebsd
K="-i $HOME/.ssh/fresco_bsd_ed25519 -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o LogLevel=ERROR"
V() { "$ROOT/scripts/vssh" "$@"; }
mon() { python3 -I -c '
import socket,sys,time
s=socket.socket(socket.AF_UNIX); s.connect(sys.argv[1])
for c in sys.argv[2:]:
    s.sendall(c.encode()+b"\n"); time.sleep(0.5)
' "$VMDIR/mon.sock" "$@"; }
fails=0
ok() { echo "  ok $1"; }
bad() { echo "  FAIL $1"; fails=$((fails+1)); }

# An unreachable guest would make every "does not contain" check pass.
V true >/dev/null 2>&1 || { echo "the VM does not answer over ssh (scripts/cloud-vm.sh up)"; echo "devices: 1 failed"; exit 1; }

echo "== cross-build"
CROSS_RUSTFLAGS=$(cd "$ROOT" && ARCH=amd64 sh -c '. ./scripts/cloud-vm.sh >/dev/null; printf %s "$CROSS_RUSTFLAGS"' sh env)
( cd "$ROOT/dispensator" && RUSTFLAGS="$CROSS_RUSTFLAGS" cargo build -q --release --target $TRIPLE ) || { echo "build failed"; exit 1; }

echo "== copy"
V 'rm -rf /root/d && mkdir -p /root/d/rules'
scp -q $K -P 2222 "$ROOT/dispensator/target/$TRIPLE/release/atrium-dispensatord" root@localhost:/root/d/
scp -q $K -P 2222 "$ROOT/dispensator/etc/atrium-dispensatord" root@localhost:/root/d/atrium-dispensatord.rc
scp -q $K -P 2222 "$ROOT/etc/atrium.devfs.rules" root@localhost:/root/d/
scp -q $K -P 2222 "$ROOT"/*/etc/devices.d/*.toml root@localhost:/root/d/rules/

echo "== install, reload rulesets, restart"
V 'sh -s' <<'EOF'
cd /root/d
install -d -m 0755 /usr/local/etc/atrium/devices.d
install -o root -g wheel -m 0644 rules/*.toml /usr/local/etc/atrium/devices.d/
install -m 0644 atrium.devfs.rules /usr/local/etc/atrium/devfs.rules
service devfs restart >/dev/null 2>&1
service atrium-dispensatord stop >/dev/null 2>&1
install -m 0755 atrium-dispensatord /usr/local/bin/atrium-dispensatord
install -m 0555 atrium-dispensatord.rc /usr/local/etc/rc.d/atrium-dispensatord
sysrc atrium_dispensatord_enable=YES >/dev/null
/usr/local/bin/atrium-dispensatord --check | sed 's/^/  rules: /'
# A service whose jail is not running gets the mount jaild would give it.
rm -f /root/d/mounted
for sv in lyrad:24 tactusd:26; do
  n=${sv%:*} rs=${sv#*:} m=/var/lib/atrium/jails/${sv%:*}/dev
  if ! mount -p | awk '{print $2}' | grep -qx "$m"; then
    install -d "$m" && mount -t devfs devfs "$m" && devfs -m "$m" ruleset $rs && devfs -m "$m" rule applyset && echo "$m" >> /root/d/mounted
    echo "  ($n not running: mounted its devfs on ruleset $rs)"
  fi
done
kldload -n snd_hda snd_uaudio 2>/dev/null
service atrium-dispensatord start >/dev/null 2>&1
sleep 6
pgrep -qf atrium-dispensatord && echo "  ok dispensatord running" || { echo "  FAIL dispensatord not running"; tail -5 /var/log/atrium/dispensatord.log; }
EOF

echo "== rulesets"
V 'devfs rule -s 24 show; echo ---; devfs rule -s 26 show' > /tmp/devices-rs.log
grep -E "(dsp|mixer|atkbd|bpsm).*unhide" /tmp/devices-rs.log && bad "a ruleset still unhides a device wholesale" || ok "rulesets 24 and 26 unhide no device wholesale"

L=/var/lib/atrium/jails/lyrad/dev
T=/var/lib/atrium/jails/tactusd/dev
echo "== lyrad"
PCMS=$(V "sysctl -N dev.pcm 2>/dev/null | sed -n 's/^dev\.pcm\.\([0-9]*\)\.%desc\$/\1/p' | sort -u | tr '\n' ' '")
echo "  sound cards: ${PCMS:-none}"
if [ -z "$PCMS" ]; then
  echo "  skip: no sound card (add -device intel-hda -device hda-output)"
else
  for u in $PCMS; do
    V "test -e $L/dsp$u && test -e $L/mixer$u" && ok "lyrad sees dsp$u and mixer$u" || bad "lyrad lacks dsp$u/mixer$u ($(V "ls $L | tr '\n' ' '"))"
  done
  V "test -e $L/dsp && test -e $L/mixer" && ok "/dev/dsp and /dev/mixer resolve in lyrad's devfs" || bad "/dev/dsp does not resolve in lyrad's devfs"
fi
V "ls $L | grep -qE '^(hidraw|atkbd|bpsm|u2f)'" && bad "lyrad sees input nodes" || ok "lyrad sees no input node"

echo "== tactusd"
for n in atkbd0 bpsm0; do
  if V "test -e /dev/$n"; then
    V "test -e $T/$n" && ok "tactusd sees $n" || bad "tactusd lacks $n"
  else
    echo "  skip: no /dev/$n on this machine"
  fi
done
V "ls $T | grep -qE '^(dsp|mixer)'" && bad "tactusd sees sound nodes" || ok "tactusd sees no sound node"

echo "== hot-plug a USB sound card"
before=$(V "ls $L | grep -c '^dsp[0-9]'")
mon "device_add usb-audio,id=ua9"; sleep 10
after=$(V "ls $L | grep -c '^dsp[0-9]'")
NEW=$(V "sysctl -N dev.pcm 2>/dev/null | sed -n 's/^dev\.pcm\.\([0-9]*\)\.%desc\$/\1/p' | sort -u | tail -1")
if [ "$after" -gt "$before" ]; then ok "hot-plugged card shown to lyrad (dsp$NEW)"; else bad "no new dsp in lyrad's devfs ($before -> $after; host: $(V 'ls /dev | grep ^dsp | tr "\n" " "'))"; fi
mon "device_del ua9"; sleep 6
gone=$(V "ls $L | grep -c '^dsp[0-9]'")
[ "$gone" -eq "$before" ] && ok "unplugged card gone, the others stay ($gone)" || bad "after unplug lyrad has $gone dsp nodes, had $before"
V 'cat /var/run/atrium/dispensator.state' | sed 's/^/  state: /'

V 'sh -s' <<'EOF'
[ -f /root/d/mounted ] && while read m; do umount "$m"; done < /root/d/mounted
EOF
echo "devices: $fails failed"
exit $fails
