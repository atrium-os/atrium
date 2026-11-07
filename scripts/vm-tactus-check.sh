#!/bin/sh
# Tactus T2 check (docs/spec/atrium-input.md §8): real USB HID through a
# jailed tactusd into frescod and a window. Runs on the HOST of the cloud
# amd64 VM (scripts/cloud-vm.sh), which must have been started with QEMU's
# USB devices:
#
#   ARCH=amd64 TCG_CPU=Haswell-noTSX QEMU_BIN=~/atrium-os/qemu/build/qemu-system-x86_64 \
#     QEMU_EXTRA="-device qemu-xhci -device usb-kbd -device usb-tablet -device usb-mouse" \
#     sh scripts/cloud-vm.sh up
#   sh scripts/vm-tactus-check.sh TACTUSD FRESCOD INPUT_PROBE
#
# (x86_64-unknown-freebsd release builds of tactus's tactusd, frescod and
# atrium-test-client's atrium-input-probe.) It commissions tactusd as the
# atrium-tactusd jail (scripts/tactus-service.sh, devfs ruleset 26, the
# jaild policy entries), starts it through portcullisd-bootstrap, runs a
# headless frescod (no GPU needed) and the probe window, then types and
# clicks through the QEMU monitor and checks what the window got:
# keys with shift, a token on each press and none on releases, repeats not
# passed on, a click with a token, motion, a scroll, a hot-plugged keyboard
# and frescod waiting for tactusd when it restarts. Ends "tactus: N failed".
set -u
TD=$1 FD=$2 PROBE=$3
ROOT=$(cd "$(dirname "$0")/.." && pwd)
VMDIR=${VMDIR:-$HOME/atrium-vm-amd64}
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

echo "== copy"
V 'pkill -x frescod; pkill -f atrium-input-probe; pkill -f "services-dir /tmp/tsvc"; pkill -f atrium-tactusd; sleep 1; mkdir -p /root/t/scripts /root/t/tactus/etc/services.d'
scp -q $K -P 2222 "$TD" root@localhost:/root/t/tactusd
scp -q $K -P 2222 "$FD" root@localhost:/root/t/frescod
scp -q $K -P 2222 "$PROBE" root@localhost:/root/t/atrium-input-probe
scp -q $K -P 2222 "$ROOT/scripts/tactus-service.sh" root@localhost:/root/t/scripts/
scp -q $K -P 2222 "$ROOT/tactus/etc/services.d/38-atrium-tactusd.toml" root@localhost:/root/t/tactus/etc/services.d/
scp -q $K -P 2222 "$ROOT/etc/atrium.devfs.rules" root@localhost:/root/t/

echo "== commission tactusd (jailed, devfs ruleset 26)"
V 'sh -s' <<'EOF'
cd /root/t
pkill -f "tactusd" ; pkill -x frescod; pkill -f atrium-input-probe
install -m 0644 atrium.devfs.rules /usr/local/etc/atrium/devfs.rules
service devfs restart >/dev/null 2>&1
devfs rule -s 26 show | grep -q hidraw && echo "  ok ruleset 26 loaded" || echo "  FAIL ruleset 26 not loaded"
P=/etc/atrium/jaild.policy.toml
grep -q 'atrium-tactusd' $P || {
  cp $P $P.pre-tactus
  sed -i '' -E 's/^allowed_ids = \[([0-9, ]*)\]/allowed_ids = [\1, 26]/' $P
  sed -i '' -E 's/^    65010,   # _frescod: the compositor/    65018,   # _tactus\n&/' $P
  sed -i '' -E 's/^    65010,   # _frescod$/    65018,   # _tactus\n&/' $P
  printf '\n[[devfs_rulesets.pinned]]\nid        = 26\njail_name = "atrium-tactusd"\nexec_path = "/usr/local/bin/atrium-tactusd"\nuid       = 65018\n' > /tmp/pin
  awk 'BEGIN{while((getline l < "/tmp/pin")>0) pin=pin l "\n"} /^\[\[devfs_rulesets.pinned\]\]/ && !done {printf "%s\n", pin; done=1} {print}' $P > $P.new && mv $P.new $P
}
grep -c "65018" $P | sed 's/^/  policy mentions 65018: /'
service atrium-jaild restart >/dev/null 2>&1; sleep 2
sh scripts/tactus-service.sh /root/t/tactusd | tail -1
mkdir -p /tmp/tsvc && cp /etc/atrium/services.d/38-atrium-tactusd.toml /tmp/tsvc/
pkill -f "services-dir /tmp/tsvc"
daemon -f -o /var/log/atrium/tsvc-bootstrap.log /usr/local/bin/atrium-portcullisd-bootstrap --socket /var/run/atrium/jaild.sock --services-dir /tmp/tsvc
sleep 12
jls -n name | grep -q atrium-tactusd && echo "  ok atrium-tactusd jail up" || { echo "  FAIL no tactusd jail"; tail -8 /var/log/atrium/tsvc-bootstrap.log; }
ps -axo user,jid,command | grep "[a]trium-tactusd" | head -2
ls -l /atrium/sockets/tactus/
EOF

