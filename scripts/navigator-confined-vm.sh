#!/bin/sh
# The Navigator with its pages confined (docs/spec/navigator-ui.md §2.1,
# §14): the live stack of navigator-live-vm.sh, but every part that touches
# a page runs as shipped, in a jail of its own, and pages come over a
# network instead of from fixture files.
#
#   sh scripts/cloud-vm.sh down     # if it was booted without the fixture net
#   sh scripts/navigator-confined-vm.sh [out-dir]
#
# - The page child (org.atrium.navigator.page) is started by the real
#   limend, as _limen, through portcullisd's ExecInstance: one jail per
#   page, as _navpage (a system worker rule), with one capability,
#   `navigator-page` (navigatord's ticket socket). scripts/navigator-page-bundle.sh.
# - navigatord runs without --unconfined-for-testing, as an unprivileged
#   broker (navtest): each page is fetched by navigator-fetchd in a jail of
#   its own (org.atrium.navigator.fetcher, network) and converted by
#   prerender in a one-shot jail with no capabilities
#   (org.atrium.navigator.converter), as navigator-converter-jail-e2e.sh
#   installs them.
# - The network is the hermetic "public" fixture of nuntius-vm-check.sh:
#   slirp in 1.2.3.0/24, 1.2.3.100:80/443 forwarded to a host-side server
#   that serves navigator-ui/fixtures by Host, with a throwaway CA the
#   fetcher's bundle trusts. The guest's /etc/hosts names the fixture
#   sites.
#
# Needs `scripts/cloud-vm.sh setup` (image, sysroot, portcullis stack) and
# kern.racct.enable=1 (every worker here asks for a memory limit).
# - frescod runs as _frescod and starts each client's renderer (the UI's
#   window, each page's window) through portcullisd, in a jail of its own
#   as _fresco-render (org.atrium.fresco.renderer, scripts/fresco-renderer-bundle.sh;
#   fresco-f4-split.md).
# What is not confined here: navigator-ui (an ordinary process as navtest)
# and forum-wm (as root).
set -u
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
OUT="${1:-$ROOT/scratch/navigator-confined}"
VMDIR="${VMDIR:-$HOME/atrium-vm}"
WAIT="${WAIT:-120}"
GAP="${GAP:-20}"
KEY="$HOME/.ssh/fresco_bsd_ed25519"
SSHOPT="-i $KEY -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o LogLevel=ERROR"
g() { ssh $SSHOPT -o ConnectTimeout=30 -p 2222 root@localhost "$@"; }
put() { scp $SSHOPT -P 2222 -q "$1" "root@localhost:$2"; }
get() { scp $SSHOPT -P 2222 -q "root@localhost:$1" "$2"; }
FIX="$VMDIR/navfix"
PUB=1.2.3.100
SITES="atlas.example old-atlas.example"
PSOCK=/atrium/sockets/portcullis/portcullis.sock
D=/opt/navigator-confined
PAGE_ID=org.atrium.navigator.page
CONV_ID=org.atrium.navigator.converter
FETCH_ID=org.atrium.navigator.fetcher
fails=0
ok() { echo "  ok: $*"; }
bad() { echo "  FAIL: $*"; fails=$((fails + 1)); }
die() { echo "ABORT: $*"; exit 1; }
cleanup() { [ -n "${SRVPID:-}" ] && kill "$SRVPID" 2>/dev/null; }
trap cleanup EXIT INT TERM

