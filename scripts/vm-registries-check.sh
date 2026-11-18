#!/bin/sh
# Dev-tools M8 (docs/spec/dev-tools-plan.md, dev-tools.md §5.2): the
# `registries` network mode. Runs on the HOST after
#   ARCA=amd64 sh scripts/cloud-vm.sh up
#   sh scripts/vm-registries-check.sh
# Ends "registries: N failed". MODE=2 (the default) runs the filter denying.
#
#   - a `registries` workspace's jail has only its own loopback: no route
#     out, a direct connection fails;
#   - its proxy (org.atrium.nuntius-egress) runs in a one-shot jail of its
#     own, as _egress, and holds 127.0.0.1:3128 in the workspace's stack
#     before anything runs there (the workspace can't bind it);
#   - HTTPS to a listed host works through it (fetch, as cargo or pkg would);
#   - an unlisted host, a port other than 443, a plain GET, and listed
#     names that resolve to a private address or the metadata address are
#     refused;
#   - the list is the workspace's (`--hosts`), and stopping the workspace
#     ends its proxy;
#   - an `outbound` workspace is unchanged: no proxy, a direct route.
# The proxy CA of a cloud session (CA=..., default /root/.ccr/ca-bundle.crt
# when it exists) is added to the VM's trust store for the run.
set -u
ROOT=$(cd "$(dirname "$0")/.." && pwd)
export ARCH=amd64
set -- env
. "$ROOT/scripts/cloud-vm.sh" >/dev/null
set --
K="-i $HOME/.ssh/fresco_bsd_ed25519 -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o LogLevel=ERROR"
V() { "$ROOT/scripts/vssh" "$@"; }
fails=0
ok() { echo "  ok $1"; }
bad() { echo "  FAIL $1"; fails=$((fails+1)); }
check() { if [ "$2" = "$3" ]; then ok "$1"; else bad "$1: expected [$2] got [$3]"; fi; }
MODE=${MODE:-2}
CA=${CA:-/root/.ccr/ca-bundle.crt}
HOST=${HOST:-pkg.freebsd.org}
OTHER=${OTHER:-github.com}

echo "=== cross-build"
(cd "$ROOT/portcullis" && RUSTFLAGS="$CROSS_RUSTFLAGS" cargo build -q --release --target "$TARGET" \
    -p portcullisd -p portcullis-cli -p opifex --bin portcullisd --bin portcullis --bin opifex) \
    || { echo "build failed"; echo "registries: 1 failed"; exit 1; }
(cd "$ROOT/navigator-fetch" && RUSTFLAGS="$CROSS_RUSTFLAGS" cargo build -q --release --target "$TARGET" --bin nuntius-egress) \
    || { echo "nuntius-egress build failed"; echo "registries: 1 failed"; exit 1; }

echo "=== install"
B=/var/tmp/registries
V "pkill -x portcullisd; sleep 1; rm -rf $B; mkdir -p $B; chmod 755 $B"
for b in portcullis/target/$TARGET/release/portcullisd portcullis/target/$TARGET/release/portcullis \
         portcullis/target/$TARGET/release/opifex navigator-fetch/target/$TARGET/release/nuntius-egress \
         scripts/nuntius-egress-bundle.sh etc/jaild.policy.toml; do
    scp -q $K -P 2222 "$ROOT/$b" root@localhost:$B/ || { echo "copy $b failed"; echo "registries: 1 failed"; exit 1; }
done
[ -f "$CA" ] && scp -q $K -P 2222 "$CA" root@localhost:$B/proxy-ca.pem
PSOCK=/atrium/sockets/portcullis/portcullis.sock
LOG=$B/portcullisd.log
V 'sh -s' <<EOS
set -e
for b in portcullisd portcullis opifex; do install -m 0755 $B/\$b /usr/local/bin/\$b; done
mkdir -p /etc/atrium
sed -e 's/^mode     = 1/mode     = $MODE/' $B/jaild.policy.toml > /etc/atrium/jaild.policy.toml
grep -q '^allow_rctl' /etc/atrium/jaild.policy.toml || printf '\n[resource_control]\nallow_rctl = true\n' >> /etc/atrium/jaild.policy.toml
if [ -f $B/proxy-ca.pem ]; then
    [ -f /etc/ssl/cert.pem.pkg-bak ] || cp /etc/ssl/cert.pem /etc/ssl/cert.pem.pkg-bak
    cat /etc/ssl/cert.pem.pkg-bak $B/proxy-ca.pem > /etc/ssl/cert.pem
