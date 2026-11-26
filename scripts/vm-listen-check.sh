#!/bin/sh
# Background apps B4 (docs/spec/background-apps.md §4): an app's `listen`
# ports, in the dev VM. Runs on the HOST; ends "listen: N ok, M failed".
#
#   ARCH=amd64 sh scripts/cloud-vm.sh up      # or the Mac VM, with its OK
#   sh scripts/deploy-portcullis.sh           # once: jaild with allow_rctl
#   ARCH=amd64 sh scripts/vm-listen-check.sh
#
# The local network is simulated in the guest: an epair whose far end is a
# vnet jail `lanpeer` (192.168.77.2, "a device on the network"), its near
# end 192.168.77.1 in group atrium_egress. atrium-net is played by a socket
# at its ctl path (examples/fake_netd) that answers `status` with that
# interface as the egress, wired, its router lanpeer; the check
# edits that answer to change networks. The person's grants are written to
# their policy.toml, as Settings' switches write them (the switches need a
# real press; Curia's tests cover them). jaild is replaced with this tree's
# and the base pf rules with etc/atrium.pf.
#
#   1. lsrv (8080 from the local network, 9000 from this computer) runs:
#      with no grant, nothing reaches it from the LAN or locally, and the
#      LAN can't route to its address either;
#   2. the grant (this computer): the person gets "probe 9000" through
#      127.0.0.1; another user is refused there and directly;
#   3. the network allowed: lanpeer gets "probe 8080"; it can't reach 9000;
#      a held connection from each side is counted;
#   4. the network changes (another router): the LAN rule goes; back again:
#      it returns;
#   5. a second app asking for 8080 doesn't get it; the first keeps it;
#   6. lsrv ends: its ports and its forwarder are gone.
#
# An app jail (Navigator's engines are app jails) can't reach the port: its
# 127.0.0.1 is its own stack's, and the base rules block apps from every
# host address; vm-network checks cover that block.
set -u
ROOT=$(cd "$(dirname "$0")/.." && pwd)
export ARCH=${ARCH:-aarch64}
set -- env
. "$ROOT/scripts/cloud-vm.sh" >/dev/null
set --
. "$ROOT/scripts/lib/gate.sh"
gate_begin listen 24
K="-i $HOME/.ssh/fresco_bsd_ed25519 -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o LogLevel=ERROR"
V() { "$ROOT/scripts/vssh" "$@"; }
TGT=${CARGO_TARGET_DIR:-$ROOT/portcullis/target}

B=/var/tmp/listen
PSOCK=/atrium/sockets/portcullis/portcullis.sock
SEAT=/atrium/sockets/ostiarius/active-session
NETD=/atrium/sockets/atrium-net/ctl.sock
LOG=$B/portcullisd.log
U=lscheck; HU=/home/$U
O=lsother
APPS=/var/lib/atrium/apps
A1=org.test.lsrv; A2=org.test.lsrv2

echo "=== cross-build"
(cd "$ROOT/portcullis" && RUSTFLAGS="$CROSS_RUSTFLAGS" cargo build -q --release --target "$TARGET" \
    -p portcullisd -p portcullis-cli -p opifex -p jaild \
    --bin portcullisd --bin portcullis --bin opifex --bin atrium-jaild --example listen_probe --example fake_netd --example jaild_ask) \
    || gate_abort "cross-build failed"

echo "=== install"
V "mkdir -p $B"
for b in release/portcullisd release/portcullis release/opifex release/atrium-jaild release/examples/listen_probe release/examples/fake_netd release/examples/jaild_ask; do
    scp -q $K -P "$VM_SSH_PORT" "$TGT/$TARGET/$b" root@localhost:$B/ || gate_abort "copy $b"
done
scp -q $K -P "$VM_SSH_PORT" "$ROOT/etc/atrium.pf" root@localhost:$B/ || gate_abort "copy atrium.pf"

