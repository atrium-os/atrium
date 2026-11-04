#!/bin/sh
# Threat model §2.1 in the VM: navigator-fetchd's address filter and per-
# conversion pin, under Capsicum + casper, unjailed AND inside its jail.
#
# ★ WHY A HERMETIC "PUBLIC" SERVER. The cloud container has no direct route to
# the internet (only an HTTP proxy, which fetchd rightly does not speak), so
# a real public fetch from the guest is impossible here. Instead slirp's
# network is put in a PUBLIC range (net=1.2.3.0/24) and slirp forwards
# 1.2.3.100:80/443 to a small server on this host. The fetcher sees a public
# address and real TLS; the trust store gets a throwaway fixture CA, and the
# fetcher bundle's /etc/hosts names the fixture `example.com` and
# `public.fixture.test`, so casper resolves them inside the jail with no DNS.
# Nothing else about the fetcher is changed.
#
#   sh scripts/cloud-vm.sh down     # if it was booted without the fixture net
#   sh scripts/navigator-fetch-vm-check.sh
#
# Needs scripts/cloud-vm.sh setup to have been run (image, ssh, sysroot,
# portcullis stack). Boots the VM itself with the fixture network.
set -u
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
VMDIR="${VMDIR:-$HOME/atrium-vm}"
VSSH="$ROOT/scripts/vssh"
KEY="$HOME/.ssh/fresco_bsd_ed25519"
SCP="scp -i $KEY -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o LogLevel=ERROR -P 2222"
TARGET=aarch64-unknown-freebsd
FIX="$VMDIR/fetchfix"
APP=org.atrium.navigator.fetcher
# portcullis, portcullisd and opifex as navigator-jail-e2e.sh staged them
# (run `sh scripts/cloud-vm.sh e2e` first); our own daemon socket.
PB=/root/navigator-e2e
PSOCK=/tmp/pd-fetch.sock
PUB=1.2.3.100
STAGE=startup
say() { printf '\n=== %s\n' "$*"; }
die() { printf '\nFAILED [%s]: %s\n' "$STAGE" "$*" >&2; exit 1; }
FAILS=0
check() { # check <label> <ok 0/1> <detail>
    if [ "$2" = 0 ]; then printf 'PASS %-62s %s\n' "$1" "$3"; else printf 'FAIL %-62s %s\n' "$1" "$3"; FAILS=$((FAILS+1)); fi
}

cleanup() {
    [ -n "${SRVPID:-}" ] && kill "$SRVPID" 2>/dev/null
    "$VSSH" "pkill -f 'nc -l 127.0.0.1 80' >/dev/null 2>&1; pkill -f 'portcullisd --socket $PSOCK'; for j in \$(jls name | grep navigator-fetcher); do jail -r \$j; done" >/dev/null 2>&1
}
trap cleanup EXIT INT TERM

# ---- fixture: CA, server cert, host-side server ----------------------------
STAGE="fixture"
say "$STAGE"
mkdir -p "$FIX"
if [ ! -f "$FIX/ca.pem" ]; then
    openssl req -x509 -newkey ec -pkeyopt ec_paramgen_curve:prime256v1 -nodes -days 30 \
        -subj "/CN=atrium fetch fixture CA" -keyout "$FIX/ca.key" -out "$FIX/ca.pem" 2>/dev/null
    openssl req -newkey ec -pkeyopt ec_paramgen_curve:prime256v1 -nodes \
        -subj "/CN=public.fixture.test" -keyout "$FIX/srv.key" -out "$FIX/srv.csr" 2>/dev/null
    printf 'subjectAltName=IP:%s,DNS:public.fixture.test,DNS:example.com\nbasicConstraints=CA:FALSE\n' "$PUB" > "$FIX/ext"
    openssl x509 -req -in "$FIX/srv.csr" -CA "$FIX/ca.pem" -CAkey "$FIX/ca.key" -CAcreateserial \
        -days 30 -extfile "$FIX/ext" -out "$FIX/srv.pem" 2>/dev/null || die "fixture certs"
