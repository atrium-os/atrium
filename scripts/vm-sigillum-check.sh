#!/bin/sh
# Sigillum S2 gate (docs/spec/sigillum.md §16.2): a USB security key through
# a per-key driver jail and the jailed broker, end to end. Runs on the HOST
# of the cloud amd64 VM (scripts/cloud-vm.sh), started with QEMU's emulated
# U2F key (the atrium-os/qemu fork built against libu2f-emu, with
# scripts/qemu-patches applied; see scripts/build-qemu.sh) and a keyboard:
#
#   ARCH=amd64 QEMU_BIN=~/qemu-atrium/build/qemu-system-x86_64 \
#     QEMU_EXTRA="-device qemu-xhci -device usb-kbd -device u2f-emulated" \
#     sh scripts/cloud-vm.sh up
#   sh scripts/vm-sigillum-check.sh [TACTUSD]
#
# It cross-builds sigillumd, sigillum-fido, the sigillum-probe example and
# atrium-dispensatord, commissions them (scripts/sigillum-service.sh, which
# installs Sigillum's devices.d rule), restarts dispensatord so it finds the
# key, and checks:
#
#   - the key gets a jail of its own, atrium-sigillum-fido-N, whose /dev
#     holds that key's one node (u2f/M when FreeBSD's u2f(4) drives it,
#     else hidrawN), owned _sigillum-dev, 0600, and whose driver runs as
#     _sigillum-dev with no network;
#   - with TACTUSD (an x86_64-unknown-freebsd tactusd), tactusd is
#     commissioned too, and its jail's /dev has the keyboard's hidraw but
#     not the key's;
#   - sigillumd, started by portcullisd-bootstrap in its jail as _sigillum,
#     takes the driver's Hello;
#   - a client the launch registry names as the web broker app registers a
#     credential and signs in with it (the probe checks the RP ID hash and
#     the ES256 signature); the scripted trusted UI runs as _sigillum-ui;
#   - the same client, as a page through sigillum-webauthn (S3), runs
#     navigator.credentials.create() and .get() for example.com from
#     https://login.example.com, verified as a relying party would, and a
#     foreign RP ID is SecurityError without reaching sigillumd;
#   - an unknown uid and a wrong-uid UI are refused;
#   - a key hot-plugged through the QEMU monitor gets a jail from the devd
#     path, and unplugging it removes that jail (and only that one).
#
# Ends "sigillum: N failed".
set -u
TD=${1:-}
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

V true >/dev/null 2>&1 || { echo "the VM does not answer over ssh (scripts/cloud-vm.sh up)"; echo "sigillum: 1 failed"; exit 1; }

echo "== cross-build"
# (cloud-vm.sh reads its verb from $1, so source it with "env" as $1.)
CROSS_RUSTFLAGS=$(cd "$ROOT" && ARCH=amd64 sh -c '. ./scripts/cloud-vm.sh >/dev/null; printf %s "$CROSS_RUSTFLAGS"' sh env)
( cd "$ROOT/sigillum" && RUSTFLAGS="$CROSS_RUSTFLAGS" cargo build -q --release --target $TRIPLE \
    -p sigillumd -p sigillum-fido --bins --example sigillum-probe ) || { echo "build failed"; exit 1; }
B=$ROOT/sigillum/target/$TRIPLE/release
( cd "$ROOT/dispensator" && RUSTFLAGS="$CROSS_RUSTFLAGS" cargo build -q --release --target $TRIPLE ) || { echo "dispensatord build failed"; exit 1; }

