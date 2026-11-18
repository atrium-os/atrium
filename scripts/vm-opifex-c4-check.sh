#!/bin/sh
# Opifex C4 (docs/spec/opifex.md §6.4, §7.2, §14.5): opifexd syncs the catalog
# and downloads archives over the network through opifex-fetch, a launched
# bundle whose only capability is net-fetch (its own Nuntius, its sites
# narrowed), and refuses a catalog whose history was forked.
#
# The network is the hermetic fixture of nuntius-vm-check.sh: slirp in
# 1.2.3.0/24, 1.2.3.100:443 forwarded to a host-side HTTPS server with a
# throwaway CA, here serving files (the catalog and the archives). Boot the VM
# with it first:
#
#   sh scripts/vm-opifex-c4-check.sh netdev          # prints NETDEV_EXTRA
#   NETDEV_EXTRA="$(sh scripts/vm-opifex-c4-check.sh netdev)" sh scripts/run-vm.sh   # Mac
#   NETDEV_EXTRA="…" sh scripts/cloud-vm.sh up                                       # cloud
#   ARCH=aarch64 sh scripts/vm-opifex-c4-check.sh
#
# Ends "opifex-c4: N failed". It redeploys the portcullis stack and Nuntius
# (trusting the fixture CA while it runs, the system's store again at the end).
#
#   - opifex sync with no argument: the catalog through the fetcher;
#     the fetcher was launched by portcullisd and fetched through Nuntius;
#   - opifex get: the archive downloaded, checked, installed;
#   - a server handing out other bytes: refused, nothing kept, 1.0 stays;
#   - a host outside the fetcher's sites: refused by Nuntius;
#   - a forked history (an old entry changed, the head re-signed with the
#     catalog key): refused, the local catalog unchanged;
#   - no fetcher jail left afterwards.
set -u
ROOT=$(cd "$(dirname "$0")/.." && pwd)
VMDIR="${VMDIR:-$HOME/atrium-vm}"
FIX="$VMDIR/opifexfix"
PUB=1.2.3.100
HOST=public.fixture.test
OTHER=other.fixture.test
mkdir -p "$FIX"
printf '#!/bin/sh\nexec nc 127.0.0.1 18444\n' > "$FIX/fwd443"; chmod +x "$FIX/fwd443"
if [ "${1:-}" = netdev ]; then
    echo "net=1.2.3.0/24,host=1.2.3.2,dns=1.2.3.3,guestfwd=tcp:$PUB:443-cmd:$FIX/fwd443"
    exit 0
fi
export ARCH=${ARCH:-amd64}
set -- env
. "$ROOT/scripts/cloud-vm.sh" >/dev/null
set --
K="-i $HOME/.ssh/fresco_bsd_ed25519 -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o LogLevel=ERROR"
V() { "$ROOT/scripts/vssh" "$@"; }
fails=0
ok() { echo "  ok $1"; }
bad() { echo "  FAIL $1"; fails=$((fails+1)); }
check() { if [ "$2" = "$3" ]; then ok "$1"; else bad "$1: expected [$2] got [$3]"; fi; }
die() { echo "  FAIL $1"; echo "opifex-c4: $((fails+1)) failed"; exit 1; }
SRVPID=
trap '[ -n "$SRVPID" ] && kill $SRVPID 2>/dev/null' EXIT INT TERM

echo "=== fixture"
if [ ! -f "$FIX/ca.pem" ]; then
    openssl req -x509 -newkey ec -pkeyopt ec_paramgen_curve:prime256v1 -nodes -days 30 \
        -subj "/CN=atrium opifex fixture CA" -keyout "$FIX/ca.key" -out "$FIX/ca.pem" 2>/dev/null
    openssl req -newkey ec -pkeyopt ec_paramgen_curve:prime256v1 -nodes \
        -subj "/CN=$HOST" -keyout "$FIX/srv.key" -out "$FIX/srv.csr" 2>/dev/null
    printf 'subjectAltName=IP:%s,DNS:%s,DNS:%s\nbasicConstraints=CA:FALSE\n' "$PUB" "$HOST" "$OTHER" > "$FIX/ext"
    openssl x509 -req -in "$FIX/srv.csr" -CA "$FIX/ca.pem" -CAkey "$FIX/ca.key" -CAcreateserial \
        -days 30 -extfile "$FIX/ext" -out "$FIX/srv.pem" 2>/dev/null || die "fixture certs"
