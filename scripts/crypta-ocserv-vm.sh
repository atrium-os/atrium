#!/bin/sh
# ocserv in the Ubuntu VM of crypta-wg-linux-vm.sh, as the AnyConnect gateway
# for Crypta V4 (docs/spec/vpn.md §11). ocserv is GPL: a test tool in the VM
# only, never shipped (LICENSING-POLICY.md). Its packages come from the
# host's apt (same Ubuntu release) over the VM's HTTP side door.
#
#   sh scripts/crypta-ocserv-vm.sh up       install, certificates, four gateways, host forwards
#   sh scripts/crypta-ocserv-vm.sh totp     the OTP user's current code
#   sh scripts/crypta-ocserv-vm.sh down     stop the gateways and forwards
#
# Four ocserv instances, each its own port, network and auth:
#   :443 password   (alice / alice-pw, ann / ann-pw; groups Staff, Eng)  10.98.0.0/24
#   :444 password + TOTP (otto / otto-pw, then a code)                   10.98.1.0/24
#   :445 client certificate (carol, signed by the gate CA)               10.98.2.0/24
#   :446 password, pushing what Atrium must refuse (hostile)             10.98.3.0/24
# The VM's ports are forwarded to the host at 127.0.0.1:8443-8446 (ssh -L),
# where atrium-net/sim/lan.py's TCP relay sends the guest's connections to
# 198.51.100.52:443-446. The gate CA, the server and carol's key and
# certificate are in $D/pki on the host.
set -eu
ROOT=$(cd "$(dirname "$0")/.." && pwd)
D=${CRYPTA_LINUX_DIR:-$HOME/crypta-linux-vm}
SSH_PORT=${CRYPTA_LINUX_SSH:-2224}
HTTP_PORT=${CRYPTA_LINUX_HTTP:-8771}
KEY=$HOME/.ssh/fresco_bsd_ed25519
P=$D/pki
# A TOTP secret (RFC 6238 test key), hex.
OTP_HEX=3132333435363738393031323334353637383930
SSHO="-i $KEY -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o LogLevel=ERROR -o ConnectTimeout=60 -p $SSH_PORT"

gssh() { printf '%s\n' "$*" | ssh $SSHO ubuntu@localhost "sudo sh -s"; }

pki() {
    mkdir -p "$P"
    [ -f "$P/ca.pem" ] && return 0
    (cd "$P"
    openssl req -x509 -newkey ec -pkeyopt ec_paramgen_curve:P-256 -nodes -keyout ca.key -out ca.pem -days 3650 -subj "/CN=Crypta gate CA" 2>/dev/null
    openssl req -newkey ec -pkeyopt ec_paramgen_curve:P-256 -nodes -keyout server.key -out server.csr -subj "/CN=gw.crypta.test" 2>/dev/null
    printf 'subjectAltName=DNS:gw.crypta.test,IP:198.51.100.52\nextendedKeyUsage=serverAuth\n' > server.ext
    openssl x509 -req -in server.csr -CA ca.pem -CAkey ca.key -CAcreateserial -out server.pem -days 3650 -extfile server.ext 2>/dev/null
    # carol's client certificate (the V4 gate can also sign one Arca made).
    openssl req -newkey ec -pkeyopt ec_paramgen_curve:P-256 -nodes -keyout carol.key -out carol.csr -subj "/CN=carol/UID=carol" 2>/dev/null
    printf 'extendedKeyUsage=clientAuth\n' > client.ext
    openssl x509 -req -in carol.csr -CA ca.pem -CAkey ca.key -CAcreateserial -out carol.pem -days 3650 -extfile client.ext 2>/dev/null
    # A self-signed gateway certificate, for the pinned profile.
    openssl req -x509 -newkey ec -pkeyopt ec_paramgen_curve:P-256 -nodes -keyout self.key -out self.pem -days 3650 -subj "/CN=self.crypta.test" 2>/dev/null)
}

# One instance's config: port, n (network 10.98.n.0/24), auth line, extra lines.
conf() {
    cat <<EOF
auth = "$3"
tcp-port = $1
socket-file = /run/ocserv-$1.socket
occtl-socket-file = /run/occtl-$1.socket
pid-file = /run/ocserv-$1.pid
server-cert = /etc/ocserv/server.pem
server-key = /etc/ocserv/server.key
ca-cert = /etc/ocserv/ca.pem
run-as-user = nobody
run-as-group = nogroup
isolate-workers = false
max-clients = 16
max-same-clients = 4
keepalive = 30
dpd = 20
mobile-dpd = 20
try-mtu-discovery = false
cert-user-oid = 2.5.4.3
tls-priorities = "NORMAL:%SERVER_PRECEDENCE:-VERS-SSL3.0:-VERS-TLS1.0:-VERS-TLS1.1"
auth-timeout = 240
min-reauth-time = 1
max-ban-score = 0
cookie-timeout = 300
deny-roaming = false
rekey-time = 172800
rekey-method = ssl
use-occtl = true
device = vpns$2-
predictable-ips = true
ipv4-network = 10.98.$2.0
ipv4-netmask = 255.255.255.0
dns = 10.98.$2.1
default-domain = corp.crypta.test
route = 10.98.$2.0/255.255.255.0
route = 203.0.113.0/255.255.255.0
cisco-client-compat = true
dtls-legacy = false
banner = "Crypta gate gateway $1"
EOF
    shift 3
    for l in "$@"; do echo "$l"; done
}