mkdir -p "$OUT"
echo "=== fixture network"
mkdir -p "$FIX"
if [ ! -f "$FIX/srv.pem" ]; then
    openssl req -x509 -newkey ec -pkeyopt ec_paramgen_curve:prime256v1 -nodes -days 30 \
        -subj "/CN=atrium navigator fixture CA" -keyout "$FIX/ca.key" -out "$FIX/ca.pem" 2>/dev/null
    openssl req -newkey ec -pkeyopt ec_paramgen_curve:prime256v1 -nodes \
        -subj "/CN=atlas.example" -keyout "$FIX/srv.key" -out "$FIX/srv.csr" 2>/dev/null
    printf 'subjectAltName=IP:%s,DNS:atlas.example,DNS:old-atlas.example\nbasicConstraints=CA:FALSE\n' "$PUB" > "$FIX/ext"
    openssl x509 -req -in "$FIX/srv.csr" -CA "$FIX/ca.pem" -CAkey "$FIX/ca.key" -CAcreateserial \
        -days 30 -extfile "$FIX/ext" -out "$FIX/srv.pem" 2>/dev/null || die "fixture certs"
fi
cat > "$FIX/server.py" <<'EOF'
import http.server, os, socketserver, ssl, sys, threading
root = os.path.realpath(sys.argv[2])
TYPES = {".css": "text/css", ".pdf": "application/pdf"}
class H(http.server.BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"
    def do_GET(self):
        host = (self.headers.get("Host") or "").split(":")[0].lower()
        rel = self.path.split("?")[0].lstrip("/") or "index.html"
        p = os.path.realpath(os.path.join(root, host, rel))
        if os.path.isdir(p): p = os.path.join(p, "index.html")
        if not p.startswith(root + os.sep) or not os.path.isfile(p):
            body, code, ctype = b"not here\n", 404, "text/plain"
        else:
            body, code = open(p, "rb").read(), 200
            ctype = TYPES.get(os.path.splitext(p)[1], "text/html; charset=utf-8")
        self.send_response(code); self.send_header("Content-Type", ctype)
        self.send_header("Content-Length", str(len(body))); self.send_header("Connection", "close")
        self.end_headers(); self.wfile.write(body)
    def log_message(self, f, *a): sys.stderr.write((f % a) + "\n"); sys.stderr.flush()
class S(socketserver.ThreadingMixIn, http.server.HTTPServer): daemon_threads = True; allow_reuse_address = True
d = sys.argv[1]
plain = S(("127.0.0.1", 18081), H)
tls = S(("127.0.0.1", 18444), H)
ctx = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER); ctx.load_cert_chain(d + "/srv.pem", d + "/srv.key")
tls.socket = ctx.wrap_socket(tls.socket, server_side=True)
threading.Thread(target=plain.serve_forever, daemon=True).start()
tls.serve_forever()
EOF
python3 -I "$FIX/server.py" "$FIX" "$ROOT/navigator-ui/fixtures" > "$FIX/server.log" 2>&1 &
SRVPID=$!
sleep 1
kill -0 "$SRVPID" 2>/dev/null || die "fixture server: $(cat "$FIX/server.log")"
for p in 80:18081 443:18444; do
    printf '#!/bin/sh\nexec nc 127.0.0.1 %s\n' "${p#*:}" > "$FIX/fwd${p%%:*}"; chmod +x "$FIX/fwd${p%%:*}"
done
echo "  $SITES at $PUB (server on 127.0.0.1:18081/18444)"

echo "=== boot"
NETDEV_EXTRA="net=1.2.3.0/24,host=1.2.3.2,dns=1.2.3.3,guestfwd=tcp:$PUB:80-cmd:$FIX/fwd80,guestfwd=tcp:$PUB:443-cmd:$FIX/fwd443"
if pgrep -f "file=$VMDIR/vm.qcow2" >/dev/null && ! pgrep -f "guestfwd=tcp:$PUB:80-cmd:$FIX" >/dev/null; then
    die "the VM is running without this fixture network: sh scripts/cloud-vm.sh down"
fi
NETDEV_EXTRA="$NETDEV_EXTRA" sh "$ROOT/scripts/cloud-vm.sh" up || die "boot"
[ "$(g "sysctl -n kern.racct.enable" | tr -d '\r')" = 1 ] || die "kern.racct.enable is 0 (loader.conf, then reboot)"