fi
rm -rf "$FIX/web"; mkdir -p "$FIX/web"
cat > "$FIX/fileserver.py" <<'EOF'
import functools, http.server, socketserver, ssl, sys
class H(http.server.SimpleHTTPRequestHandler):
    protocol_version = "HTTP/1.1"
    def log_message(self, f, *a): sys.stderr.write((f % a) + "\n"); sys.stderr.flush()
class S(socketserver.ThreadingMixIn, http.server.HTTPServer): daemon_threads = True; allow_reuse_address = True
d, web = sys.argv[1], sys.argv[2]
srv = S(("127.0.0.1", 18444), functools.partial(H, directory=web))
ctx = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER); ctx.load_cert_chain(d + "/srv.pem", d + "/srv.key")
srv.socket = ctx.wrap_socket(srv.socket, server_side=True)
srv.serve_forever()
EOF
python3 -I "$FIX/fileserver.py" "$FIX" "$FIX/web" > "$FIX/server.log" 2>&1 &
SRVPID=$!
sleep 1
kill -0 "$SRVPID" 2>/dev/null || die "file server: $(cat "$FIX/server.log")"
V "nc -z -w 5 $PUB 443" >/dev/null 2>&1 || die "the guest can't reach $PUB:443: boot it with NETDEV_EXTRA=\"\$(sh $0 netdev)\""
ok "the guest reaches the fixture server"

echo "=== cross-build"
# As nuntius-vm-check.sh: ring's C code needs the guest's sysroot (assert.h…).
# llvm-ar is Homebrew's on the Mac, where it isn't on PATH.
AR_X=$(command -v llvm-ar || ls /opt/homebrew/opt/llvm/bin/llvm-ar /usr/local/opt/llvm/bin/llvm-ar 2>/dev/null | head -1)
export CC_aarch64_unknown_freebsd=clang AR_aarch64_unknown_freebsd="${AR_X:-llvm-ar}"
export CFLAGS_aarch64_unknown_freebsd="--target=aarch64-unknown-freebsd --sysroot=$VMDIR/sysroot"
(cd "$ROOT/portcullis" && TESSERA_CORE_LIB="$ROOT/atrium-tessera/core" RUSTFLAGS="$CROSS_RUSTFLAGS" \
    cargo build -q --release --target "$TARGET") || die "portcullis cross-build"
(cd "$ROOT/navigator-fetch" && RUSTFLAGS="$CROSS_RUSTFLAGS" cargo build -q --release \
    --target "$TARGET" -Z build-std=std,panic_abort --bin nuntiusd) || die "nuntiusd cross-build"
T="$ROOT/portcullis/target/$TARGET/release"
ND="$ROOT/navigator-fetch/target/$TARGET/release/nuntiusd"

echo "=== deploy"
sh "$ROOT/scripts/deploy-portcullis.sh" 2>&1 | grep -v -e '^warning' -e '^ *|' -e '^ \+= ' | tail -3
for b in portcullisd portcullis opifex; do
    h=$(sha256sum "$T/$b" 2>/dev/null || shasum -a 256 "$T/$b"); h=${h%% *}
    [ "$h" = "$(V "sha256 -q /usr/local/bin/$b" | tr -d '\r')" ] || die "/usr/local/bin/$b is not the build just made"
done
B=/var/tmp/opifex-c4
PSOCK=/atrium/sockets/portcullis/portcullis.sock
PLOG=/var/log/atrium/portcullisd-opifex-c4.log
ID=org.atrium.c4hello
ARCHN=$( [ "$ARCH" = amd64 ] && echo amd64 || echo aarch64 )
V "pkill -x opifexd; rm -rf $B /var/db/atrium/opifex /var/lib/atrium/opifex /var/lib/atrium/apps/$ID; mkdir -p $B"
for f in "$T/opifexd" "$T/opifex-check" "$T/opifex-fetch" "$ND" "$FIX/ca.pem" "$ROOT/scripts/nuntius-bundle.sh" "$ROOT/scripts/opifex-fetch-bundle.sh"; do
    scp -q $K -P 2222 "$f" root@localhost:$B/ || die "copy $f"