fi
# Listed names that point inside: the proxy reads the bundle's hosts file.
[ -f /etc/hosts.reg-bak ] || cp /etc/hosts /etc/hosts.reg-bak
cp /etc/hosts.reg-bak /etc/hosts
printf '169.254.169.254 metadata.reg.test\n10.0.2.2 inside.reg.test\n' >> /etc/hosts
id alice >/dev/null 2>&1 || pw useradd alice -m -s /bin/sh
service atrium-jaild restart >/dev/null 2>&1 || service atrium-jaild start >/dev/null 2>&1 || true
test -f /root/reg-key.pem || openssl ecparam -name prime256v1 -genkey -noout -out /root/reg-key.pem
mkdir -p /etc/atrium/publishers; openssl ec -in /root/reg-key.pem -pubout -out /etc/atrium/publishers/reg-gate.pem 2>/dev/null
cd $B && PATH=/usr/local/bin:\$PATH sh nuntius-egress-bundle.sh $B/nuntius-egress /root/reg-key.pem /usr/local/bin/opifex
cp /etc/hosts.reg-bak /etc/hosts
mkdir -p /atrium/sockets/portcullis; rm -f $PSOCK
: > $LOG
daemon -f -o $LOG /usr/local/bin/portcullisd --socket $PSOCK
sleep 3; test -S $PSOCK; chmod 666 $PSOCK
EOS
[ $? = 0 ] && ok "portcullisd and the proxy's bundle installed (filter mode $MODE)" || { bad "setup: $(V "tail -3 $LOG")"; echo "registries: $fails failed"; exit 1; }

A() { V "su -m alice -c 'cd /tmp; portcullis workspace $1'" 2>&1; }
wx() { A "exec $1 -- /bin/sh -c \"$2\""; }
J=ws_alice_reg
for w in reg scratch; do A "stop $w" >/dev/null; done
A "new reg --network registries" >/dev/null
A "set reg --network registries --hosts $HOST,metadata.reg.test,inside.reg.test,localhost" >/dev/null
case "$(A list)" in *reg*registries*) ok "reg is a registries workspace" ;; *) bad "list: $(A list | tr '\n' ' ')" ;; esac

echo "=== the jails"
out=$(wx reg 'echo started')
case "$out" in started*) ok "reg is up" ;; *) bad "exec: $out" ;; esac
check "reg: the filter's mode" "$MODE" "$(V "jls -j $J sysfil.mode")"
check "reg: only its loopback" "lo0" "$(V "jexec $J ifconfig -l")"
check "reg: no route out" "none" "$(V "jexec $J route -n get default >/dev/null 2>&1 && echo route || echo none")"
EJ=$(V "jls name | grep -i egress" | head -1)
[ -n "$EJ" ] && ok "the proxy's own jail: $EJ" || bad "no proxy jail: $(V "tail -5 $LOG")"
check "the proxy runs as _egress" "_egress" "$(V "ps -axo user,comm | awk '\$2==\"nuntius-egress\" {print \$1}'" | head -1)"
check "the proxy isn't in the workspace's jail" "" "$(V "ps -J $J -o comm= 2>/dev/null | grep nuntius")"
out=$(wx reg 'env | grep -c "^HTTPS_PROXY=http://127.0.0.1:3128\$"')
check "HTTPS_PROXY in the workspace" "1" "$out"
# A plain bind (nc -l sets options the filter refuses): 3129 is free,
# 3128 is the proxy's.
scp -q $K -P 2222 "$ROOT/scripts/probes/bindtest.c" root@localhost:$B/
V "cc -o $B/bindtest $B/bindtest.c && cp $B/bindtest \$(jls -j $J path)/tmp/bindtest && chmod 755 \$(jls -j $J path)/tmp/bindtest"
out=$(wx reg '/tmp/bindtest 3129; /tmp/bindtest 3128')
case "$out" in *"3129: ok"*"3128: Address already in use"*) ok "the workspace can't take 3128 (3129 binds)" ;; *) bad "bind: $out" ;; esac

