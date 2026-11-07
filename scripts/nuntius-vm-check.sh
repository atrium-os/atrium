#!/bin/sh
# Nuntius step 4 in the VM (docs/spec/nuntius.md §6.3): an app holding
# `net-fetch`, launched by portcullisd, asks for a fetcher of its own
# (OpenFetcher) and fetches through it; the fetcher runs jailed as _nuntius,
# never as the app, and its jail is gone when the app exits.
#
#   sh scripts/cloud-vm.sh down     # if it was booted without the fixture net
#   sh scripts/nuntius-vm-check.sh
#
# The network is navigator-fetch-vm-check.sh's hermetic "public" fixture:
# slirp in 1.2.3.0/24, 1.2.3.100:80/443 forwarded to a host-side server with
# a throwaway CA (the container has no direct route out). Needs
# `scripts/cloud-vm.sh setup` (image, ssh, sysroot, portcullis stack).
#
# Checks:
#   1. the net-fetch app fetches public fixture URLs (http, https, by name)
#      through its fetcher; the fetcher is nuntiusd in its own jail as
#      _nuntius (65023); the app itself has no network (raw connect fails)
#   2. the fetcher refuses loopback, localhost, the metadata endpoint, and a
#      public redirect to the metadata endpoint
#   3. an app WITHOUT net-fetch (but with portcullisd's socket, via
#      app-launch) is refused; so is a caller that is not a launched app
#   4. ExecInstance of org.atrium.nuntius is refused
#   5. no fetcher jail or mount is left once the app exits
set -u
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
VMDIR="${VMDIR:-$HOME/atrium-vm}"
VSSH="$ROOT/scripts/vssh"
KEY="$HOME/.ssh/fresco_bsd_ed25519"
SCP="scp -i $KEY -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o LogLevel=ERROR -P 2222"
TARGET=aarch64-unknown-freebsd
FIX="$VMDIR/fetchfix"
PUB=1.2.3.100
W=/root/nuntiuscheck
PROBE=org.atrium.test.nuntius-probe
NOFETCH=org.atrium.test.nuntius-nofetch
PSOCK=/atrium/sockets/portcullis/portcullis.sock
PLOG=/var/log/atrium/portcullisd-nuntius.log
STAGE=startup
say() { printf '\n=== %s\n' "$*"; }
die() { printf '\nFAILED [%s]: %s\n' "$STAGE" "$*" >&2; exit 1; }
FAILS=0
check() { # check <label> <ok 0/1> <detail>
    if [ "$2" = 0 ]; then printf 'PASS %-66s %s\n' "$1" "$3"; else printf 'FAIL %-66s %s\n' "$1" "$3"; FAILS=$((FAILS+1)); fi
}
cleanup() { [ -n "${SRVPID:-}" ] && kill "$SRVPID" 2>/dev/null; }
trap cleanup EXIT INT TERM

# ---- fixture (as navigator-fetch-vm-check.sh) ------------------------------
STAGE=fixture
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
cat > "$FIX/nserver.py" <<'EOF'
import http.server, socketserver, ssl, sys, threading
class H(http.server.BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"
    def do_GET(self):
        sch = "https" if isinstance(self.connection, ssl.SSLSocket) else "http"
        redir = {"/to-localhost": sch + "://localhost/",
                 "/to-metadata": sch + "://169.254.169.254/latest/meta-data/"}
        if self.path in redir:
            self.send_response(302); self.send_header("Location", redir[self.path])
            self.send_header("Content-Length", "0"); self.send_header("Connection", "close"); self.end_headers(); return
        n = 100000 if self.path == "/big" else 0
        body = b"atrium public fixture " + self.path.encode() + b"\n" + b"x" * n
        self.send_response(200); self.send_header("Content-Type", "text/plain")
        self.send_header("Content-Length", str(len(body))); self.send_header("Connection", "close")
        self.end_headers(); self.wfile.write(body)
    def log_message(self, f, *a): sys.stderr.write((f % a) + "\n"); sys.stderr.flush()
class S(socketserver.ThreadingMixIn, http.server.HTTPServer): daemon_threads = True; allow_reuse_address = True
d = sys.argv[1]
plain = S(("127.0.0.1", 18080), H)
tls = S(("127.0.0.1", 18443), H)
ctx = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER); ctx.load_cert_chain(d + "/srv.pem", d + "/srv.key")
tls.socket = ctx.wrap_socket(tls.socket, server_side=True)
threading.Thread(target=plain.serve_forever, daemon=True).start()
tls.serve_forever()
EOF
python3 -I "$FIX/nserver.py" "$FIX" > "$FIX/nserver.log" 2>&1 &
SRVPID=$!
sleep 1
kill -0 "$SRVPID" 2>/dev/null || die "fixture server: $(cat "$FIX/nserver.log")"
for p in 80:18080 443:18443; do
    printf '#!/bin/sh\nexec nc 127.0.0.1 %s\n' "${p#*:}" > "$FIX/fwd${p%%:*}"; chmod +x "$FIX/fwd${p%%:*}"
