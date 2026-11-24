set -eu
D=/tmp/i3-net
cd "$D"
id _crypta-key >/dev/null 2>&1 || pw useradd _crypta-key -u 65052 -d /nonexistent -s /usr/sbin/nologin
id _atrium-lease >/dev/null 2>&1 || pw useradd _atrium-lease -u 65034 -d /nonexistent -s /usr/sbin/nologin
id _crypta-key
service atrium-jaild onestop >/dev/null 2>&1 || true
install -m755 atrium-jaild /usr/local/bin/
install -m644 jaild.policy.toml /etc/atrium/jaild.policy.toml
if ! grep -q '^\[resource_control\]' /etc/atrium/jaild.policy.toml; then
    printf '\n[resource_control]\nallow_rctl = true\n' >>/etc/atrium/jaild.policy.toml
fi
service atrium-jaild onestart
install -m755 atrium-cryptad atrium-crypta-ac /usr/local/bin/
kldload -n pf
ifconfig vtnet0 group atrium_egress
pfctl -f "$D/atrium.pf"
pfctl -e >/dev/null 2>&1 || true
mkdir -p /var/empty /var/run/atrium
test -S /tmp/i3-net/net.sock || daemon -f -o "$D/net.log" "$D/atrium-net" -v --iface vtnet0 --capsicum --no-probe --no-unbound --no-mdns --lease "$D/atrium-lease" --sock "$D/net.sock"
for i in $(jot 60); do test -S "$D/net.sock" && break; sleep .25; done
test -S "$D/net.sock"
# With --no-unbound atrium-net leaves /etc/resolv.conf without a server;
# give it QEMU user networking's resolver back so couriers can resolve a
# real host (the fixture hosts are in /etc/hosts either way).
for i in $(jot 20); do grep -q '^nameserver' /etc/resolv.conf && break; sleep .25; done
grep -q '^nameserver' /etc/resolv.conf || echo 'nameserver 10.0.2.3' >> /etc/resolv.conf
daemon -f -o "$D/crypta.log" /usr/local/bin/atrium-cryptad -v --state "$D/state" --net-sock "$D/net.sock" --sock "$D/crypta.sock" --arca /atrium/sockets/arca/arcad.sock
for i in $(jot 20); do test -S "$D/crypta.sock" && break; sleep .25; done
test -S "$D/crypta.sock"
cat > "$D/ctl" <<'EOF'
#!/bin/sh
exec env CRYPTA_SOCK=/tmp/i3-net/crypta.sock /usr/local/bin/atrium-cryptad ctl "$@"
EOF
chmod 755 "$D/ctl"
ca=$(openssl x509 -in /tmp/i3-vpn/ca.pem -outform DER | openssl base64 -A)
"$D/ctl" create-ac name=win host=sso.crypta.test:447 ca="$ca" scoped=0