done
V 'sh -s' <<EOS || die "setup"
set -e
# The fetcher bundles ask for a memory limit, so jaild must allow rctl (deploy rewrites the policy).
P=/etc/atrium/jaild.policy.toml
if grep -q '^allow_rctl' \$P; then sed -i '' 's/^allow_rctl.*/allow_rctl = true/' \$P
elif grep -q '^\[resource_control\]' \$P; then sed -i '' '/^\[resource_control\]/a\\
allow_rctl = true
' \$P
else printf '\n[resource_control]\nallow_rctl = true\n' >> \$P; fi
service atrium-jaild restart >/dev/null 2>&1; sleep 2
pkill -x portcullisd || true; sleep 1; mkdir -p /atrium/sockets/portcullis; rm -f $PSOCK
daemon -f -o $PLOG /usr/local/bin/portcullisd --socket $PSOCK; sleep 3
test -S $PSOCK
install -m 0755 $B/opifexd /usr/local/sbin/opifexd
mkdir -p /usr/local/libexec; install -m 0755 $B/opifex-check /usr/local/libexec/opifex-check
id _opifex >/dev/null 2>&1 || pw useradd _opifex -u 65041 -d /nonexistent -s /usr/sbin/nologin -c "Opifex worker"
grep -q '$HOST' /etc/hosts || printf '$PUB $HOST $OTHER\n' >> /etc/hosts
cd $B
chmod +x opifex-fetch nuntiusd
for k in root1 root2 catalog platform; do
  openssl ecparam -name prime256v1 -genkey -noout | openssl pkcs8 -topk8 -nocrypt -out \$k.pem
  openssl ec -in \$k.pem -pubout -out \$k.pub 2>/dev/null
