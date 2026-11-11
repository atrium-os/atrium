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
# keys with shift, a token on each press and none on releases or repeats,
# a held key repeating, a click with a token, motion, a scroll, a hot-plugged keyboard
# and frescod waiting for tactusd when it restarts. Ends "tactus: N failed".
#
# PS/2 (T4): start the VM with no USB input (QEMU_EXTRA=""), so QEMU's
# i8042 keyboard and mouse are the only ones, and pass tactus-console:
#
#   PS2=1 sh scripts/vm-tactus-check.sh TACTUSD FRESCOD INPUT_PROBE TACTUS_CONSOLE
#
# The same typing and clicking then goes through atkbd0 (K_RAW, handed over
# from the console's kbdmux by tactus-console) and psm0; the hot-plug step is
# skipped, and the restart step also checks the console gets atkbd0 back.
#
# Gamepads (T5): no USB input needed. A stand-in vinculumd
# (tactus/examples/fake-pad.rs) binds the jailed tactusd's default Bluetooth
# HID socket and plays an LE gamepad; frescod runs in split mode with an
# unconfined fresco-renderer (FRESCOD_RENDERER_LAUNCHER=unconfined), so the
# pad events cross the split records; two probe windows check that the pad
# follows focus (releases and disconnect to the old owner, connect and an
# axis snapshot to the new one):
#
#   GAMEPAD=1 FAKE_PAD=.../examples/fake-pad RENDERER=.../fresco-renderer \
#     sh scripts/vm-tactus-check.sh TACTUSD FRESCOD INPUT_PROBE
set -u
TD=$1 FD=$2 PROBE=$3 CON=${4:-}
PS2=${PS2:-0}
GAMEPAD=${GAMEPAD:-0}
# CLAIM=1 (with GAMEPAD=1): window 1's app is granted input.gamepad_exclusive
# through a test launch registry and policy, claims the pad, and keeps it when
# window 2 takes focus (docs/spec/fresco-capabilities.md §4).
CLAIM=${CLAIM:-0}
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
if [ -n "$CON" ]; then
  V 'mkdir -p /root/t/tactus/etc/rc.d'
  scp -q $K -P 2222 "$CON" root@localhost:/root/t/tactus-console
  scp -q $K -P 2222 "$ROOT/tactus/etc/rc.d/atrium_tactus_console" root@localhost:/root/t/tactus/etc/rc.d/
fi
if [ "$GAMEPAD" = 1 ]; then
  scp -q $K -P 2222 "$FAKE_PAD" root@localhost:/root/t/fake-pad
  scp -q $K -P 2222 "$RENDERER" root@localhost:/root/t/fresco-renderer
fi

echo "== commission tactusd (jailed, devfs ruleset 26)"
V "CON=${CON:+/root/t/tactus-console} sh -s" <<'EOF'
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
sh scripts/tactus-service.sh /root/t/tactusd $CON | tail -1
mkdir -p /tmp/tsvc && cp /etc/atrium/services.d/38-atrium-tactusd.toml /tmp/tsvc/
pkill -f "services-dir /tmp/tsvc"
daemon -f -o /var/log/atrium/tsvc-bootstrap.log /usr/local/bin/atrium-portcullisd-bootstrap --socket /var/run/atrium/jaild.sock --services-dir /tmp/tsvc
sleep 12
jls -n name | grep -q atrium-tactusd && echo "  ok atrium-tactusd jail up" || { echo "  FAIL no tactusd jail"; tail -8 /var/log/atrium/tsvc-bootstrap.log; }
ps -axo user,jid,command | grep "[a]trium-tactusd" | head -2
ls -l /atrium/sockets/tactus/
EOF

echo "== frescod (headless) and the probe window"
V "GAMEPAD=$GAMEPAD CLAIM=$CLAIM sh -s" <<'EOF'
cd /root/t
rm -rf /tmp/hl /tmp/frescod-t.log; mkdir -p /tmp/hl /atrium/sockets/fresco
SPLIT="FRESCOD_SPLIT=0"
[ "$GAMEPAD" = 1 ] && SPLIT="FRESCOD_SPLIT=1 FRESCOD_RENDERER=compute FRESCOD_RENDERER_LAUNCHER=unconfined"
CAPS="FRESCOD_APP_REGISTRY=/tmp/capreg/none"
CLAIMENV=""
if [ "$CLAIM" = 1 ]; then
  # The probes run as root: name root's uid as an app, granted the pad.
  rm -rf /tmp/capreg /tmp/cappol; mkdir -p /tmp/capreg /tmp/cappol/root
  echo "0 root org.test.game" > /tmp/capreg/registry
  printf '%s\n' '[grants."org.test.game"]' 'manifest_hash = "test"' 'granted_at = "test"' \
    '[grants."org.test.game".capabilities]' 'graphics = "fresco"' 'input.gamepad = true' 'input.gamepad_exclusive = true' \
    > /tmp/cappol/root/policy.toml
  CAPS="FRESCOD_APP_REGISTRY=/tmp/capreg/registry ATRIUM_POLICY_BASE=/tmp/cappol"
  CLAIMENV="PROBE_PAD_CLAIM=1"