echo "== copy"
V 'mkdir -p /root/s/scripts /root/s/sigillum/etc/services.d /root/s/tactus/etc/services.d /root/s/sigillum/etc/devices.d /root/s/tactus/etc/devices.d'
scp -q $K -P 2222 "$B/sigillumd" root@localhost:/root/s/sigillumd
scp -q $K -P 2222 "$B/sigillum-fido" root@localhost:/root/s/sigillum-fido
scp -q $K -P 2222 "$B/examples/sigillum-probe" root@localhost:/root/s/sigillum-probe
scp -q $K -P 2222 "$ROOT/scripts/sigillum-service.sh" "$ROOT/scripts/tactus-service.sh" root@localhost:/root/s/scripts/
scp -q $K -P 2222 "$ROOT/sigillum/etc/services.d/37-atrium-sigillumd.toml" root@localhost:/root/s/sigillum/etc/services.d/
scp -q $K -P 2222 "$ROOT/tactus/etc/services.d/38-atrium-tactusd.toml" root@localhost:/root/s/tactus/etc/services.d/
scp -q $K -P 2222 "$ROOT/sigillum/etc/devices.d/40-sigillum.toml" root@localhost:/root/s/sigillum/etc/devices.d/
scp -q $K -P 2222 "$ROOT/tactus/etc/devices.d/30-tactus.toml" root@localhost:/root/s/tactus/etc/devices.d/
scp -q $K -P 2222 "$ROOT/dispensator/target/$TRIPLE/release/atrium-dispensatord" root@localhost:/root/s/atrium-dispensatord
scp -q $K -P 2222 "$ROOT/dispensator/etc/atrium-dispensatord" root@localhost:/root/s/atrium-dispensatord.rc
[ -n "$TD" ] && scp -q $K -P 2222 "$TD" root@localhost:/root/s/tactusd

echo "== HID, rulesets, policy"
V 'sh -s' <<'EOF'
kldload -n hidraw
sysctl -n hw.usb.usbhid.enable | sed 's/^/  hw.usb.usbhid.enable=/'
ls /dev | grep hidraw | tr '\n' ' ' | sed 's/^/  nodes: /'; echo
devinfo -v | grep -E "^ *(hidbus|hidraw|ukbd|hkbd|hms|uhid)[0-9]" | sed 's/^/  /'
devfs rule -s 27 show >/dev/null 2>&1 && [ -n "$(devfs rule -s 27 show)" ] && echo "  ok ruleset 27 loaded" || echo "  FAIL ruleset 27 not loaded"
devfs rule -s 26 show | grep -q "hidraw.*unhide" && echo "  FAIL ruleset 26 still unhides hidraw" || echo "  ok ruleset 26 hides hidraw"
grep -q 'atrium-sigillum-fido' /etc/atrium/jaild.policy.toml && echo "  ok policy pins ruleset 27" || echo "  FAIL policy has no sigillum-fido pin"
grep -q '^\[sysfil.profiles.device\]' /etc/atrium/jaild.policy.toml && echo "  ok policy has the device profile" || echo "  FAIL no device profile"
EOF

echo "== commission"
V "TD=${TD:+/root/s/tactusd} sh -s" <<'EOF'
cd /root/s
pkill -f "services-dir /tmp/ssvc"; pkill -f atrium-sigillumd; pkill -f sigillum-probe; sleep 1
sh scripts/sigillum-service.sh /root/s/sigillumd /root/s/sigillum-fido | tail -1
# The probe client: uid 65099, named as Navigator's web broker app.
id _sprobe >/dev/null 2>&1 || pw useradd _sprobe -u 65099 -d /nonexistent -s /usr/sbin/nologin -c "sigillum probe"
mkdir -p /var/run/atrium; touch /var/run/atrium/app-registry
grep -q '^65099 ' /var/run/atrium/app-registry || echo "65099 root org.atrium.navigator 1111111111111111111111111111111111111111111111111111111111111111" >> /var/run/atrium/app-registry
rm -rf /tmp/ssvc; mkdir -p /tmp/ssvc
cp /etc/atrium/services.d/37-atrium-sigillumd.toml /tmp/ssvc/
printf '\n[[exec.env]]\nkey   = "SIGILLUM_WEB_BROKER"\nvalue = "org.atrium.navigator 1111111111111111111111111111111111111111111111111111111111111111"\n' >> /tmp/ssvc/37-atrium-sigillumd.toml
if [ -n "$TD" ]; then
  sh scripts/tactus-service.sh "$TD" | tail -1
  cp /etc/atrium/services.d/38-atrium-tactusd.toml /tmp/ssvc/