done
echo "  server on 127.0.0.1:18080/18443 for $PUB"

# ---- boot with the fixture network ------------------------------------------
STAGE=boot
say "$STAGE"
NETDEV_EXTRA="net=1.2.3.0/24,host=1.2.3.2,dns=1.2.3.3,guestfwd=tcp:$PUB:80-cmd:$FIX/fwd80,guestfwd=tcp:$PUB:443-cmd:$FIX/fwd443"
if pgrep -f "file=$VMDIR/vm.qcow2" >/dev/null && ! pgrep -f "guestfwd=tcp:$PUB" >/dev/null; then
    die "the VM is running WITHOUT the fixture network; run: sh scripts/cloud-vm.sh down"
fi
NETDEV_EXTRA="$NETDEV_EXTRA" sh "$ROOT/scripts/cloud-vm.sh" up || die "boot"
"$VSSH" "pfctl -s info | grep -q 'Status: Enabled'" || die "pf not enabled in the guest"
# Every fetcher is started with a required memory limit: without racct the
# one-shot is refused, and the app sees only its channel close.
[ "$("$VSSH" "sysctl -n kern.racct.enable" | tr -d '\r')" = 1 ] \
    || die "kern.racct.enable is 0: add kern.racct.enable=1 to the guest's /boot/loader.conf, cloud-vm.sh down, rerun"

# ---- cross-build -----------------------------------------------------------
STAGE=cross-build
say "$STAGE  (portcullis workspace, nuntiusd, nuntius-probe)"
set -- env; . "$ROOT/scripts/cloud-vm.sh"
S="$VMDIR/sysroot"
export CC_aarch64_unknown_freebsd=clang AR_aarch64_unknown_freebsd=llvm-ar
export CFLAGS_aarch64_unknown_freebsd="--target=aarch64-unknown-freebsd --sysroot=$S"
# ★ The whole workspace first: deploy-portcullis.sh builds only MISSING
# binaries, and would otherwise deploy a stale portcullisd.
(cd "$ROOT/portcullis" && TESSERA_CORE_LIB="$ROOT/atrium-tessera/core" RUSTFLAGS="$CROSS_RUSTFLAGS" \
    cargo build --quiet --release --target $TARGET) || die "portcullis cross-build"
(cd "$ROOT/navigator-fetch" && RUSTFLAGS="$CROSS_RUSTFLAGS" cargo +nightly build --quiet --release \
    --target $TARGET -Z build-std=std,panic_abort --bin nuntiusd) || die "nuntiusd cross-build"
(cd "$ROOT/nuntius" && RUSTFLAGS="$CROSS_RUSTFLAGS" cargo +nightly build --quiet --release \
    --target $TARGET -Z build-std=std,panic_abort --example nuntius-probe) || die "nuntius-probe cross-build"
ND="$ROOT/navigator-fetch/target/$TARGET/release/nuntiusd"
NP="$ROOT/nuntius/target/$TARGET/release/examples/nuntius-probe"

# ---- deploy ---------------------------------------------------------------
STAGE=deploy
say "$STAGE  (portcullis stack, sha256 checked in the guest)"
sh "$ROOT/scripts/deploy-portcullis.sh" 2>&1 | grep -v -e '^warning' -e '^ *|' -e '^ \+= ' | tail -3
T="$ROOT/portcullis/target/$TARGET/release"
for b in portcullisd portcullis opifex; do
    h=$(sha256sum "$T/$b" | cut -d' ' -f1); g=$("$VSSH" "sha256 -q /usr/local/bin/$b" | tr -d '\r')
    [ "$h" = "$g" ] || die "/usr/local/bin/$b is not the build just made"
done
# The fetcher's bundle asks for a memory limit, so jaild must allow rctl.
"$VSSH" "P=/etc/atrium/jaild.policy.toml
    if grep -q '^allow_rctl' \$P; then sed -i '' 's/^allow_rctl.*/allow_rctl = true/' \$P
    elif grep -q '^\[resource_control\]' \$P; then sed -i '' '/^\[resource_control\]/a\\
allow_rctl = true
' \$P
    else printf '\n[resource_control]\nallow_rctl = true\n' >> \$P; fi
    service atrium_jaild restart >/dev/null 2>&1; sleep 2
    # The app launcher on the socket a net-fetch app's jail sees
    # (/atrium/sockets/portcullis/, mounted by the capability).
    pkill -x portcullisd; sleep 1; mkdir -p /atrium/sockets/portcullis; rm -f $PSOCK
    daemon -f -o $PLOG /usr/local/bin/portcullisd --socket $PSOCK; sleep 3
    pgrep -f atrium-jaild >/dev/null && test -S $PSOCK" || die "jaild restart / portcullisd start"