echo "== frescod (headless) and the probe window"
V 'sh -s' <<'EOF'
cd /root/t
rm -rf /tmp/hl /tmp/frescod-t.log; mkdir -p /tmp/hl /atrium/sockets/fresco
daemon -f -o /tmp/frescod-t.log env FRESCOD_HEADLESS_PNG=/tmp/hl/f FRESCOD_SPLIT=0 FRESCOD_SOCK=/tmp/fresco-t.sock FRESCOD_WM_SOCK=/tmp/fresco-wm-t.sock ./frescod
sleep 8
grep -E "tactus|input" /tmp/frescod-t.log | head -5
S=$(grep -o "/[^ ]*fresco[^ ]*\.sock" /tmp/frescod-t.log | head -1); [ -S /tmp/fresco-t.sock ] && S=/tmp/fresco-t.sock
echo "  frescod socket $S"
rm -f /tmp/probe.log; daemon -f -o /tmp/probe.log ./atrium-input-probe $S 600
sleep 5; cat /tmp/probe.log
EOF

echo "== type and click (QEMU monitor)"
mon "mouse_set 5" "sendkey shift-a" "sendkey b" "sendkey c 1500" "mouse_move 20 10" "mouse_button 1" "mouse_button 0" "mouse_move 0 0 2"
sleep 4
mon "device_add usb-kbd,id=tk2"; sleep 8
mon "sendkey d"; sleep 3
mon "device_del tk2"; sleep 5
V 'cat /tmp/probe.log' > /tmp/tactus-probe.log
cat /tmp/tactus-probe.log | head -40
P=/tmp/tactus-probe.log
grep -q "usage=0x0004 pressed=1 mods=1 token" $P && ok "shift-a: a with shift, token" || bad "shift-a"
grep -q "usage=0x0004 pressed=0 mods=1 no-token" $P && ok "release without token" || bad "a release"
grep -q "usage=0x00e1" $P && bad "modifier sent as a key" || ok "modifiers only as bits"
[ "$(grep -c "usage=0x0006 pressed=1" $P)" = 1 ] && ok "held c: one press, repeats not passed on" || bad "held c pressed $(grep -c 'usage=0x0006 pressed=1' $P) times"
grep -q "button window=[1-9][0-9]* button=1 pressed=1 token" $P && ok "click with token" || bad "click"
grep -q "^motion window=" $P && ok "motion" || bad "motion"
grep -q "^scroll window=" $P && ok "scroll" || bad "scroll"
grep -q "usage=0x0007 pressed=1" $P && ok "hot-plugged keyboard types" || bad "hot-plugged keyboard"

echo "== tactusd restarts: frescod waits, then gets input again"
V 'pkill -x atrium-tactusd; sleep 8; ps -axo command | grep -c "[a]trium-tactusd"; grep -c "input from tactusd" /tmp/frescod-t.log; tail -3 /tmp/frescod-t.log'
mon "sendkey e"; sleep 3
V 'grep -q "usage=0x0008 pressed=1" /tmp/probe.log' && ok "input after tactusd restart" || bad "no input after tactusd restart"
V 'echo $(ls /var/lib/atrium/jails/tactusd/dev)' | sed 's/^/  tactusd jail \/dev: /'
V 'ls /var/lib/atrium/jails/frescod/dev 2>/dev/null | grep -c hidraw' | sed 's/^/  hidraw nodes in frescod jail devfs (needs a frescod jail restart to apply): /'
echo "tactus: $fails failed"