V 'sh -s' <<EOS
set -e
pkill -f '^/usr/local/bin/portcullisd' || true
service atrium-portcullisd-daemon onestop >/dev/null 2>&1 || true
service atrium-jaild onestop >/dev/null 2>&1 || true
sleep 1
install -m 0755 $B/atrium-jaild /usr/local/bin/atrium-jaild
install -m 0755 $B/portcullisd /usr/local/bin/portcullisd
install -m 0755 $B/portcullis /usr/local/bin/portcullis
install -m 0755 $B/opifex /usr/local/bin/opifex
cp $B/atrium.pf /usr/local/etc/atrium/pf.conf
pfctl -f /usr/local/etc/atrium/pf.conf
service atrium-jaild onestart >/dev/null
id $U >/dev/null 2>&1 || pw useradd $U -u 1078 -m -d $HU -s /bin/sh
id $O >/dev/null 2>&1 || pw useradd $O -u 1079 -m -d /home/$O -s /bin/sh
test -f /root/of-key.pem || openssl ecparam -name prime256v1 -genkey -noout -out /root/of-key.pem
mkdir -p /etc/atrium/publishers; openssl ec -in /root/of-key.pem -pubout -out /etc/atrium/publishers/of-gate.pem 2>/dev/null
mk() {
  opifex uninstall \$1 >/dev/null 2>&1 || true
  D=$B/b-\$1; mkdir -p \$D/bin; cp $B/listen_probe \$D/bin/listen_probe
  printf '[app]\nid = "%s"\nname = "%s"\nversion = "1"\nentry = "bin/listen_probe"\n\n[capabilities.network]\noutbound = "any"\nlisten = [{ port = 8080, proto = "tcp", from = "local-network" }, { port = 9000, proto = "tcp", from = "this-computer" }]\n\n[resources]\nmemory = "96M"\ncpu = 40\n' \$1 \$1 > \$D/atrium.toml
  openssl dgst -sha256 -sign /root/of-key.pem -out \$D/atrium.toml.sig \$D/atrium.toml
  opifex install \$D >/dev/null
}
mk $A1; mk $A2
# The simulated network.
jail -r lanpeer >/dev/null 2>&1 || true
for e in \$(ifconfig -l); do case \$e in epair*a) ifconfig \$e | grep -q 'description: lscheck' && ifconfig \$e destroy ;; esac; done
L=\$(ifconfig epair create); P=\${L%a}b
ifconfig \$L description lscheck
ifconfig \$L inet 192.168.77.1/24 up
ifconfig \$L group atrium_egress
jail -c name=lanpeer path=/ vnet persist vnet.interface=\$P host.hostname=lanpeer
jexec lanpeer ifconfig \$P inet 192.168.77.2/24 up
jexec lanpeer ifconfig lo0 inet 127.0.0.1/8 up
jexec lanpeer route -q add -net 100.64.0.0/16 192.168.77.1
ping -c 1 -t 2 192.168.77.2 >/dev/null
echo \$L > $B/lan.if
mkdir -p /var/db/atrium/$U; : > /var/db/atrium/$U/policy.toml
printf '%s\n' $U > $SEAT
mkdir -p /atrium/sockets/portcullis /atrium/sockets/atrium-net; rm -f $PSOCK
: > $LOG
daemon -f -o $LOG /usr/local/bin/portcullisd
i=0; while [ \$i -lt 30 ] && ! [ -S $PSOCK ]; do sleep 0.5; i=\$((i+1)); done
test -S $PSOCK; chmod 666 $PSOCK
for a in $A1 $A2; do su -m $U -c "portcullis policy grant \$a" >/dev/null; done
EOS
[ $? = 0 ] && ok "jaild, pf rules, portcullisd, the two apps and the simulated network" \
    || gate_abort "setup: $(V "tail -3 $LOG")"
LIF=$(V "cat $B/lan.if" | tr -d '\r')
GWMAC=$(V "arp -n 192.168.77.2" | awk '{for(i=1;i<NF;i++) if($i=="at") print $(i+1)}' | tr -d ':\r')
HOME_NET=wired-$GWMAC