up() {
    pki
    mkdir -p "$D/www/oc"
    (cd "$D/www/oc" && [ -f ocserv_*.deb ] 2>/dev/null || apt-get download ocserv libev4t64 libhttp-parser2.9 libmaxminddb0 libnl-3-200 libnl-route-3-200 liboath0t64 libprotobuf-c1 libradcli4 libtalloc2 ssl-cert)
    (cd "$D/www/oc" && for f in *%3a*; do [ -e "$f" ] && mv "$f" "$(echo "$f" | sed "s/%3a/_/")"; done; ls *.deb > list.txt)
    pkill -f "http.server $HTTP_PORT" 2>/dev/null || true
    (cd "$D/www" && nohup python3 -m http.server "$HTTP_PORT" --bind 127.0.0.1 >"$D/http.log" 2>&1 &)
    sleep 1
    gssh "set -e
        if ! command -v ocserv >/dev/null; then
            mkdir -p /root/oc && cd /root/oc
            for f in \$(curl -fsS http://10.0.2.2:$HTTP_PORT/oc/list.txt); do curl -fsS -o \$f http://10.0.2.2:$HTTP_PORT/oc/\$f; done
            DEBIAN_FRONTEND=noninteractive dpkg -i --force-depends *.deb >/dev/null 2>&1 || true
            systemctl disable --now ocserv.service ocserv.socket >/dev/null 2>&1 || true
        fi
        ocserv --version | head -1"
    for f in ca.pem server.pem server.key; do
        scp -q -P "$SSH_PORT" -i "$KEY" -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o LogLevel=ERROR "$P/$f" ubuntu@localhost:/tmp/$f
    done
    pw() { openssl passwd -6 "$1"; }
    C443=$(conf 443 0 "plain[passwd=/etc/ocserv/passwd]" 'select-group = Staff' 'select-group = Eng' 'auto-select-group = false')
    C444=$(conf 444 1 "plain[passwd=/etc/ocserv/passwd,otp=/etc/ocserv/otp]")
    C445=$(conf 445 2 "certificate")
    # Hostile: a route over loopback, a DNS server in the app range, a
    # domain that is Atrium's own, and a proxy (ignored, never followed).
    C446=$(conf 446 3 "plain[passwd=/etc/ocserv/passwd]" 'route = 127.0.0.0/255.0.0.0' 'dns = 100.64.0.1' 'split-dns = localhost' 'proxy-url = http://10.98.3.1:3128/')
    gssh "set -e
        mkdir -p /etc/ocserv
        mv /tmp/ca.pem /tmp/server.pem /tmp/server.key /etc/ocserv/
        chmod 600 /etc/ocserv/server.key
        printf '%s\n' 'alice:Staff,Eng:$(pw alice-pw)' 'ann:Staff:$(pw ann-pw)' 'otto:*:$(pw otto-pw)' > /etc/ocserv/passwd
        printf 'HOTP/T30/6 otto - $OTP_HEX\n' > /etc/ocserv/otp
        chmod 600 /etc/ocserv/otp /etc/ocserv/passwd
        cat > /etc/ocserv/g443.conf <<'EOF'
$C443
EOF
        cat > /etc/ocserv/g444.conf <<'EOF'
$C444
EOF
        cat > /etc/ocserv/g445.conf <<'EOF'
$C445
EOF
        cat > /etc/ocserv/g446.conf <<'EOF'
$C446
EOF
        sysctl -qw net.ipv4.ip_forward=1
        pkill -x ocserv-main || true; pkill -x ocserv || true; sleep 1
        for g in 443 444 445 446; do ocserv -c /etc/ocserv/g\$g.conf; done
        # HTTP the tunnels reach: on every address, and on 203.0.113.80.
        ip addr add 203.0.113.80/32 dev lo 2>/dev/null || true
        pgrep -f 'http.server 80' >/dev/null || (mkdir -p /root/www && echo crypta-ac-ok > /root/www/index.html && cd /root/www && nohup python3 -m http.server 80 >/dev/null 2>&1 &)
        sleep 2; ss -ltn | grep -E ':44[3-6] ' | wc -l"
    pkill -f "ssh.*-L 127.0.0.1:8443" 2>/dev/null || true
    ssh $SSHO -f -N -o ExitOnForwardFailure=yes -L 127.0.0.1:8443:127.0.0.1:443 -L 127.0.0.1:8444:127.0.0.1:444 -L 127.0.0.1:8445:127.0.0.1:445 -L 127.0.0.1:8446:127.0.0.1:446 ubuntu@localhost
    echo "crypta-ocserv: up (CA $P/ca.pem)"
}

totp() {
    python3 - "$OTP_HEX" <<'PY'
import hmac, hashlib, struct, sys, time
k = bytes.fromhex(sys.argv[1]); c = int(time.time()) // 30
h = hmac.new(k, struct.pack(">Q", c), hashlib.sha1).digest(); o = h[-1] & 15
print("%06d" % ((struct.unpack(">I", h[o:o + 4])[0] & 0x7fffffff) % 1000000))
PY
}

down() {
    pkill -f "ssh.*-L 127.0.0.1:8443" 2>/dev/null || true
    gssh "pkill -x ocserv-main; pkill -x ocserv; true" || true
}

case "${1:-}" in
up) up ;;
totp) totp ;;
down) down ;;
*) echo "usage: $0 up|totp|down" >&2; exit 2 ;;
esac