fi
daemon -f -o /tmp/frescod-t.log env FRESCOD_HEADLESS_PNG=/tmp/hl/f $SPLIT $CAPS FRESCOD_SOCK=/tmp/fresco-t.sock FRESCOD_WM_SOCK=/tmp/fresco-wm-t.sock ./frescod
sleep 8
grep -E "tactus|input" /tmp/frescod-t.log | head -5
S=$(grep -o "/[^ ]*fresco[^ ]*\.sock" /tmp/frescod-t.log | head -1); [ -S /tmp/fresco-t.sock ] && S=/tmp/fresco-t.sock
echo "  frescod socket $S"
rm -f /tmp/probe.log; daemon -f -o /tmp/probe.log env $CLAIMENV ./atrium-input-probe $S 600
sleep 5; cat /tmp/probe.log
EOF

if [ "$GAMEPAD" = 1 ]; then
  echo "== gamepad: fake Bluetooth pad -> jailed tactusd -> split frescod -> windows"
  V 'sh -s' <<'EOF'
cd /root/t
S=/tmp/fresco-t.sock
mkdir -p /atrium/sockets/bluetooth-hid; chmod 0755 /atrium/sockets/bluetooth-hid
rm -f /tmp/pad.log
(umask 0; daemon -f -o /tmp/pad.log ./fake-pad /atrium/sockets/bluetooth-hid/hid.sock 25)
# tactusd retries the socket every 2 s; then four steps 1.5 s apart.
i=0; until grep -q "pausing" /tmp/pad.log || [ $i -ge 60 ]; do sleep 1; i=$((i+1)); done
sleep 3
rm -f /tmp/probe2.log; daemon -f -o /tmp/probe2.log ./atrium-input-probe $S 600
i=0; until grep -q "detached" /tmp/pad.log || [ $i -ge 90 ]; do sleep 1; i=$((i+1)); done
sleep 3
cat /tmp/pad.log
EOF
  V 'grep -E "^pad-(connect|disconnect|button|axis|claim) " /tmp/probe.log' > /tmp/tactus-pad1.log
  V 'grep -E "^pad-(connect|disconnect|button|axis|claim) " /tmp/probe2.log' > /tmp/tactus-pad2.log
  V 'grep "^pad-std " /tmp/probe.log' > /tmp/tactus-std1.log
  V 'grep "^pad-std " /tmp/probe2.log' > /tmp/tactus-std2.log
  echo "-- window 1"; cat /tmp/tactus-pad1.log; echo "-- window 2"; cat /tmp/tactus-pad2.log
  P1=/tmp/tactus-pad1.log P2=/tmp/tactus-pad2.log
  if [ "$CLAIM" = 1 ]; then
    V 'grep "input caps" /tmp/frescod-t.log | head -3'
    grep -q 'pad-connect .*name="Fake Pad"' $P1 && ok "window 1: connect" || bad "window 1 connect"
    grep -q "pad-claim .*held=1" $P1 && ok "window 1: claim granted" || bad "window 1 claim not granted"
    grep -q "pad-button .*usage=0x00090002 pressed=1" $P1 && grep -q "pad-button .*usage=0x00090002 pressed=0" $P1 && ok "window 1: button 2 after window 2 took focus" || bad "window 1 lost the claimed pad"
    tail -1 $P1 | grep -q "pad-disconnect" && ok "window 1: disconnect on detach" || bad "window 1 detach"
    [ -s $P2 ] && bad "window 2 got pad events while window 1 held the pad" || ok "window 2: nothing while the pad is held"
    echo "tactus: $fails failed"
    exit $fails
  fi
  grep -q 'pad-connect .*id=1234:5678 buttons=15 axes=7 name="Fake Pad"' $P1 && ok "window 1: connect with counts and name" || bad "window 1 connect"
  grep -q "pad-axis .*usage=0x00010030 value=32767" $P1 && ok "window 1: stick right" || bad "window 1 stick"
  grep -q "pad-button .*usage=0x00090001 pressed=1" $P1 && grep -q "pad-button .*usage=0x00090001 pressed=0" $P1 && ok "window 1: button 1 press and release" || bad "window 1 button 1"
  grep -q "pad-axis .*usage=0x00010039 value=2" $P1 && ok "window 1: hat right" || bad "window 1 hat"
  tail -1 $P1 | grep -q "pad-disconnect" && ok "window 1: disconnect when focus moved" || bad "window 1 not disconnected last"
  grep -q "usage=0x00090002" $P1 && bad "window 1 got button 2 after losing the pad" || ok "window 1: nothing after losing the pad"
  head -1 $P2 | grep -q "pad-connect" && ok "window 2: connect first" || bad "window 2 connect"
  grep -q "pad-axis .*usage=0x00010039 value=2" $P2 && ok "window 2: snapshot has the hat" || bad "window 2 snapshot"
  grep -q "pad-button .*usage=0x00090002 pressed=1" $P2 && grep -q "pad-button .*usage=0x00090002 pressed=0" $P2 && ok "window 2: button 2" || bad "window 2 button 2"
  tail -1 $P2 | grep -q "pad-disconnect" && ok "window 2: disconnect on detach" || bad "window 2 detach"
  echo "-- standard names (pergola-gamepad), window 1"; cat /tmp/tactus-std1.log
  S1=/tmp/tactus-std1.log S2=/tmp/tactus-std2.log
  grep -q "button: South, pressed: true" $S1 && ok "std: button 1 is South (A)" || bad "std South"
  grep -q "axis: LeftX, value: 1.0" $S1 && ok "std: left stick right" || bad "std LeftX"
  grep -q "button: DpadRight, pressed: true" $S1 && grep -q "button: DpadRight, pressed: false" $S1 && ok "std: hat right holds DpadRight, released on disconnect" || bad "std DpadRight"
  grep -q "button: East, pressed: true" $S2 && ok "std: window 2 button 2 is East (B)" || bad "std East"
  V 'grep "split mode" /tmp/frescod-t.log | head -2'
  V 'jls -n name | grep -c atrium-tactusd' | sed 's/^/  tactusd jails: /'
  echo "tactus: $fails failed"
  exit $fails