# atrium-net's answer: the LAN interface as the egress, wired, router lanpeer.
SEQ=0; ROUTER=192.168.77.2
net() { # router: a changed answer is an event to portcullisd
    ROUTER=$1; SEQ=$((SEQ + 1))
    V "printf 'iface name=$LIF mac=x state=bound addr=192.168.77.1/24 router=$1 link=up internet=yes egress=1 seq=$SEQ\nok\n' > $B/status.new && mv $B/status.new $B/status.txt"
}
net 192.168.77.2
V "pkill -f fake_netd; daemon -f $B/fake_netd $NETD $B/status.txt"
i=0; while [ $i -lt 10 ] && ! V "test -S $NETD"; do sleep 1; i=$((i+1)); done

h() { V "echo sha256:\$(sha256 -q $APPS/$1/atrium.toml)" | tr -d '\r'; }
grant() { # app networks
    printf '[listen."%s"]\nmanifest_hash = "%s"\ngranted_at = "2026-11-26T00:00:00Z"\npersona = "personal"\nnetworks = [%s]\n\n' "$1" "$(h $1)" "$2"
}
# Settings' SetListen asks portcullisd to look again; a file edit doesn't,
# so the check pokes it with an atrium-net event.
policy() { printf '%s' "$1" | V "cat > /var/db/atrium/$U/policy.new && mv /var/db/atrium/$U/policy.new /var/db/atrium/$U/policy.toml"; net $ROUTER; echo "  (policy written $(V 'date -u +%T' | tr -d '\r'))"; }
lan() { V "jexec lanpeer nc -N -w 3 $1 $2 < /dev/null" 2>/dev/null | tr -d '\r'; }
as() { V "su -m $1 -c 'nc -N -w 3 $2 $3 < /dev/null'" 2>/dev/null | tr -d '\r'; }
jail_of() { V "jls name | grep '$(echo $1 | tr . _)\$' | head -1" | tr -d '\r'; }
app_addr() { V "pfctl -a atrium/$1 -sr 2>/dev/null" | sed -n 's/.* from \(100\.64\.[0-9.]*\) to .*/\1/p' | head -1; }
count() { V "grep -c -F '$1' $LOG" 2>/dev/null | tr -d '\r '; }
settle() { sleep 12; }

echo "=== 1. running, nothing allowed"
V ": > $B/l1.log; chmod 666 $B/l1.log; daemon -f -o $B/l1.log su -m $U -c 'portcullis launch --no-prompt $A1'"
J1=""; i=0; while [ $i -lt 60 ] && [ -z "$J1" ]; do J1=$(jail_of $A1); [ -z "$J1" ] && sleep 1; i=$((i+1)); done
[ -n "$J1" ] && ok "lsrv runs ($J1)" || gate_abort "lsrv didn't start: $(V "tail -3 $B/l1.log; tail -3 $LOG")"
sleep 3
AD=$(app_addr "$J1")
check "the LAN can't reach 8080" "" "$(lan 192.168.77.1 8080)"
case "$AD" in
    100.64.*) check "…nor route to the app's address" "" "$(lan "$AD" 8080)" ;;
    *) bad "no app address in atrium/$J1" ;;
esac
check "nothing on 127.0.0.1:9000" "" "$(as $U 127.0.0.1 9000)"
check "root on the host reaches the app's own address" "probe 8080" "$(V "nc -N -w 3 $AD 8080 < /dev/null" 2>/dev/null | tr -d '\r')"
check "…another local user doesn't, on a port it doesn't listen on for them" "" "$(as $O "$AD" 8080)"

echo "=== 2. this computer"
policy "$(grant $A1 '')"
settle
check "the person gets through 127.0.0.1:9000" "probe 9000" "$(as $U 127.0.0.1 9000)"
check "another user is refused there" "" "$(as $O 127.0.0.1 9000)"
check "…and straight at the app" "" "$(as $O "$AD" 9000)"
check "…the forwarder's refusal is logged" 1 "$(V "grep -c 'refused 127.0.0.1:9000 to uid Some(1079)' /var/log/atrium/jaild.log | awk '{print (\$1>0)}'" | tr -d '\r ')"
check "the LAN still can't reach 8080" "" "$(lan 192.168.77.1 8080)"