fi
cat > "$FIX/server.py" <<'EOF'
import http.server, socketserver, ssl, sys, threading
class H(http.server.BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"
    def do_GET(self):
        # Same scheme as the request: an https->http hop is refused for the
        # DOWNGRADE before the address rule is even asked (measured: the
        # first version of this check proved only that).
        sch = "https" if isinstance(self.connection, ssl.SSLSocket) else "http"
        redir = {"/to-private": sch + "://10.0.0.5/admin", "/to-localhost": sch + "://localhost/",
                 "/to-metadata": sch + "://169.254.169.254/latest/meta-data/"}
        if self.path in redir:
            self.send_response(302); self.send_header("Location", redir[self.path])
            self.send_header("Content-Length", "0"); self.send_header("Connection", "close"); self.end_headers(); return
        if self.path.endswith(".js"):
            # Valid script, so a networked conversion of a page that loads it
            # through fetchd has something to run (and visibly ran it).
            body, ctype = b"document.title += ' (fixture script ran)';\n", "text/javascript"
        else:
            body, ctype = b"atrium public fixture " + self.path.encode() + b"\n", "text/plain"
        self.send_response(200); self.send_header("Content-Type", ctype)
        self.send_header("Content-Length", str(len(body))); self.send_header("Connection", "close")
        self.end_headers(); self.wfile.write(body)
    def log_message(self, *a): pass
class S(socketserver.ThreadingMixIn, http.server.HTTPServer): daemon_threads = True; allow_reuse_address = True
d = sys.argv[1]
plain = S(("127.0.0.1", 18080), H)
tls = S(("127.0.0.1", 18443), H)
ctx = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER); ctx.load_cert_chain(d + "/srv.pem", d + "/srv.key")
tls.socket = ctx.wrap_socket(tls.socket, server_side=True)
threading.Thread(target=plain.serve_forever, daemon=True).start()
tls.serve_forever()
EOF
python3 -I "$FIX/server.py" "$FIX" > "$FIX/server.log" 2>&1 &
SRVPID=$!
sleep 1
kill -0 "$SRVPID" 2>/dev/null || die "fixture server: $(cat "$FIX/server.log")"
echo "  CA + cert for $PUB / public.fixture.test / example.com; server on 127.0.0.1:18080/18443"

# ---- boot with the fixture network ----------------------------------------
STAGE="boot"
say "$STAGE  (slirp net 1.2.3.0/24, $PUB:80/443 -> host fixture)"
# ★ guestfwd's cmd: runs once per connection; a wrapper because cloud-vm.sh
# word-splits the -netdev options, so the command can hold no spaces.
for p in 80:18080 443:18443; do
    printf '#!/bin/sh\nexec nc 127.0.0.1 %s\n' "${p#*:}" > "$FIX/fwd${p%%:*}"; chmod +x "$FIX/fwd${p%%:*}"
done
NETDEV_EXTRA="net=1.2.3.0/24,host=1.2.3.2,dns=1.2.3.3,guestfwd=tcp:$PUB:80-cmd:$FIX/fwd80,guestfwd=tcp:$PUB:443-cmd:$FIX/fwd443"
if pgrep -f "file=$VMDIR/vm.qcow2" >/dev/null && ! pgrep -f "guestfwd=tcp:$PUB" >/dev/null; then
    die "the VM is running WITHOUT the fixture network; run: sh scripts/cloud-vm.sh down"
fi
NETDEV_EXTRA="$NETDEV_EXTRA" sh "$ROOT/scripts/cloud-vm.sh" up || die "boot"
GIP=$("$VSSH" "ifconfig vtnet0 inet | awk '/inet /{print \$2}'" | tr -d '\r')
echo "  guest vtnet0 $GIP"
# pf's NAT names the egress interface; DHCP changed nothing about it, but the
# ruleset must be loaded and enabled for jaild to create a networked jail.
"$VSSH" "pfctl -s info | grep -q 'Status: Enabled'" || die "pf not enabled in the guest"

# ---- cross-build -------------------------------------------------------------
STAGE="cross-build"
say "$STAGE"
# dash passes no arguments to `.`: the sourced script reads OUR "$1".
set -- env; . "$ROOT/scripts/cloud-vm.sh"
S="$VMDIR/sysroot"
export CC_aarch64_unknown_freebsd=clang AR_aarch64_unknown_freebsd=llvm-ar
export CFLAGS_aarch64_unknown_freebsd="--target=aarch64-unknown-freebsd --sysroot=$S"
(cd "$ROOT/navigator-fetch" && RUSTFLAGS="$CROSS_RUSTFLAGS" cargo +nightly build --quiet --release \
    --target $TARGET -Z build-std=std,panic_abort --bin navigator-fetchd) || die "fetchd cross-build"