fi

echo "== type and click (QEMU monitor)"
mon "mouse_set 5" "sendkey shift-a" "sendkey b" "sendkey c 1500" "mouse_move 20 10" "mouse_button 1" "mouse_button 0" "mouse_move 0 0 2"
sleep 4
if [ "$PS2" = 1 ]; then
  V 'grep -E "atkbd0|bpsm0" /var/log/atrium/atrium-tactusd.log | tail -4; grep tactus-console /var/log/messages | tail -3'
  V 'grep -q "device [0-9]* /dev/atkbd0" /var/log/atrium/atrium-tactusd.log' && ok "tactusd has atkbd0" || bad "tactusd has no atkbd0"
  V 'grep -q "device [0-9]* /dev/bpsm0" /var/log/atrium/atrium-tactusd.log' && ok "tactusd has bpsm0" || bad "tactusd has no bpsm0"
else
mon "device_add usb-kbd,id=tk2"; sleep 8
mon "sendkey d"; sleep 3
mon "device_del tk2"; sleep 5
fi
V 'cat /tmp/probe.log' > /tmp/tactus-probe.log
cat /tmp/tactus-probe.log | head -40
P=/tmp/tactus-probe.log
grep -q "usage=0x0004 pressed=1 mods=1 token" $P && ok "shift-a: a with shift, token" || bad "shift-a"
grep -q "usage=0x0004 pressed=0 mods=1 no-token" $P && ok "release without token" || bad "a release"
grep -q "usage=0x00e1" $P && bad "modifier sent as a key" || ok "modifiers only as bits"
[ "$(grep -c "usage=0x0006 pressed=1 mods=0 token$" $P)" = 1 ] && ok "held c: one press with a token" || bad "held c pressed $(grep -c 'usage=0x0006 pressed=1 mods=0 token$' $P) times"
n=$(grep -c "usage=0x0006 pressed=1 mods=0 no-token repeat" $P)
[ "$n" -ge 15 ] && [ "$n" -le 40 ] && ok "held c 1.5 s: $n repeats, no tokens" || bad "held c: $n repeats"
grep "repeat" $P | grep -qv "usage=0x0006" && bad "repeats of other keys" || ok "only the held key repeats"
grep -q "button window=[1-9][0-9]* button=1 pressed=1 token" $P && ok "click with token" || bad "click"
grep -q "^motion window=" $P && ok "motion" || bad "motion"
grep -q "^scroll window=" $P && ok "scroll" || bad "scroll"
[ "$PS2" = 1 ] || { grep -q "usage=0x0007 pressed=1" $P && ok "hot-plugged keyboard types" || bad "hot-plugged keyboard"; }

echo "== tactusd restarts: frescod waits, then gets input again"
V 'pkill -x atrium-tactusd; sleep 8; ps -axo command | grep -c "[a]trium-tactusd"; grep -c "input from tactusd" /tmp/frescod-t.log; tail -3 /tmp/frescod-t.log'
if [ "$PS2" = 1 ]; then
  V 'grep -q "back to the console" /var/log/messages' && ok "atkbd0 back to the console while tactusd was down" || bad "console did not get atkbd0 back"
  V 'grep -c "atkbd0 to tactusd" /var/log/messages' | sed 's/^/  atkbd0 handed to tactusd times: /'
  sleep 4
fi
mon "sendkey e"; sleep 3
V 'grep -q "usage=0x0008 pressed=1" /tmp/probe.log' && ok "input after tactusd restart" || bad "no input after tactusd restart"
V 'echo $(ls /var/lib/atrium/jails/tactusd/dev)' | sed 's/^/  tactusd jail \/dev: /'
V 'ls /var/lib/atrium/jails/frescod/dev 2>/dev/null | grep -c hidraw' | sed 's/^/  hidraw nodes in frescod jail devfs (needs a frescod jail restart to apply): /'
echo "tactus: $fails failed"