echo "=== 3. this network"
policy "$(grant $A1 "\"$HOME_NET\"")"
settle
R=$(lan 192.168.77.1 8080)
if [ "$R" = "probe 8080" ]; then ok "lanpeer gets probe 8080"; else
    bad "lanpeer got [$R]: $(V "pfctl -a atrium/$J1 -sn; pfctl -a atrium/$J1 -sr; pfctl -ss | grep -E '8080|9000'; jexec lanpeer nc -v -N -w 3 192.168.77.1 8080 < /dev/null 2>&1; date -u +%T; grep SetListen /var/log/atrium/jaild.log | tail -2; grep listen: $LOG | tail -6; grep -c networks /var/db/atrium/$U/policy.toml" | tr '\n' '|')"
fi
check "…not 9000" "" "$(lan 192.168.77.1 9000)"
# One held connection from the LAN and one through the forwarder: what the
# bar's "2 connections" is read from.
V "daemon -f jexec lanpeer sh -c 'sleep 8 | nc 192.168.77.1 8080 > /dev/null'; daemon -f su -m $U -c 'sleep 8 | nc 127.0.0.1 9000 > /dev/null'"
sleep 3
check "jaild counts both connections" '{"kind":"listen_stats","connections":2}' \
    "$(V "$B/jaild_ask '{\"kind\":\"listen_stats\",\"jail_name\":\"$J1\"}'" | tr -d '\r')"
sleep 6
check "…the rdr is from this network only" 1 "$(V "pfctl -a atrium/$J1 -sn" | grep -c "rdr on $LIF inet proto tcp from 192.168.77.0/24 to ($LIF) port = [a-z0-9-]* -> $AD port 8080")"

echo "=== 4. the network changes"
BEFORE=$(count "$A1 for $U: 1 port(s) now")
net 192.168.77.9
settle
check "on another network the LAN rule goes" "" "$(lan 192.168.77.1 8080)"
check "…and the log says so" $((BEFORE + 1)) "$(count "$A1 for $U: 1 port(s) now")"
net 192.168.77.2
settle
check "back on the allowed network it returns" "probe 8080" "$(lan 192.168.77.1 8080)"

echo "=== 5. a second app asking for the same port"
policy "$(grant $A1 "\"$HOME_NET\""; echo; grant $A2 "\"$HOME_NET\"")"
V ": > $B/l2.log; chmod 666 $B/l2.log; daemon -f -o $B/l2.log su -m $U -c 'portcullis launch --no-prompt $A2'"
sleep 15
J2=$(jail_of $A2)
case "$(V "cat $B/l2.log; grep -F '$A2' $LOG" | tr -d '\r')" in
    *"port 8080 is in use"*|*"port 9000 is in use"*) ok "lsrv2 is refused the port" ;;
    *) bad "lsrv2 not refused: $(V "tail -3 $B/l2.log")" ;;
esac
check "…and lsrv keeps it" "probe 8080" "$(lan 192.168.77.1 8080)"
[ -n "$J2" ] && V "pkill -9 -j $J2 listen_probe" >/dev/null 2>&1

echo "=== 6. lsrv ends"
# The app's own end, as a crash or exit: the launch removes the jail.
V "pkill -9 -j $J1 listen_probe" >/dev/null 2>&1
sleep 8
check "the LAN gets nothing" "" "$(lan 192.168.77.1 8080)"
check "nothing on 127.0.0.1:9000" "" "$(as $U 127.0.0.1 9000)"
check "…its anchor is empty" 0 "$(V "pfctl -a atrium/$J1 -sr 2>/dev/null | wc -l" | tr -d '\r ')"

echo "=== clean up"
V "pkill -f fake_netd; rm -f $NETD; jail -r lanpeer; ifconfig $LIF destroy; : > $SEAT; pkill -f '^/usr/local/bin/portcullisd'; \
   service atrium-portcullisd-daemon onestart >/dev/null 2>&1" >/dev/null 2>&1
gate_end