fi
service atrium-jaild restart >/dev/null 2>&1; sleep 2
rm -f /var/log/atrium/ssvc-bootstrap.log
daemon -f -o /var/log/atrium/ssvc-bootstrap.log /usr/local/bin/atrium-portcullisd-bootstrap --socket /var/run/atrium/jaild.sock --services-dir /tmp/ssvc
sleep 12
jls -n name | grep -q "name=atrium-sigillumd$" && echo "  ok atrium-sigillumd jail up" || { echo "  FAIL no sigillumd jail"; tail -8 /var/log/atrium/ssvc-bootstrap.log; }
[ -n "$TD" ] && { jls -n name | grep -q "name=atrium-tactusd$" && echo "  ok atrium-tactusd jail up" || echo "  FAIL no tactusd jail"; }
ps -axo user,jid,command | grep "[a]trium-sigillumd" | head -2 | sed 's/^/  /'
ls -l /atrium/sockets/sigillum /atrium/sockets/sigillum-dev /atrium/sockets/sigillum-ui | sed 's/^/  /'
# dispensatord reads devinfo at start: (re)install and restart it. The old
# agent in atrium-portcullisd-daemon is gone; restart that too so an old
# binary cannot race dispensatord for the same jails.
service atrium-dispensatord stop >/dev/null 2>&1
install -m 0755 /root/s/atrium-dispensatord /usr/local/bin/atrium-dispensatord
install -m 0555 /root/s/atrium-dispensatord.rc /usr/local/etc/rc.d/atrium-dispensatord
sysrc atrium_dispensatord_enable=YES >/dev/null
/usr/local/bin/atrium-dispensatord --check | sed 's/^/  rules: /'
service atrium-portcullisd-daemon restart >/dev/null 2>&1
service atrium-dispensatord start >/dev/null 2>&1
sleep 10
pgrep -qf atrium-dispensatord && echo "  ok dispensatord running" || { echo "  FAIL dispensatord not running"; tail -5 /var/log/atrium/dispensatord.log; }
EOF

echo "== the key's driver jail"
V 'sh -s' > /tmp/sigillum-jail.log <<'EOF'
J=$(jls -n name | sed -n 's/^name=\(atrium-sigillum-fido-[0-9]*\)$/\1/p' | head -1)
echo "jail $J"
[ -n "$J" ] || exit 0
N=${J##*-}
R=/var/lib/atrium/jails/sigillum-fido-$N/dev
# Every device node the jail can see, minus the basics every jail gets.
echo "devs $(cd $R && find . ! -type d | sed 's|^\./||' | grep -vxE 'null|zero|random|urandom' | tr '\n' ' ')"
D=$(ps -axww -o command | sed -n 's/^atrium-sigillum-fido --dev \/dev\/\([^ ]*\) .*/\1/p' | head -1)
echo "drives $D"
ls -l $R/$D | awk '{print "node " $1 " " $3}'
echo "proc $(ps -axo user,jid,command | grep '[a]trium-sigillum-fido' | head -1)"
echo "vnet $(jls -j $J ip4 2>/dev/null)"
EOF
cat /tmp/sigillum-jail.log | sed 's/^/  /'
J=$(sed -n 's/^jail //p' /tmp/sigillum-jail.log)
[ -n "$J" ] && ok "key jail $J" || bad "no atrium-sigillum-fido jail (dispensatord log: $(V 'tail -3 /var/log/atrium/dispensatord.log'))"
N=${J##*-}
D=$(sed -n 's/^drives //p' /tmp/sigillum-jail.log)
# u2f(4) keys (FreeBSD 15+) are driven through u2f/M, others through hidrawN.
case "$D" in u2f/[0-9]*|hidraw$N) ok "driven through /dev/$D" ;; *) bad "driver's node: '$D'" ;; esac
[ -n "$D" ] && [ "$(sed -n 's/^devs //p' /tmp/sigillum-jail.log)" = "$D " ] && ok "its /dev holds $D only" || bad "its /dev: $(sed -n 's/^devs //p' /tmp/sigillum-jail.log)"
grep -q "^node crw------- _sigillum-dev" /tmp/sigillum-jail.log && ok "node is _sigillum-dev 0600" || bad "node: $(sed -n 's/^node //p' /tmp/sigillum-jail.log)"
grep -q "^proc _sigillum-dev " /tmp/sigillum-jail.log && ok "driver runs as _sigillum-dev" || bad "driver process: $(sed -n 's/^proc //p' /tmp/sigillum-jail.log)"
grep -q "^vnet disable" /tmp/sigillum-jail.log && ok "no network (ip4=disable)" || bad "ip4: $(sed -n 's/^vnet //p' /tmp/sigillum-jail.log)"