set -- env
. "$ROOT/scripts/cloud-vm.sh" >/dev/null
set --
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
# After cloud-vm.sh env, which sets its own S (the sysroot).
S=/tmp/nconf
R="$TARGET/release"
if [ "${BUILD:-1}" = 1 ]; then
    echo "=== cross-build"
    T=$(echo "$TARGET" | tr - _)
    if ! env | grep -q "^CC_$T="; then
        export "CC_$T=$(command -v clang)" "CFLAGS_$T=--target=$TARGET --sysroot=${SYSROOT:-$HOME/atrium-vm/sysroot}" \
               "AR_$T=$(command -v llvm-ar || command -v ar)"
    fi
    xb() { (cd "$ROOT/$1" && RUSTFLAGS="$CROSS_RUSTFLAGS" cargo build -q --release --target "$TARGET" $2) || die "build $1"; }
    # The whole portcullis workspace: deploy-portcullis.sh deploys only
    # missing binaries, and would otherwise leave a stale portcullisd.
    (cd "$ROOT/portcullis" && TESSERA_CORE_LIB="$ROOT/atrium-tessera/core" RUSTFLAGS="$CROSS_RUSTFLAGS" \
        cargo build -q --release --target "$TARGET") || die "portcullis cross-build"
    xb frescod "--bin frescod --bin fresco-renderer"
    xb forum-wm "--bin forum-wm"
    xb navigator-ui "--bin navigator-ui"
    xb navigator-page "--bin navigator-page"
    xb limen "--bin limend"
    xb navigator-prerender "--bin prerender"
    xb navigator-fetch "--bin navigator-fetchd"
    xb navigatord "--bin navigatord"
fi

echo "=== portcullis"
sh "$ROOT/scripts/deploy-portcullis.sh" 2>&1 | grep -v -e '^warning' -e '^ *|' -e '^ \+= ' | tail -2
for b in portcullisd portcullis opifex; do
    [ "$(sha256sum "$ROOT/portcullis/target/$R/$b" | cut -d' ' -f1)" = "$(g "sha256 -q /usr/local/bin/$b" | tr -d '\r')" ] \
        || die "/usr/local/bin/$b is not the build just made"
done
g "P=/etc/atrium/jaild.policy.toml
    if grep -q '^allow_rctl' \$P; then sed -i '' 's/^allow_rctl.*/allow_rctl = true/' \$P
    elif grep -q '^\[resource_control\]' \$P; then sed -i '' '/^\[resource_control\]/a\\
allow_rctl = true
' \$P
    else printf '\n[resource_control]\nallow_rctl = true\n' >> \$P; fi
    service atrium-jaild restart >/dev/null 2>&1; sleep 2
    pkill -x portcullisd; sleep 1; mkdir -p /atrium/sockets/portcullis; rm -f $PSOCK
    daemon -f -o /var/log/atrium/portcullisd-navigator.log /usr/local/bin/portcullisd --socket $PSOCK; sleep 3
    pgrep -f atrium-jaild >/dev/null && test -S $PSOCK" || die "jaild restart / portcullisd start"
echo "  jaild (rctl allowed) and portcullisd on $PSOCK"

echo "=== binaries"
g "pkill -x navigator-ui; pkill -x forum-wm; pkill -x limend; pkill -x navigatord; pkill -x frescod; sleep 1; rm -rf $S; mkdir -p $D $S /var/run/atrium; chmod 777 $S; true"
for b in "$ROOT/frescod/target/$R/frescod" "$ROOT/frescod/target/$R/fresco-renderer" "$ROOT/forum-wm/target/$R/forum-wm" \
         "$ROOT/navigator-ui/target/$R/navigator-ui" "$ROOT/navigator-page/target/$R/navigator-page" \
         "$ROOT/limen/target/$R/limend" "$ROOT/navigator-prerender/target/$R/prerender" \
         "$ROOT/navigator-fetch/target/$R/navigator-fetchd" "$ROOT/navigatord/target/$R/navigatord" \
         "$ROOT/scripts/navigator-page-bundle.sh" "$ROOT/scripts/fresco-renderer-bundle.sh" "$FIX/ca.pem"; do
    h=$(sha256sum "$b" | cut -d' ' -f1)
    [ "$h" = "$(g "sha256 -q $D/$(basename "$b") 2>/dev/null || true")" ] && continue
    put "$b" "$D/"
    # TCG scp sometimes lands a run of zeros: check every file.
    [ "$h" = "$(g "sha256 -q $D/$(basename "$b")")" ] || die "hash mismatch for $(basename "$b")"