done
# The test platform key signs the fetcher bundle too: on the side-load list while this runs.
mkdir -p /etc/atrium/publishers; cp platform.pub /etc/atrium/publishers/opifex-c4.pem
cat /etc/ssl/cert.pem ca.pem > trust.pem
# Nuntius signed with the gate key nuntius-vm-check.sh uses, which stays listed.
test -f /root/e2e-key.pem || openssl ecparam -name prime256v1 -genkey -noout -out /root/e2e-key.pem
openssl ec -in /root/e2e-key.pem -pubout -out /etc/atrium/publishers/e2e.pem 2>/dev/null
PATH=/usr/local/bin:\$PATH sh nuntius-bundle.sh $B/nuntiusd /root/e2e-key.pem /usr/local/bin/opifex $B/trust.pem >/dev/null
PATH=/usr/local/bin:\$PATH sh opifex-fetch-bundle.sh $B/opifex-fetch $B/platform.pem $HOST
{
  echo 'schema = "atrium-catalog-root/1"'; echo 'name = "Atrium"'; echo 'version = 1'
  echo 'expires = 4000000000'; echo 'threshold = 2'
  for k in root1 root2; do printf '\n[[key]]\npem = """%s"""\nroles = ["root"]\n' "\$(cat \$k.pub)"; done
  printf '\n[[key]]\npem = """%s"""\nroles = ["catalog"]\n' "\$(cat catalog.pub)"
  printf '\n[[key]]\npem = """%s"""\nroles = ["platform"]\nprefixes = ["org.atrium."]\nkinds = ["app"]\n' "\$(cat platform.pub)"
} > root.toml
mkdir -p cat /etc/atrium/opifex
cp root.toml cat/root.toml; cp root.toml /etc/atrium/opifex/root.toml
opifex catalog root-sign cat --key root1.pem --pub root1.pub >/dev/null
opifex catalog root-sign cat --key root2.pem --pub root2.pub >/dev/null
cat > publish.sh <<'PUB'
set -e
v=\$1
rm -rf b\$v; mkdir -p b\$v/bin; cp /bin/echo b\$v/bin/hello; printf "\$v" >> b\$v/bin/hello
printf '[app]\nid = "%s"\nname = "C4 hello"\nversion = "%s"\nentry = "bin/hello"\n' $ID \$v > b\$v/atrium.toml
opifex pack b\$v --sign platform.pem >/dev/null
opifex archive b\$v \$v.insula >/dev/null
opifex catalog add cat b\$v --arch $ARCHN --archive \$v.insula --url https://$HOST/\$v.insula >/dev/null
opifex catalog head cat --key catalog.pem >/dev/null
PUB
sh publish.sh 1.0
(OPIFEX_SOCKET=$B/opifex.sock OPIFEX_CATALOG_URL=https://$HOST/catalog/ /usr/local/sbin/opifexd > $B/opifexd.log 2>&1 &)
sleep 1
EOS
ok "portcullisd, Nuntius (trusting the fixture CA), the fetcher bundle, opifexd"
# serve <dir in the guest>: what the "web" serves as /catalog/, plus the archives.
serve() {
    rm -rf "$FIX/web/catalog"
    scp -q -r $K -P 2222 "root@localhost:$B/$1" "$FIX/web/catalog" || die "copy the catalog"
    for v in 1.0 1.1; do V "test -f $B/$v.insula" && scp -q $K -P 2222 "root@localhost:$B/$v.insula" "$FIX/web/" ; done
    true
}
Q() { V "cd $B && OPIFEX_SOCKET=$B/opifex.sock opifex $* 2>&1"; }

echo "=== sync and install over the network"
serve cat
check "opifex sync: the catalog through the fetcher" "1" "$(Q sync | grep -c '1 entries, 1 releases')"
check "the server was asked for the head" "1" "$(grep -c 'GET /catalog/head HTTP' "$FIX/server.log" | awk '{print ($1>0)}')"
check "portcullisd launched the fetcher bundle" "1" "$(V "grep -c 'launching org.atrium.opifex.fetch as uid' $PLOG" | tr -d ' ' | awk '{print ($1>0)}')"
check "…which asked for a Nuntius of its own" "1" "$(V "grep -c 'fetcher .* for org.atrium.opifex.fetch' $PLOG" | tr -d ' ' | awk '{print ($1>0)}')"
check "opifex get downloads and installs 1.0" "1" "$(Q get $ID | grep -c '1.0: done')"
check "the archive came from the server" "1" "$(grep -c 'GET /1.0.insula HTTP' "$FIX/server.log" | awk '{print ($1>0)}')"
check "the tree is in apps/" "yes" "$(V "cmp -s $B/b1.0/bin/hello /var/lib/atrium/apps/$ID/bin/hello && echo yes")"

echo "=== refusals"
V "cd $B && sh publish.sh 1.1" || die "publish 1.1"
serve cat
printf 'INSB this is not the archive' > "$FIX/web/1.1.insula"
check "sync sees 1.1" "1" "$(Q sync | grep -c '2 entries, 2 releases')"
check "other bytes for 1.1: refused" "1" "$(Q get $ID | grep -c 'not the archive the catalog names')"
check "nothing kept" "gone" "$(V "test -e /var/db/atrium/opifex/dl/\$(sha256 -q $B/1.1.insula).insula && echo kept || echo gone")"
check "1.0 stays installed" "1.0" "$(V "grep '^version' /var/lib/atrium/apps/$ID/atrium.toml | cut -d'\"' -f2")"
check "a host outside the fetcher's sites" "1" "$(Q sync https://$OTHER/catalog/ | grep -c 'Policy')"
# A forked history: change entry 0's manifest inside the bundle, re-sign the head with the catalog key.
V "cd $B && rm -rf forged && cp -R cat forged && f=\$(ls forged/entries | head -1) && sed -i '' 's/C4 hello/C4 hellx/' forged/entries/\$f && opifex catalog head forged --key catalog.pem >/dev/null 2>&1; true"
serve forged
check "a forked history: refused" "1" "$(Q sync | grep -c 'history rewritten\|differs\|not signed\|hash to the signed root')"
check "the local catalog unchanged" "1" "$(Q describe $ID | grep -c 'C4 hello')"
sleep 5
check "no fetcher jail left" "0" "$(V "jls name | grep -c -e opifex-fetch -e nuntius" | tr -d ' ')"

echo "=== cleanup"
V "pkill -x opifexd; opifex uninstall $ID >/dev/null 2>&1; opifex uninstall org.atrium.opifex.fetch >/dev/null 2>&1
   rm -f /etc/atrium/publishers/opifex-c4.pem /etc/atrium/opifex/root.toml
   cd $B && PATH=/usr/local/bin:\$PATH sh nuntius-bundle.sh $B/nuntiusd /root/e2e-key.pem /usr/local/bin/opifex /etc/ssl/cert.pem >/dev/null 2>&1" \
   && ok "Nuntius trusts the system's store again" || bad "cleanup"
echo "opifex-c4: $fails failed"