# Tests need unwinding, so their own std build.
TESTBIN=$(cd "$ROOT/navigator-fetch" && RUSTFLAGS="$CROSS_RUSTFLAGS" CARGO_TARGET_DIR=target/xtest \
    cargo +nightly test --release --target $TARGET -Z build-std --lib --no-run --message-format=json 2>/dev/null \
    | python3 -I -c 'import json,sys
for l in sys.stdin:
    try: m=json.loads(l)
    except Exception: continue
    if m.get("reason")=="compiler-artifact" and m.get("profile",{}).get("test") and m.get("executable"): print(m["executable"])' | tail -1)
[ -n "$TESTBIN" ] && [ -f "$TESTBIN" ] || die "fetch test cross-build"
FD="$ROOT/navigator-fetch/target/$TARGET/release/navigator-fetchd"
echo "  fetchd + test binary built"

# ---- stage ---------------------------------------------------------------------
STAGE="stage"
say "$STAGE  (sha256 checked in the guest)"
"$VSSH" "mkdir -p /root/fetchcheck && rm -f /root/fetchcheck/navigator-fetchd /root/fetchcheck/fetch-tests" >/dev/null
$SCP "$FD" root@localhost:/root/fetchcheck/navigator-fetchd >/dev/null || die "scp fetchd"
$SCP "$TESTBIN" root@localhost:/root/fetchcheck/fetch-tests >/dev/null || die "scp tests"
$SCP "$FIX/ca.pem" root@localhost:/root/fetchcheck/fixture-ca.pem >/dev/null || die "scp ca"
for f in navigator-fetchd:"$FD" fetch-tests:"$TESTBIN"; do
    n=${f%%:*}; p=${f#*:}
    h=$(sha256sum "$p" | cut -d' ' -f1); g=$("$VSSH" "sha256 -q /root/fetchcheck/$n" | tr -d '\r')
    [ "$h" = "$g" ] || die "$n hash mismatch"
done
"$VSSH" "chmod +x /root/fetchcheck/*; cat /etc/ssl/cert.pem /root/fetchcheck/fixture-ca.pem > /root/fetchcheck/trust.pem"
echo "  ok"

# ---- 1. the library's tests, on FreeBSD ------------------------------------
STAGE="unit tests on FreeBSD"
say "$STAGE"
OUT=$("$VSSH" "cd /root/fetchcheck && ./fetch-tests --test-threads=4 2>&1 | tail -3")
echo "$OUT" | sed 's/^/  /'
echo "$OUT" | grep -q 'test result: ok' ; check "navigator-fetch lib tests on FreeBSD" $? "$(echo "$OUT" | grep 'test result' | cut -c1-60)"

# ---- 2. unjailed, capability mode, as navtest ------------------------------
STAGE="unjailed"
say "$STAGE  (cap_enter + casper, uid navtest, fixture trust)"
"$VSSH" "cp /root/fetchcheck/navigator-fetchd /root/fetchcheck/trust.pem /home/navtest/ && chown navtest /home/navtest/navigator-fetchd /home/navtest/trust.pem"
# A canned listener on the guest's loopback: the control that a refused
# private address had something there to reach.
"$VSSH" "(while :; do printf 'HTTP/1.1 200 OK\r\nContent-Length: 6\r\nConnection: close\r\n\r\nlocal\n' | nc -l 127.0.0.1 80 >/dev/null; done) >/dev/null 2>&1 &" >/dev/null
sleep 1
fetch() { # fetch <args> -- urls...   (one fetchd = one conversion)
    a="$1"; shift
    printf '%s\n' "$@" | "$VSSH" "su -m navtest -c '/home/navtest/navigator-fetchd --trust /home/navtest/trust.pem $a' 2>&1" | grep -aE '^(ok|err) '
}
"$VSSH" "grep -q public.fixture.test /etc/hosts || printf '$PUB public.fixture.test example.com\n' >> /etc/hosts"
R=$(fetch "" "https://$PUB/" "http://$PUB/plain" "https://public.fixture.test/name")
echo "$R" | sed 's/^/  /'
[ "$(echo "$R" | grep -c '^ok 200')" = 3 ]; check "public literal + name, http + https, admitted (unjailed)" $? ""
R=$(fetch "" "http://127.0.0.1/" "http://localhost/" "http://10.0.0.5/" "http://169.254.169.254/" "http://100.64.0.1/" "http://[::1]/" "http://2130706433/")
echo "$R" | sed 's/^/  /'
[ "$(echo "$R" | grep -c 'not granted')" = 7 ]; check "7 non-public targets refused (unjailed)" $? ""
R=$(fetch "" "https://$PUB/to-private" "https://$PUB/to-localhost" "https://$PUB/to-metadata" \
            "http://$PUB/to-private" "http://$PUB/to-localhost" "http://$PUB/to-metadata")
echo "$R" | sed 's/^/  /'
[ "$(echo "$R" | grep -c 'not granted')" = 6 ]; check "redirects from public to private/localhost/metadata refused" $? ""
R=$(fetch "--grant-private-network" "http://localhost/")
echo "  control (grant): $R"
echo "$R" | grep -q '^ok 200'; check "control: with the grant, localhost IS reachable (listener there)" $? ""

# ---- 3. the gate, unjailed --------------------------------------------------
STAGE="gate"
say "$STAGE  (navigator-fetchd --gate; its example.com row reaches the fixture via the guest's /etc/hosts)"
G=$("$VSSH" "su -m navtest -c '/home/navtest/navigator-fetchd --gate --trust /home/navtest/trust.pem' 2>&1")
echo "$G" | sed 's/^/  /'
echo "$G" | grep -q '^M1 GATE: PASS'; check "M1 gate (unjailed, capability mode)" $? "$(echo "$G" | grep -c '^PASS') rows pass"

# ---- 4. inside its jail -------------------------------------------------------
STAGE="jail"
say "$STAGE  (signed bundle $APP, network=full, system grant, daemon lane as navtest)"
"$VSSH" "set -e
    test -f /root/e2e-key.pem || openssl ecparam -name prime256v1 -genkey -noout -out /root/e2e-key.pem
    openssl ec -in /root/e2e-key.pem -pubout -out /root/e2e-pub.pem 2>/dev/null
    mkdir -p /etc/atrium/publishers && cp /root/e2e-pub.pem /etc/atrium/publishers/e2e.pem
    rm -rf /root/fetcher-bundle && mkdir -p /root/fetcher-bundle/bin /root/fetcher-bundle/etc/ssl
    cp /root/fetchcheck/navigator-fetchd /root/fetcher-bundle/bin/
    cp /root/fetchcheck/trust.pem /root/fetcher-bundle/etc/ssl/cert.pem
    printf '127.0.0.1 localhost\n::1 localhost\n$PUB public.fixture.test example.com\n' > /root/fetcher-bundle/etc/hosts
    printf 'hosts: files dns\n' > /root/fetcher-bundle/etc/nsswitch.conf
    printf '[app]\nid = \"$APP\"\nname = \"Navigator fetcher\"\nversion = \"1\"\nentry = \"bin/navigator-fetchd\"\n\n[capabilities]\nnetwork = \"full\"\n' > /root/fetcher-bundle/atrium.toml
    openssl dgst -sha256 -sign /root/e2e-key.pem -out /root/fetcher-bundle/atrium.toml.sig /root/fetcher-bundle/atrium.toml
    $PB/opifex install /root/fetcher-bundle
    $PB/portcullis policy grant --system $APP
    pkill -f 'portcullisd --socket $PSOCK' || true; rm -f $PSOCK
    $PB/portcullisd --socket $PSOCK > /root/fetchcheck/pd.log 2>&1 &
    sleep 2; pgrep -f 'portcullisd --socket $PSOCK' >/dev/null
    cp $PB/portcullis /home/navtest/portcullis && chown navtest /home/navtest/portcullis" > "$FIX/install.log" 2>&1 \
    || die "install fetcher bundle: $(tail -3 "$FIX/install.log")"
jfetch() {
    printf '%s\n' "$@" | "$VSSH" "export PORTCULLIS_SOCKET=$PSOCK; su -m navtest -c '/home/navtest/portcullis exec --daemon --instance fc1 $APP' 2>&1"
}
R=$(jfetch "https://$PUB/" "https://public.fixture.test/name" "http://$PUB/plain" "https://example.com/" \
           "http://127.0.0.1/" "http://localhost/" "http://10.0.0.5/" "http://169.254.169.254/" "http://100.64.0.1/" \
           "http://[::1]/" "https://$PUB/to-private" "https://$PUB/to-localhost" "http://$PUB/to-metadata" "--gate")
echo "$R" | sed 's/^/  /'
[ "$(echo "$R" | grep -ac '^ok 200')" = 4 ]; check "in the jail: 4 public fetches admitted" $? ""
[ "$(echo "$R" | grep -a '^err ' | grep -c 'not granted')" = 9 ]; check "in the jail: 6 private targets + 3 private redirect hops refused" $? ""
echo "$R" | grep -aq '^M1 GATE: PASS'; check "in the jail: M1 gate" $? "$(echo "$R" | grep -ac '^PASS') rows pass"
LJ=$("$VSSH" "jls -d name | grep -c navigator-fetcher" | tr -d ' \r')
LM=$("$VSSH" "mount -p | grep -c navigator-fetcher" | tr -d ' \r')
[ "${LJ:-0}" = 0 ] && [ "${LM:-0}" = 0 ]; check "no fetcher jails or mounts left" $? "jails=$LJ mounts=$LM"

say "$([ $FAILS = 0 ] && echo PASS || echo "FAIL ($FAILS)")"
[ $FAILS = 0 ]