done
g "mkdir -p /usr/local/share/fonts/ibm-plex"
for f in "$ROOT"/fonts/ibm-plex/*.ttf "$ROOT"/fonts/phosphor/*.ttf; do
    g "test -f /usr/local/share/fonts/ibm-plex/$(basename "$f")" || put "$f" /usr/local/share/fonts/ibm-plex/
done
echo "  staged in $D"

echo "=== bundles"
g "set -e
    grep -q 'atlas.example' /etc/hosts || printf '$PUB $SITES\n' >> /etc/hosts
    test -f /root/e2e-key.pem || openssl ecparam -name prime256v1 -genkey -noout -out /root/e2e-key.pem
    openssl ec -in /root/e2e-key.pem -pubout -out /root/e2e-pub.pem 2>/dev/null
    mkdir -p /etc/atrium/publishers && cp /root/e2e-pub.pem /etc/atrium/publishers/e2e.pem
    rm -rf /root/conv-bundle && mkdir -p /root/conv-bundle/bin
    cp $D/prerender /root/conv-bundle/bin/prerender
    printf '[app]\nid = \"$CONV_ID\"\nname = \"Navigator converter\"\nversion = \"1\"\nentry = \"bin/prerender\"\n\n[resources]\nmemory = \"4G\"\ncpu = 100\n\n[resources.pool]\nweight = 1.0\nfloor = \"256M\"\n' > /root/conv-bundle/atrium.toml
    openssl dgst -sha256 -sign /root/e2e-key.pem -out /root/conv-bundle/atrium.toml.sig /root/conv-bundle/atrium.toml
    opifex install /root/conv-bundle
    rm -rf /root/fetch-bundle && mkdir -p /root/fetch-bundle/bin /root/fetch-bundle/etc/ssl
    cp $D/navigator-fetchd /root/fetch-bundle/bin/navigator-fetchd
    cat /etc/ssl/cert.pem $D/ca.pem > /root/fetch-bundle/etc/ssl/cert.pem
    cp /etc/resolv.conf /etc/hosts /etc/nsswitch.conf /root/fetch-bundle/etc/ 2>/dev/null || true
    printf '[app]\nid = \"$FETCH_ID\"\nname = \"Navigator fetcher\"\nversion = \"1\"\nentry = \"bin/navigator-fetchd\"\n\n[capabilities]\nnetwork = \"full\"\n\n[resources]\nmemory = \"256M\"\ncpu = 100\n' > /root/fetch-bundle/atrium.toml
    openssl dgst -sha256 -sign /root/e2e-key.pem -out /root/fetch-bundle/atrium.toml.sig /root/fetch-bundle/atrium.toml
    opifex install /root/fetch-bundle
    portcullis policy grant --system $FETCH_ID
    chmod +x $D/navigator-page
    sh $D/navigator-page-bundle.sh $D/navigator-page /root/e2e-key.pem /usr/local/bin/opifex
    chmod +x $D/fresco-renderer
    cd /root && sh $D/fresco-renderer-bundle.sh $D/fresco-renderer /usr/local/share/fonts /root/e2e-key.pem /usr/local/bin/opifex
    pw usershow _limen >/dev/null 2>&1 || pw useradd _limen -u 65022 -d /nonexistent -s /usr/sbin/nologin
    pw usershow navtest >/dev/null 2>&1 || pw useradd navtest -m" > "$OUT.bundles.log" 2>&1 \
    || die "bundles: $(tail -5 "$OUT.bundles.log")"
for a in $PAGE_ID:navigator-page $CONV_ID:prerender $FETCH_ID:navigator-fetchd org.atrium.fresco.renderer:fresco-renderer; do
    g "test -x /var/lib/atrium/apps/${a%%:*}/bin/${a#*:}" && ok "${a%%:*} installed" || bad "${a%%:*} not installed"
done
g "grep -q 'workers.\"$PAGE_ID\"' /etc/atrium/policy.toml" && ok "worker rule: _limen starts the page as _navpage" || bad "no worker rule for $PAGE_ID"

echo "=== start"
# frescod as _frescod, renderers from portcullisd (no FRESCOD_RENDERER_LAUNCHER):
# the worker rule runs each as _fresco-render.
AS=$S/activation.sock
g "cd $S && env FRESCOD_SPLIT=1 FRESCOD_RENDERER=compute \
    FRESCOD_APP_REGISTRY=$S/none FRESCOD_HEADLESS_PNG=$S/screen FRESCOD_SOCK=$S/fresco.sock FRESCOD_WM_SOCK=$S/wm.sock \
    FRESCOD_INPUT_SOCK=$S/inject.sock FRESCOD_CHROME_SOCK=$S/chrome.sock FRESCOD_SLOT_UIDS=65022 FRESCOD_ACTIVATION_SOCK=$AS \
    daemon -f -o $S/frescod.log -u _frescod $D/frescod </dev/null"
i=0; while [ $i -lt 60 ] && ! g "test -S $S/fresco.sock && test -S $AS"; do sleep 2; i=$((i+1)); done
g "grep -q 'renderers are org.atrium.fresco.renderer jails' $S/frescod.log" && ok "frescod (as _frescod) starts renderers as jails" || bad "frescod: $(g "grep -i renderer $S/frescod.log | head -3 | tr '\n' ';'")"
g "cd $S && env FRESCO_SOCKET=$S/fresco.sock FRESCO_WM_SOCKET=$S/wm.sock FORUM_CTL_SOCKET=$S/ctl.sock FORUM_STATE=$S/forum-state daemon -f -o $S/wm.log $D/forum-wm </dev/null"
# limend as _limen: the Navigator (navigator-ui, as navtest: navigatord,
# also navtest, opens the UI's frescod connection, so its windows are
# navtest's) is named by this run's registry, as portcullisd's launch
# registry would name it.
g "printf '%s navtest org.atrium.navigator\n' \$(id -u navtest) > $S/registry; chmod 644 $S/registry
   cd $S && env LIMEN_SOCKET=$S/limend.sock LIMEN_APP_REGISTRY=$S/registry LIMEN_DEFAULTS=doc-viewer=$PAGE_ID \
    LIMEN_FRESCOD_ACTIVATION=$AS daemon -f -o $S/limend.log -u _limen $D/limend </dev/null"
# navigatord as an unprivileged broker: a worker runs as whoever asks for
# it, and portcullisd refuses a one-shot for root. Its ticket socket is in
# the directory the page's capability mounts.
g "mkdir -p /atrium/sockets/navigator-page && chown navtest /atrium/sockets/navigator-page && chmod 755 /atrium/sockets/navigator-page
   mkdir -p $S/store && chown navtest $S/store
   cd $S && env PORTCULLIS_SOCKET=$PSOCK NAVIGATORD_SOCKET=$S/nav.sock NAVIGATORD_STORE=$S/store FRESCOD_CHROME_SOCK=$S/chrome.sock \
    NAVIGATORD_CONVERTER='/usr/local/bin/portcullis exec --daemon --instance {instance} $CONV_ID' \
    NAVIGATORD_FETCHD='/usr/local/bin/portcullis exec --daemon --instance f{instance} $FETCH_ID' \
    NAVIGATORD_DOWNLOADS=$S/downloads daemon -f -o $S/navigatord.log -u navtest $D/navigatord </dev/null"
sleep 10
g "test -S $S/limend.sock && test -S $S/nav.sock" || { g "tail -5 $S/limend.log $S/navigatord.log"; die "limend or navigatord did not start"; }
g "grep -q UNCONFINED $S/navigatord.log" && bad "navigatord says it is unconfined" || ok "navigatord runs confined (no testing switch, no fixtures)"
mkdir -p "$OUT"

shot() { g "cp $S/screen.png $S/shot.png"; get $S/shot.png "$OUT/$1.png" && echo "  frame: $OUT/$1.png"; }
inj() { g "printf '$1\\n' | nc -N -U $S/inject.sock"; }
press() { inj "MOVE $1 $2"; sleep 1; inj "BTN 1 1"; sleep 1; inj "BTN 1 0"; sleep "$GAP"; }
last() { g "grep 'trace' $S/ui.log | tail -1"; }
until_trace() { i=0; while [ $i -lt "$WAIT" ]; do last | grep -q "$1" && return 0; sleep 3; i=$((i+3)); done; return 1; }
# The page jails and who runs in them.
jails() { g "jls name | grep -c 'limen-' || true" | tr -d '\r'; }

echo "=== drive"
g "cd $S && env FRESCO_SOCKET=$S/fresco.sock NAVIGATORD_SOCKET=$S/nav.sock LIMEN_SOCKET=$S/limend.sock NAVIGATOR_UI_TRACE=1 \
    NAVIGATOR_TRAILS=$S/trails.json NAVIGATOR_UI_LIVE=1 NAVIGATOR_UI_OPEN=https://atlas.example/wiki/Atrium daemon -f -o $S/ui.log -u navtest $D/navigator-ui --light </dev/null"
until_trace 'url=https://atlas.example/wiki/Atrium live=shown' && ok "the first page opened over the network" || bad "first page: $(last)"
sleep "$GAP"
g "grep -q 'laid out \"Atrium (architecture)\"' $S/ui.log" && ok "the jailed child redeemed its ticket and laid the page out" \
    || bad "child: $(g "grep -h -e navigator-page -e limend $S/limend.log | tail -4 | tr '\n' ';'")"
PS=$(g "ps -axo user,uid,jid,comm | awk '\$4==\"navigator-page\"'")
echo "$PS" | sed 's/^/    /'
echo "$PS" | awk '$2==65035 && $3!=0 {f=1} END {exit !f}' && ok "navigator-page runs as _navpage (65035) inside a jail" || bad "navigator-page: $PS"
echo "$PS" | awk '$2!=65035 || $3==0 {f=1} END {exit f}' && ok "no navigator-page outside a jail or as another user" || bad "a navigator-page outside: $PS"
JP=$(g "jls name path | awk '\$1 ~ /limen-/ {print \$2; exit}'" | tr -d '\r')
[ -n "$JP" ] || JP=/no-page-jail
M=$(g "mount -p | awk -v r='$JP' 'index(\$2, r)==1 {print \$1\" -> \"substr(\$2, length(r)+1)}'")
echo "$M" | sed 's/^/    /'
echo "$M" | grep -q '/atrium/sockets/navigator-page -> /atrium/sockets/navigator-page' && ok "the page jail mounts navigatord's ticket socket" || bad "no ticket socket in the page jail"
echo "$M" | grep -E -e '/atrium/sockets/(fresco|portcullis|clipboard|broker)' -e "$S" -e '/var/run/atrium' -e 'fonts' >/dev/null \
    && bad "the page jail mounts more than its capability: $(echo "$M" | tr '\n' ';')" || ok "and nothing else of the host's sockets or files"
# A vnet jail of its own with no interface but loopback: no way out.
J=$(g "jls jid name | awk '\$2 ~ /limen-/ {print \$1; exit}'" | tr -d '\r')
NJ=$(g "jls -n -j ${J:-0} vnet; ifconfig -j ${J:-0} -l" | tr '\n' ' ')
echo "    $NJ"
echo "$NJ" | grep -q 'vnet=new' && [ "$(echo "$NJ" | sed 's/vnet=new//' | tr -s ' ' | sed 's/^ //;s/ $//')" = lo0 ] \
    && ok "the page jail has no network (its own stack, lo0 only)" || bad "the page jail's network: $NJ"
L=$(g "grep -h -e '$CONV_ID' -e '$FETCH_ID' -e 'navigator.page' /var/log/atrium/portcullisd-navigator.log | tail -6")
echo "$L" | sed 's/^/    /'
g "grep -q '$CONV_ID' /var/log/atrium/portcullisd-navigator.log" && ok "the conversion ran in a converter jail" || bad "no converter jail was started"
g "grep -q '$FETCH_ID' /var/log/atrium/portcullisd-navigator.log" && ok "the fetches went through a fetcher jail" || bad "no fetcher jail was started"
g "grep -q '_limen starts $PAGE_ID as _navpage' /var/log/atrium/portcullisd-navigator.log" && ok "portcullisd applied the worker rule" || bad "no worker rule line"
RP=$(g "ps -axo user,uid,jid,comm | awk '\$4==\"fresco-renderer\"'")
echo "$RP" | sed 's/^/    /'
[ "$(echo "$RP" | awk '$2==65011 && $3!=0' | wc -l)" -ge 2 ] && ok "the UI's and the page's windows are drawn by renderers in jails, as _fresco-render" || bad "renderers: $RP"
echo "$RP" | awk '$2!=65011 || $3==0 {f=1} END {exit f}' && ok "no renderer outside a jail" || bad "a renderer outside a jail: $RP"
shot confined-1-atrium

P=$(last | sed -n 's/.* page=\([0-9.]*\),\([0-9.]*\).*/\1 \2/p'); PX=${P% *}; PY=${P#* }; PX=${PX%.*}; PY=${PY%.*}
TOP=62
press $((PX + 50)) $((TOP + PY + 64 + 20))
until_trace 'url=https://atlas.example/wiki/Impluvium live=shown' && ok "a link in the jailed page went through navigatord and followed" || bad "link: $(last)"
sleep "$GAP"
g "grep -q 'laid out \"Impluvium' $S/ui.log" && ok "the next page drew in a new jail" || bad "Impluvium: $(g "grep -h navigator-page $S/limend.log | tail -3 | tr '\n' ';'")"
shot confined-2-impluvium
# One jail per live page: the page shown and the page it came from (the
# parent of the page on screen stays live even over the budget, §4.4).
N=$(jails)
[ "${N:-0}" = 2 ] && ok "two live pages, two page jails" || bad "$N page jails for two live pages"

echo "=== end"
g "pkill -x navigator-ui"
sleep 15
N=$(jails)
[ "${N:-0}" = 0 ] && ok "the page jails ended with the Navigator" || bad "$N page jails left"
LEFT=$(g "jls name | grep -cE 'navigator[-_](converter|fetcher)' || true" | tr -d '\r')
[ "${LEFT:-0}" = 0 ] && ok "no converter or fetcher jail left" || bad "$LEFT converter/fetcher jails left"
for f in ui limend navigatord frescod; do get $S/$f.log "$OUT/$f.log" 2>/dev/null; done
get /var/log/atrium/portcullisd-navigator.log "$OUT/portcullisd.log" 2>/dev/null
g "pkill -x limend; pkill -x navigatord; pkill -x forum-wm; pkill -x frescod; true"
sleep 5
LEFT=$(g "jls name | grep -c 'fresco-renderer' || true" | tr -d '\r')
[ "${LEFT:-0}" = 0 ] && ok "no renderer jail left after frescod" || bad "$LEFT renderer jails left"
echo "navigator-confined: $fails failed"
[ "$fails" = 0 ]