if [ -n "$TD" ]; then
  echo "== tactusd's /dev"
  T=$(V 'ls /var/lib/atrium/jails/tactusd/dev | grep hidraw | tr "\n" " "')
  echo "  tactusd hidraw: $T"
  [ -n "$N" ] && echo " $T" | grep -q " hidraw$N " && bad "tactusd sees the key's hidraw$N" || ok "tactusd does not see the key"
  [ -n "$T" ] && ok "tactusd sees the keyboard" || bad "tactusd sees no hidraw"
fi

echo "== ceremonies"
V 'sh -s' > /tmp/sigillum-cer.log 2>&1 <<'EOF'
cd /root/s
install -m 0755 sigillum-probe /usr/local/bin/sigillum-probe-test
pkill -f "sigillum-probe-test ui"; sleep 1
rm -f /tmp/sprobe-ui.log /tmp/sprobe.cred
# A UI under the wrong uid is turned away (root may connect, sigillumd closes).
timeout 5 /usr/local/bin/sigillum-probe-test ui > /tmp/sprobe-wrongui.log 2>&1
echo "wrong-ui $(tail -1 /tmp/sprobe-wrongui.log)"
daemon -f -o /tmp/sprobe-ui.log -u _sigillum-ui /usr/local/bin/sigillum-probe-test ui
sleep 2
touch /tmp/sprobe.cred; chown _sprobe /tmp/sprobe.cred
su -m _sprobe -c "timeout 120 /usr/local/bin/sigillum-probe-test register example.com /tmp/sprobe.cred"; echo "register-rc $?"
su -m _sprobe -c "timeout 120 /usr/local/bin/sigillum-probe-test assert example.com /tmp/sprobe.cred"; echo "assert-rc $?"
# S3: the page's side through sigillum-webauthn (create + get from
# https://login.example.com for example.com), checked as an RP would.
su -m _sprobe -c "timeout 240 /usr/local/bin/sigillum-probe-test webauthn https://login.example.com example.com"; echo "webauthn-rc $?"
su -m nobody -c "timeout 30 /usr/local/bin/sigillum-probe-test register example.com /tmp/sprobe-x.cred"; echo "unknown-rc $?"
echo "-- ui"; cat /tmp/sprobe-ui.log
echo "-- sigillumd"; tail -12 /var/log/atrium/atrium-sigillumd.log 2>/dev/null
EOF
cat /tmp/sigillum-cer.log | sed 's/^/  /'
C=/tmp/sigillum-cer.log
grep -q "^wrong-ui ui: closed" $C && ok "a UI under another uid is closed at once" || bad "wrong-uid UI: $(sed -n 's/^wrong-ui //p' $C)"
grep -q "^register: ok .*rp_hash_ok=true" $C && grep -q "^register-rc 0" $C && ok "register: credential for example.com" || bad "register"
grep -q "^assert: cred_match=true signature_ok=true" $C && grep -q "^assert-rc 0" $C && ok "assert: signature verifies" || bad "assert"
grep -q "^webauthn create: ok=true" $C && grep -q "^webauthn get: ok=true" $C && grep -q "^webauthn-rc 0" $C && ok "webauthn: create + get through sigillum-webauthn verify as an RP" || bad "webauthn"
grep -q "^webauthn foreign rp: security_error=true" $C && ok "webauthn: a foreign RP ID is SecurityError" || bad "webauthn foreign RP"
grep -q "^unknown-rc 0" $C && bad "an unknown uid registered" || ok "an unknown uid is refused"
grep -q "Show" $C && ok "the UI was shown the sheets" || bad "the UI saw no sheet"

echo "== hot-plug and unplug a second key (devd path)"
before=$(V 'jls -n name | grep -c "^name=atrium-sigillum-fido-"')
mon "device_add u2f-emulated,id=fido9"; sleep 10
J2=$(V "jls -n name | sed -n 's/^name=\\(atrium-sigillum-fido-[0-9]*\\)\$/\\1/p' | grep -vx '$J' | head -1")
[ -n "$J2" ] && ok "hot-plugged key got $J2" || bad "no jail for the hot-plugged key ($before before)"
mon "device_del fido9"; sleep 8
[ -n "$J2" ] && { V "jls -n name | grep -qx 'name=$J2'" && bad "$J2 still there after unplug" || ok "unplug removed $J2"; }
V "jls -n name | grep -qx 'name=$J'" && ok "$J untouched" || bad "$J went away too"
echo "sigillum: $fails failed"
exit $fails