echo "=== through the proxy"
out=$(wx reg "fetch -q -o /dev/null https://$HOST/ 2>&1; echo rc=\\\$?")
case "$out" in *rc=0*) ok "https://$HOST/ fetched through the proxy" ;; *) bad "listed host: $out" ;; esac
out=$(wx reg "env -i PATH=/bin:/usr/bin fetch -T 10 -q -o /dev/null https://$HOST/ 2>&1; echo rc=\\\$?")
case "$out" in *rc=0*) bad "a direct fetch worked: $out" ;; *) ok "a direct fetch fails (no route)" ;; esac
# A request head to the proxy from inside the workspace's stack.
P() { printf '%s\r\n\r\n' "$1" | V "jexec $J nc -N -w 15 127.0.0.1 3128 | head -1"; }
case "$(P "CONNECT $OTHER:443 HTTP/1.1")" in *403*) ok "an unlisted host: 403" ;; *) bad "unlisted: $(P "CONNECT $OTHER:443 HTTP/1.1")" ;; esac
case "$(P "CONNECT $HOST:80 HTTP/1.1")" in *403*) ok "port 80: 403" ;; *) bad "port 80 admitted" ;; esac
case "$(P "GET http://$HOST/ HTTP/1.1")" in *405*) ok "a plain GET: 405" ;; *) bad "GET admitted" ;; esac
case "$(P "CONNECT metadata.reg.test:443 HTTP/1.1")" in *403*) ok "a listed name at the metadata address: 403" ;; *) bad "metadata admitted" ;; esac
case "$(P "CONNECT inside.reg.test:443 HTTP/1.1")" in *403*) ok "a listed name at a private address: 403" ;; *) bad "private admitted" ;; esac
case "$(P "CONNECT localhost:443 HTTP/1.1")" in *403*) ok "a listed localhost: 403" ;; *) bad "localhost admitted" ;; esac
case "$(P "CONNECT $HOST:443 HTTP/1.1")" in *200*) ok "CONNECT $HOST:443: 200" ;; *) bad "listed CONNECT: $(P "CONNECT $HOST:443 HTTP/1.1")" ;; esac
EL=$(V "grep -c '^egress: refused' $LOG")
[ "${EL:-0}" -ge 6 ] && ok "each refusal logged ($EL)" || bad "refusals logged: $EL"

echo "=== the list is the workspace's"
A "set reg --hosts $OTHER" >/dev/null
out=$(wx reg "fetch -q -o /dev/null https://$HOST/ 2>&1; echo rc=\\\$?")
case "$out" in *rc=0*) bad "$HOST still reachable after it was taken off" ;; *) ok "$HOST refused once off the list" ;; esac
A "stop reg" >/dev/null
check "stopping the workspace ends its proxy" "" "$(V "jls name | grep -i egress; pgrep -x nuntius-egress")"

echo "=== outbound is unchanged"
out=$(wx scratch 'env | grep -c PROXY=http://127.0.0.1')
check "Scratch has no proxy" "0" "$out"
out=$(wx scratch "fetch -q -o /dev/null https://$OTHER/ 2>&1; echo rc=\\\$?")
case "$out" in *rc=0*) ok "Scratch fetches $OTHER directly" ;; *) bad "outbound: $out" ;; esac
check "no proxy for Scratch" "" "$(V "pgrep -x nuntius-egress")"

if [ "$MODE" = 2 ]; then
    A "set reg --hosts $HOST" >/dev/null
    wx reg "fetch -q -o /dev/null https://$HOST/" >/dev/null
    check "reg was denied nothing" "" "$(V "jls -j $J sysfil.seen.syscalls sysfil.seen.ioctls sysfil.seen.sockopts sysfil.seen.families" | tr ' ,' '\n\n' | grep -v '^kldload$' | grep -v '^\"\"$' | tr -d '\n')"
fi

for w in reg scratch; do A "stop $w" >/dev/null; done
V "[ -f /etc/ssl/cert.pem.pkg-bak ] && cp /etc/ssl/cert.pem.pkg-bak /etc/ssl/cert.pem; true"
echo "registries: $fails failed"
[ $fails = 0 ]