"$VSSH" "rm -rf $W && mkdir -p $W" >/dev/null
$SCP "$ND" root@localhost:$W/nuntiusd >/dev/null || die "scp nuntiusd"
$SCP "$NP" root@localhost:$W/nuntius-probe >/dev/null || die "scp probe"
$SCP "$FIX/ca.pem" "$ROOT/scripts/nuntius-bundle.sh" root@localhost:$W/ >/dev/null || die "scp ca"
for f in nuntiusd:"$ND" nuntius-probe:"$NP"; do
    n=${f%%:*}; p=${f#*:}
    h=$(sha256sum "$p" | cut -d' ' -f1); g=$("$VSSH" "sha256 -q $W/$n" | tr -d '\r')
    [ "$h" = "$g" ] || die "$n hash mismatch"
done
echo "  ok"

# ---- install: the fetcher and two test apps ----------------------------------
STAGE=install
say "$STAGE  (org.atrium.nuntius via nuntius-bundle.sh; $PROBE with net-fetch; $NOFETCH without)"
"$VSSH" "set -e
    chmod +x $W/nuntiusd $W/nuntius-probe
    cat /etc/ssl/cert.pem $W/ca.pem > $W/trust.pem
    grep -q public.fixture.test /etc/hosts || printf '$PUB public.fixture.test example.com\n' >> /etc/hosts
    test -f /root/e2e-key.pem || openssl ecparam -name prime256v1 -genkey -noout -out /root/e2e-key.pem
    openssl ec -in /root/e2e-key.pem -pubout -out /root/e2e-pub.pem 2>/dev/null
    mkdir -p /etc/atrium/publishers && cp /root/e2e-pub.pem /etc/atrium/publishers/e2e.pem
    cd $W && PATH=/usr/local/bin:\$PATH sh nuntius-bundle.sh $W/nuntiusd /root/e2e-key.pem /usr/local/bin/opifex $W/trust.pem
    for a in $PROBE:'net-fetch = true' $NOFETCH:'app-launch = true'; do
        id=\${a%%:*}; cap=\${a#*:}; B=$W/bundle-\$id
        rm -rf \$B && mkdir -p \$B/bin && cp $W/nuntius-probe \$B/bin/
        printf '[app]\nid = \"%s\"\nname = \"Nuntius probe\"\nversion = \"1\"\nentry = \"bin/nuntius-probe\"\n\n[capabilities]\n%s\n' \"\$id\" \"\$cap\" > \$B/atrium.toml
        openssl dgst -sha256 -sign /root/e2e-key.pem -out \$B/atrium.toml.sig \$B/atrium.toml
        /usr/local/bin/opifex install \$B
    done" > "$VMDIR/nuntius-install.log" 2>&1 || die "install: $(tail -5 "$VMDIR/nuntius-install.log")"
tail -1 "$VMDIR/nuntius-install.log" | sed 's/^/  /'

# ---- 1+2+5. the net-fetch app ----------------------------------------------------
STAGE="net-fetch app"
say "$STAGE  (portcullis launch $PROBE; URLs on its stdin)"
"$VSSH" "printf '%s\n' https://$PUB/ http://$PUB/plain https://public.fixture.test/name http://$PUB/big \
        raw:$PUB:80 http://127.0.0.1/ http://localhost/ http://169.254.169.254/latest/meta-data/ \
        https://$PUB/to-metadata https://$PUB/to-localhost sleep:30 > $W/urls
    rm -f $W/launch.out; (/usr/local/bin/portcullis launch --no-prompt $PROBE < $W/urls > $W/launch.out 2>&1; echo \"exit \$?\" >> $W/launch.out) >/dev/null 2>&1 &" >/dev/null
# While the probe sleeps: the fetcher's jail and its process's uid.
SEEN=""
t0=$(date +%s)
while [ $(( $(date +%s) - t0 )) -lt 600 ]; do
    if "$VSSH" "grep -q '^sleeping' $W/launch.out 2>/dev/null"; then
        SEEN=$("$VSSH" "jls name | grep nuntius; echo ---; ps -axo user,uid,jid,comm | awk '\$4==\"nuntiusd\" || \$4==\"nuntius-probe\"'")
        break
    fi
    sleep 5
done
echo "$SEEN" | sed 's/^/  /'
t0=$(date +%s)
until "$VSSH" "grep -q '^exit ' $W/launch.out 2>/dev/null"; do
    [ $(( $(date +%s) - t0 )) -gt 600 ] && break; sleep 5
done
R=$("$VSSH" "cat $W/launch.out" | tr -d '\r')
echo "$R" | sed 's/^/  /'
[ "$(echo "$R" | grep -c '^ok 200 ')" = 4 ]; check "4 public fetches through the app's own fetcher (http, https, name, 100 KB)" $? ""
echo "$R" | grep -q "^ok 200 100027 http://$PUB/big"; check "100 KB body arrives whole by Fd" $? ""
echo "$R" | grep -q "^raw refused .* $PUB:80"; check "the app itself has no network (raw connect fails)" $? "$(echo "$R" | grep '^raw')"
for u in http://127.0.0.1/ http://localhost/ http://169.254.169.254/latest/meta-data/ https://$PUB/to-metadata https://$PUB/to-localhost; do
    echo "$R" | grep -q "^err .* $u\$"; check "fetcher refuses $u" $? "$(echo "$R" | grep " $u\$" | cut -d' ' -f1-2)"
done
FJ=$(echo "$SEEN" | grep -c '^app-org-atrium-nuntius--u')
check "a fetcher jail of its own while the app runs" $([ "$FJ" = 1 ]; echo $?) "$(echo "$SEEN" | grep '^app-org-atrium-nuntius' | head -1)"
FU=$(echo "$SEEN" | awk '$4=="nuntiusd"{print $1":"$2}' | sort -u)
check "nuntiusd runs as _nuntius (65023)" $([ "$FU" = "_nuntius:65023" ]; echo $?) "$FU"
PU=$(echo "$SEEN" | awk '$4=="nuntius-probe"{print $1":"$2}' | sort -u)
check "the app runs as its own uid, not _nuntius" $([ -n "$PU" ] && [ "${PU#*:}" != 65023 ]; echo $?) "$PU"
FJID=$(echo "$SEEN" | awk '$4=="nuntiusd"{print $3}' | sort -u); PJID=$(echo "$SEEN" | awk '$4=="nuntius-probe"{print $3}' | sort -u)
check "fetcher and app are in different jails" $([ -n "$FJID" ] && [ -n "$PJID" ] && [ "$FJID" != "$PJID" ] && [ "$FJID" != 0 ]; echo $?) "fetcher jid $FJID, app jid $PJID"
"$VSSH" "grep -a 'fetcher' $PLOG 2>/dev/null | tail -3" | sed 's/^/  log: /'

STAGE="after exit"
say "$STAGE"
sleep 5
LJ=$("$VSSH" "jls -d name | grep -c nuntius" | tr -d ' \r')
LM=$("$VSSH" "mount -p | grep -c nuntius" | tr -d ' \r')
LP=$("$VSSH" "pgrep -x nuntiusd | wc -l" | tr -d ' \r')
check "no fetcher jail, mount or process left after the app exits" $([ "${LJ:-0}" = 0 ] && [ "${LM:-0}" = 0 ] && [ "${LP:-0}" = 0 ]; echo $?) "jails=$LJ mounts=$LM procs=$LP"

# ---- 3. refusals of the caller ----------------------------------------------
STAGE="refusals"
say "$STAGE"
R=$("$VSSH" "echo https://$PUB/ | /usr/local/bin/portcullis launch --no-prompt $NOFETCH 2>&1" | tr -d '\r')
echo "$R" | sed 's/^/  /'
echo "$R" | grep -q '^refused .*cap.net_fetch.denied: this app'; check "an app without net-fetch is refused" $? ""
R=$("$VSSH" "cp $W/nuntius-probe /home/navtest/ && chown navtest /home/navtest/nuntius-probe
    echo https://$PUB/ | su -m navtest -c /home/navtest/nuntius-probe 2>&1" | tr -d '\r')
echo "  navtest, unjailed: $R"
echo "$R" | grep -q '^refused .*\(not a launched app\|Permission denied\|not reachable\)'; check "a caller that is not a launched app is refused" $? ""
R=$("$VSSH" "echo | /usr/local/bin/portcullis exec --daemon --instance x1 org.atrium.nuntius 2>&1; echo exit \$?" | tr -d '\r')
echo "$R" | sed 's/^/  /'
echo "$R" | grep -q 'started only by OpenFetcher'; check "ExecInstance of org.atrium.nuntius is refused" $? ""
LJ=$("$VSSH" "jls -d name | grep -c nuntius" | tr -d ' \r')
check "still no fetcher jail after the refusals" $([ "${LJ:-0}" = 0 ]; echo $?) "jails=$LJ"

say "$([ $FAILS = 0 ] && echo PASS || echo "FAIL ($FAILS)")"
[ $FAILS = 0 ]
