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
#   page, as a uid of the person's (and persona's) own (a per-instance
#   system worker rule), with one capability,
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
    xb monstrator "--bin monstratord --example monctl --example link-probe"
    xb atrium-open "--example open-probe"
fi

echo "=== portcullis"
# The portcullisd this script started last time holds its binary (ETXTBSY).
g "pkill -x portcullisd; sleep 1; true"
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
    : > /var/log/atrium/portcullisd-navigator.log
    daemon -f -o /var/log/atrium/portcullisd-navigator.log /usr/local/bin/portcullisd --socket $PSOCK; sleep 3
    pgrep -f atrium-jaild >/dev/null && test -S $PSOCK" || die "jaild restart / portcullisd start"
echo "  jaild (rctl allowed) and portcullisd on $PSOCK"

echo "=== binaries"
g "pkill -x navigator-ui; pkill -x forum-wm; pkill -x limend; pkill -x navigatord; pkill -x frescod; sleep 1; rm -rf $S; mkdir -p $D $S /var/run/atrium; chmod 777 $S; true"
for b in "$ROOT/frescod/target/$R/frescod" "$ROOT/frescod/target/$R/fresco-renderer" "$ROOT/forum-wm/target/$R/forum-wm" \
         "$ROOT/navigator-ui/target/$R/navigator-ui" "$ROOT/navigator-page/target/$R/navigator-page" \
         "$ROOT/limen/target/$R/limend" "$ROOT/navigator-prerender/target/$R/prerender" \
         "$ROOT/navigator-fetch/target/$R/navigator-fetchd" "$ROOT/navigatord/target/$R/navigatord" \
         "$ROOT/scripts/navigator-page-bundle.sh" "$ROOT/scripts/fresco-renderer-bundle.sh" \
         "$ROOT/scripts/navigator-broker-bundle.sh" "$ROOT/scripts/navigator-ui-bundle.sh" "$FIX/ca.pem" \
         "$ROOT/monstrator/target/$R/monstratord" "$ROOT/monstrator/target/$R/examples/monctl" \
         "$ROOT/monstrator/target/$R/examples/link-probe" \
         "$ROOT/atrium-open/target/$R/examples/open-probe" "$ROOT/scripts/monstrator-service.sh" \
         "$ROOT/monstrator/etc/services.d/38-atrium-monstratord.toml" \
         "$ROOT/portcullis/target/$R/atrium-portcullisd-bootstrap"; do
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
    pw usershow navtest >/dev/null 2>&1 || pw useradd navtest -m
    printf '%s\n' NAVIGATORD_SOCKET=/atrium/sockets/navigator/{instance}.sock \
        FRESCOD_CHROME_SOCK=/atrium/sockets/fresco-chrome/fresco-chrome.sock \
        NAVIGATORD_ACTIVATION=/atrium/sockets/fresco-activation/fresco-activation.sock \
        NAVIGATORD_LIMEN=/atrium/sockets/limen/limend.sock PORTCULLIS_SOCKET=$PSOCK \
        NAVIGATORD_CONVERTER=jail:$CONV_ID NAVIGATORD_FETCHD=jail:$FETCH_ID \
        NAVIGATORD_STORE=/var/db/navigatord/store NAVIGATORD_DOWNLOADS=/var/db/navigatord/downloads \
        NAVIGATORD_UI=jail:org.atrium.navigator.ui NAVIGATORD_TRAILS=/var/db/navigatord/trails.json \
        NAVIGATORD_SCHEMES=mailto \
        NAVIGATOR_UI_TRACE=1 NAVIGATOR_UI_LIVE=1 NAVIGATOR_UI_OPEN=https://atlas.example/wiki/Atrium > /root/navigatord.conf
    chmod +x $D/navigator-ui
    sh $D/navigator-ui-bundle.sh $D/navigator-ui /root/e2e-key.pem /usr/local/bin/opifex
    chmod +x $D/navigatord
    sh $D/navigator-broker-bundle.sh $D/navigatord /root/navigatord.conf /root/e2e-key.pem /usr/local/bin/opifex
    # A mail app for the hand-off (opening-links.md L4): atrium-open's
    # open-probe, taking mailto: links.
    rm -rf /root/mail-bundle && mkdir -p /root/mail-bundle/bin && cp $D/open-probe /root/mail-bundle/bin/
    printf '[app]\nid = \"org.test.mail\"\nname = \"Test mail\"\nversion = \"1\"\nentry = \"bin/open-probe\"\n\n[[links]]\nschemes = [\"mailto\"]\n' > /root/mail-bundle/atrium.toml
    openssl dgst -sha256 -sign /root/e2e-key.pem -out /root/mail-bundle/atrium.toml.sig /root/mail-bundle/atrium.toml
    opifex install /root/mail-bundle
    # A second one: with two and no default, Monstrator asks in its sheet.
    rm -rf /root/mail2-bundle && cp -R /root/mail-bundle /root/mail2-bundle
    printf '[app]\nid = \"org.test.mail2\"\nname = \"Test mail 2\"\nversion = \"1\"\nentry = \"bin/open-probe\"\n\n[[links]]\nschemes = [\"mailto\"]\n' > /root/mail2-bundle/atrium.toml
    openssl dgst -sha256 -sign /root/e2e-key.pem -out /root/mail2-bundle/atrium.toml.sig /root/mail2-bundle/atrium.toml
    opifex install /root/mail2-bundle
    rm -f /var/db/atrium/navtest/links.toml
    # navtest has a Work persona (personas.md §2) for the Work Navigator.
    mkdir -p /var/db/atrium/navtest; P=/var/db/atrium/navtest/policy.toml
    grep -q '^.personas.work.' \$P 2>/dev/null || printf '\n[personas.work]\ncolour = \"blue\"\n' >> \$P
    # Only these two take mailto: here (vm-apps-access-check.sh's own may linger).
    for a in org.test.new org.test.sys; do opifex uninstall \$a >/dev/null 2>&1 || true; done
    chmod +x $D/monstratord $D/monctl $D/link-probe
    # An app with a window that opens a web link with a press in it.
    rm -rf /root/linkprobe-bundle && mkdir -p /root/linkprobe-bundle/bin && cp $D/link-probe /root/linkprobe-bundle/bin/
    printf '[app]\nid = \"org.test.linkprobe\"\nname = \"Link probe\"\nversion = \"1\"\nentry = \"bin/link-probe\"\n\n[capabilities]\ngraphics = \"fresco\"\nopen-uri = true\n' > /root/linkprobe-bundle/atrium.toml
    openssl dgst -sha256 -sign /root/e2e-key.pem -out /root/linkprobe-bundle/atrium.toml.sig /root/linkprobe-bundle/atrium.toml
    opifex install /root/linkprobe-bundle
    portcullis policy grant --system org.test.linkprobe
    sh $D/monstrator-service.sh $D/monstratord $D/38-atrium-monstratord.toml" > "$OUT.bundles.log" 2>&1 \
    || die "bundles: $(tail -5 "$OUT.bundles.log")"
for a in org.test.mail:open-probe org.test.mail2:open-probe org.atrium.navigator:navigatord org.atrium.navigator.ui:navigator-ui $PAGE_ID:navigator-page $CONV_ID:prerender $FETCH_ID:navigator-fetchd org.atrium.fresco.renderer:fresco-renderer; do
    g "test -x /var/lib/atrium/apps/${a%%:*}/bin/${a#*:}" && ok "${a%%:*} installed" || bad "${a%%:*} not installed"
done
g "awk '/^\\[workers.\"$PAGE_ID\"\\]/ {f=1; next} /^\\[/ {f=0} f && /^per_instance *= *true/ {ok=1} END {exit !ok}' /etc/atrium/policy.toml" && ok "worker rule: _limen starts the page as a uid of the instance it serves" || bad "no per-instance worker rule for $PAGE_ID"

echo "=== start"
# frescod as _frescod, renderers from portcullisd (no FRESCOD_RENDERER_LAUNCHER):
# the worker rule runs each as _fresco-render.
# frescod's activation and chrome sockets and limend's are in directories of
# their own under /atrium/sockets, which the Navigator's jail mounts.
# frescod's own socket where jails find it (Monstrator's sheet, §6).
FS=/atrium/sockets/fresco/fresco.sock
AS=/atrium/sockets/fresco-activation/fresco-activation.sock
CS=/atrium/sockets/fresco-chrome/fresco-chrome.sock
LS=/atrium/sockets/limen/limend.sock
# navigatord's control socket, named by its uid (shared with any other
# Navigator instance's).
NS="/atrium/sockets/navigator/[0-9]*.sock"
g "mkdir -p /atrium/sockets/fresco /atrium/sockets/fresco-activation /atrium/sockets/fresco-chrome /atrium/sockets/limen /atrium/sockets/navigator \
      /atrium/sockets/navigator-page /atrium/sockets/arca /atrium/sockets/arca-courier
   chown _frescod /atrium/sockets/fresco /atrium/sockets/fresco-activation /atrium/sockets/fresco-chrome; chown _limen /atrium/sockets/limen
   rm -f /atrium/sockets/navigator/*.sock /atrium/sockets/navigator-page/*.sock
   # Bound by the Navigator's per-app uid, which portcullisd allocates at its
   # first launch: open to it here (portcullisd making a served socket's
   # directory the app's own is a later step).
   chmod 1777 /atrium/sockets/navigator /atrium/sockets/navigator-page"
g "cd $S && env FRESCOD_SPLIT=1 FRESCOD_RENDERER=compute \
    FRESCOD_APP_REGISTRY=$S/none FRESCOD_HEADLESS_PNG=$S/screen FRESCOD_SOCK=$FS FRESCOD_WM_SOCK=$S/wm.sock \
    FRESCOD_INPUT_SOCK=$S/inject.sock FRESCOD_INJECT_ACTIVATION=1 FRESCOD_TOKEN_LIFE_MS=15000 FRESCOD_CHROME_SOCK=$CS FRESCOD_SLOT_UIDS=65022 FRESCOD_ACTIVATION_SOCK=$AS \
    daemon -f -o $S/frescod.log -u _frescod $D/frescod </dev/null"
i=0; while [ $i -lt 60 ] && ! g "test -S $FS && test -S $AS"; do sleep 2; i=$((i+1)); done
# Monstrator, the link opener (opening-links.md §6), as the system service
# it is: portcullisd's bootstrap starts it from its services.d manifest, the
# only one in this run's services directory.
MS=/atrium/sockets/monstrator/monstrator.sock
g "pkill -f atrium-portcullisd-bootstrap; sleep 2; install -m 755 $D/atrium-portcullisd-bootstrap /usr/local/bin/atrium-portcullisd-bootstrap
   rm -rf $S/svc && mkdir -p $S/svc && cp /etc/atrium/services.d/38-atrium-monstratord.toml $S/svc/ && rm -f $MS /var/log/atrium/atrium-monstratord.log
   daemon -f -o $S/bootstrap.log /usr/local/bin/atrium-portcullisd-bootstrap --services-dir $S/svc </dev/null"
i=0; while [ $i -lt 30 ] && ! g "test -S $MS"; do sleep 2; i=$((i+1)); done
MON=$(g "ps -o jid= -o uid= -p \$(pgrep -f atrium-monstratord | head -1)" | tr -d '\r' | awk '{print $1, $2}')
MJID=${MON%% *}; MUID=${MON#* }
[ -n "$MJID" ] && [ "$MJID" != 0 ] && [ "$MUID" = 65038 ] && ok "monstratord runs in a jail (jid $MJID) as _monstrator (65038)" || bad "monstratord: '$MON' $(g "tail -3 $S/bootstrap.log")"
MM=$(g "mount -p | awk '{print \$2}' | grep '/jails/monstratord/atrium/sockets' | sed 's#.*/atrium/sockets/#sockets/#' | sort | tr '\n' ' '" | tr -d '\r')
[ "$MM" = "sockets/fresco sockets/fresco-activation sockets/monstrator sockets/portcullis " ] && ok "its jail reaches its own socket, portcullisd's and frescod's two, no other" || bad "monstratord's socket mounts: '$MM'"
# A services.d jail without network: no addresses at all (ip4 and ip6
# disabled), or a stack of its own.
MN=$(g "jls -n -j ${MJID:-0} ip4 ip6 vnet 2>&1" | tr -d '\r')
case "$MN" in *vnet=new*|*ip4=disable*ip6=disable*) ok "no network in its jail ($MN)" ;; *) bad "monstratord's jail network: '$MN'" ;; esac
r=$(g "MONSTRATOR_SOCKET=$MS $D/monctl 'javascript:alert(1)'")
case "$r" in *"never opened in an app"*) ok "a javascript: link is refused before anything is asked" ;; *) bad "javascript: $r" ;; esac
r=$(g "MONSTRATOR_SOCKET=$MS $D/monctl 'mailto:a@b.example'")
case "$r" in *"not a launched app"*) ok "portcullisd takes Monstrator's ask, and refuses an asker that is no app" ;; *) bad "root's mailto: $r" ;; esac
g "grep -q 'renderers are org.atrium.fresco.renderer jails' $S/frescod.log" && ok "frescod (as _frescod) starts renderers as jails" || bad "frescod: $(g "grep -i renderer $S/frescod.log | head -3 | tr '\n' ';'")"
g "cd $S && env FRESCO_SOCKET=$FS FRESCO_WM_SOCKET=$S/wm.sock FORUM_CTL_SOCKET=$S/ctl.sock FORUM_STATE=$S/forum-state daemon -f -o $S/wm.log $D/forum-wm </dev/null"
# limend as _limen: the Navigator is navigatord, launched by navtest, so
# portcullisd's launch registry names its per-app uid (navtest,
# org.atrium.navigator); navigatord opens both the UI's frescod and limend
# connections, so the UI's windows and its limend parent are that uid's.
g "cd $S && env LIMEN_SOCKET=$LS LIMEN_DEFAULTS=doc-viewer=$PAGE_ID \
    LIMEN_FRESCOD_ACTIVATION=$AS daemon -f -o $S/limend.log -u _limen $D/limend </dev/null"
# navigatord as the Navigator app (navigator-ui.md §2.3a): navtest launches
# it, portcullisd runs it as the Navigator's per-app uid in its jail, with
# its settings from the bundle. Its converter and fetcher jails run as that
# uid too (a worker runs as whoever asks, and never as root).
g "cd $S && daemon -f -o $S/navigatord.log -u navtest /usr/local/bin/portcullis launch --no-prompt org.atrium.navigator </dev/null"
i=0; while [ $i -lt 30 ] && ! g "test -S $LS && test -S $NS"; do sleep 2; i=$((i+1)); done
g "test -S $LS && test -S $NS" || { g "tail -5 $S/limend.log $S/navigatord.log /var/log/atrium/portcullisd-navigator.log"; die "limend or navigatord did not start"; }
NAV=$(g "ps -o jid= -o uid= -p \$(pgrep -x navigatord | head -1)" | tr -d '\r' | awk '{print $1, $2}')
NJID=${NAV%% *}; NUID=${NAV#* }
NAVTEST=$(g "id -u navtest" | tr -d '\r')
[ -n "$NJID" ] && [ "$NJID" != 0 ] && ok "navigatord runs in a jail (jid $NJID)" || bad "navigatord is not in a jail: '$NAV'"
[ -n "$NUID" ] && [ "$NUID" -ge 50000 ] && [ "$NUID" != "$NAVTEST" ] && ok "as the Navigator's own uid ($NUID), not navtest's ($NAVTEST)" || bad "navigatord's uid: '$NUID'"
g "grep -q \"^$NUID navtest org.atrium.navigator\" /var/run/atrium/app-registry" && ok "the launch registry names it (navtest, org.atrium.navigator)" || bad "registry: $(g "grep -a navigator /var/run/atrium/app-registry")"
NM=$(g "mount -p | awk '{print \$2}' | grep -E \"/jails/[^/]*navigator[^/.]*/\" | grep -v -e '/dev\$' -e '/tmp\$' | sed 's#.*/atrium/sockets/#sockets/#' | sort | tr '\n' ' '" | tr -d '\r')
echo "  navigator jail mounts: $NM"
for d in navigator navigator-page fresco-chrome fresco-activation limen portcullis arca arca-courier; do
    echo "$NM" | grep -q "sockets/$d " || bad "no $d socket directory in the Navigator's jail"
done
echo "$NM" | grep -qE 'sockets/(fresco|forum|clipboard|audio)( |$)' && bad "the Navigator's jail has a socket it was not given" || ok "the Navigator's jail has its broker's sockets and no others"
# The jail has no ifconfig; its interfaces, as the host sees its vnet.
VN=$(g "jls -j $NJID vnet 2>&1" | tr -d '\r')
case "$VN" in new|1) ok "no network: a network stack of its own with nothing in it (vnet $VN)" ;; *) bad "the Navigator's jail is on the host's network stack (vnet '$VN')" ;; esac
g "grep -q UNCONFINED $S/navigatord.log" && bad "navigatord says it is unconfined" || ok "navigatord runs confined (no testing switch, no fixtures)"
g "grep -q 'taking links from other apps' $S/navigatord.log" && ok "…and takes web links from other apps on its document channel" || bad "no document channel: $(g "grep -i 'document channel' $S/navigatord.log | tail -2")"
mkdir -p "$OUT"
# The UI is navigatord's worker: what it says goes to navigatord's log.
UL=$S/navigatord.log

shot() { g "cp $S/screen.png $S/shot.png"; get $S/shot.png "$OUT/$1.png" && echo "  frame: $OUT/$1.png"; }
inj() { g "printf '$1\\n' | nc -N -U $S/inject.sock"; }
press() { inj "MOVE $1 $2"; sleep 1; inj "BTN 1 1"; sleep 1; inj "BTN 1 0"; sleep "$GAP"; }
last() { g "grep 'trace' $UL | tail -1"; }
until_trace() { i=0; while [ $i -lt "$WAIT" ]; do last | grep -q "$1" && return 0; sleep 3; i=$((i+3)); done; return 1; }
# The page jails and who runs in them.
jails() { g "jls name | grep -c 'limen-' || true" | tr -d '\r'; }

echo "=== drive"
# navigatord started the UI as its worker (navigator-ui.md §2.3a J3), with
# its NAVIGATOR_UI_* settings from the bundle's config.
until_trace 'url=https://atlas.example/wiki/Atrium live=shown' && ok "the first page opened over the network" || bad "first page: $(last)"
UI=$(g "ps -o jid= -o uid= -p \$(pgrep -x navigator-ui | head -1)" | tr -d '\r' | awk '{print $1, $2}')
UJID=${UI%% *}; UUID=${UI#* }
[ -n "$UJID" ] && [ "$UJID" != 0 ] && [ "$UJID" != "$NJID" ] && ok "the UI runs in a jail of its own (jid $UJID)" || bad "the UI's jail: '$UI' (navigatord's $NJID)"
# A uid of navtest's Personal Navigator's own, not navigatord's, and named
# in the launch registry as its window (per-instance worker rule).
[ -n "$UUID" ] && [ "$UUID" -ge 50000 ] && [ "$UUID" != "$NUID" ] && g "grep -qx '$UUID navtest org.atrium.navigator.ui' /var/run/atrium/app-registry" \
    && ok "as a uid of its own ($UUID), navtest's Navigator window in the registry" || bad "the UI's uid: '$UUID' $(g "grep -a navigator.ui /var/run/atrium/app-registry" | tr '\n' ';')"
UM=$(g "mount -p | awk '{print \$2}' | grep -E \"/jails/[^/]*navigator[.-]ui[^/]*/\" | grep -c /atrium/sockets || true" | tr -d '\r')
[ "${UM:-1}" = 0 ] && ok "and with no sockets: navigatord passes it what it reaches" || bad "the UI's jail mounts $UM socket directories"
g "grep -q 'kept by navigatord' $UL" && ok "the UI's trails came from navigatord" || bad "trails: $(g "grep -h 'trail store' $UL | tail -2 | tr '\n' ';'")"
sleep "$GAP"
g "grep -q 'laid out \"Atrium (architecture)\"' $UL" && ok "the jailed child redeemed its ticket and laid the page out" \
    || bad "child: $(g "grep -h -e navigator-page -e limend $S/limend.log | tail -4 | tr '\n' ';'")"
PS=$(g "ps -axo user,uid,jid,comm | awk '\$4==\"navigator-page\"'")
echo "$PS" | sed 's/^/    /'
PU=$(g "awk '\$2==\"navtest\" && \$3==\"$PAGE_ID\" && NF==3 {print \$1}' /var/run/atrium/app-registry" | tr -d '\r')
[ -n "$PU" ] && echo "$PS" | awk -v u="$PU" '$2==u && $3!=0 {f=1} END {exit !f}' && ok "navigator-page runs inside a jail as navtest's page uid ($PU)" || bad "navigator-page: '$PU' $PS"
echo "$PS" | awk -v u="$PU" '$2!=u || $3==0 {f=1} END {exit f}' && ok "no navigator-page outside a jail or as another user" || bad "a navigator-page outside: $PS"
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
g "grep -q '$PAGE_ID for navtest.s org.atrium.navigator runs as atrium-app-$PU' /var/log/atrium/portcullisd-navigator.log" && ok "portcullisd applied the worker rule, for the Navigator instance the page serves" || bad "no worker rule line: $(g "grep -h '$PAGE_ID' /var/log/atrium/portcullisd-navigator.log | tail -2" | tr '\n' ';')"
# Images: the converter's verified bytes, kept by navigatord with the page,
# handed over with the ticket, decoded in the page's jail.
g "grep -q 'navigator-page: 1 images with the page' $S/limend.log" && ok "the page's image came with its ticket" || bad "no image with the page: $(g "grep -h -i image $S/limend.log $S/navigatord.log | tail -3 | tr '\n' ';'")"
g "grep -q 'an image not shown' $S/limend.log" && bad "an image was refused: $(g "grep 'an image not shown' $S/limend.log | head -2 | tr '\n' ';'")" || ok "and nothing refused decoding it"
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
g "grep -q 'laid out \"Impluvium' $UL" && ok "the next page drew in a new jail" || bad "Impluvium: $(g "grep -h navigator-page $S/limend.log | tail -3 | tr '\n' ';'")"
shot confined-2-impluvium
# Back to Atrium, then its in-page link (the fourth nav line, #plan):
# navigatord says it is in the page, the UI asks the page to scroll to it.
press 270 $((TOP + 20))     # the toolbar's Back
until_trace 'url=https://atlas.example/wiki/Atrium live=shown' && ok "Back went to the parent page" || bad "back: $(last)"
# A hand-off (opening-links.md §7, L4): the mailto: link asks in the prompt
# row; Open sends it with the press to Monstrator, portcullisd spends the
# press (purpose LINK, the row's guard seen clear). Two apps take mailto:
# and neither is the default, so Monstrator asks in its own sheet (L3):
# keys only (the sheet has them as the new dialog), Always use ticked, the
# first app picked with Enter, which frescod counts (PROMPT, its own
# window, the rows its guard). portcullisd starts that app with the link
# on its document channel and makes it the default; the next mailto: link
# goes straight to it.
ML=/var/log/atrium/atrium-monstratord.log
PL=/var/log/atrium/portcullisd-navigator.log
key() { inj "KEY $1 1 0"; sleep 1; inj "KEY $1 0 0"; sleep 3; }
links() { g "grep -c 'open-probe: link url=mailto:curator@atlas.example\$' $PL" | tr -d '\r '; }
press $((PX + 50)) $((TOP + PY + 64 + 20 + 5 * 40))    # mailto:
until_trace 'prompt=handoff' && ok "a mailto: link asked before going anywhere" || bad "mailto: $(last)"
W=$(last | sed -n 's/.* w=\([0-9]*\).*/\1/p')
press $((W - 40)) $((TOP + 41 + 22))                   # Open
i=0; while [ $i -lt 60 ] && ! g "grep -q 'asking which of 2 apps' $ML"; do sleep 3; i=$((i+3)); done
g "grep -q 'asking which of 2 apps opens a mailto link' $ML" && ok "two apps take mailto: and none is the default: Monstrator asks in its sheet" || bad "no sheet: $(g "tail -3 $ML")"
sleep 3
shot confined-4-sheet
key 0x51; key 0x51        # Down, Down: Always use
key 0x2c                  # Space: ticked
key 0x52; key 0x52        # Up, Up: Test mail
key 0x28                  # Enter
i=0; while [ $i -lt 90 ] && [ "$(links)" != 1 ]; do sleep 3; i=$((i+3)); done
[ "$(links)" = 1 ] && ok "…the pick counted: the mail app got the link, through Monstrator and portcullisd" \
    || bad "hand-off: $(g "grep -h 'not handed on' $UL | tail -1; tail -3 $ML; grep -h -e link -e LINK $PL | tail -3" | tr '\n' ';')"
g "grep -q 'a mailto link from org.atrium.navigator opened in org.test.mail for navtest' $PL" && ok "…from the Navigator, for navtest, to org.test.mail" || bad "portcullisd: $(g "grep -h link $PL | tail -3" | tr '\n' ';')"
g "grep -q 'handed mailto:curator@atlas.example on' $UL" && ok "…and the Navigator was told it was opened" || bad "UI: $(g "grep -h 'handed on' $UL | tail -2")"
g "grep -qx 'mailto = \"org.test.mail\"' /var/db/atrium/navtest/links.toml" && ok "Always use: org.test.mail is navtest's mailto: default" || bad "links.toml: $(g "cat /var/db/atrium/navtest/links.toml 2>&1")"
until_trace 'prompt=- ' || bad "the row stayed: $(last)"
press $((PX + 50)) $((TOP + PY + 64 + 20 + 5 * 40))    # mailto: again
until_trace 'prompt=handoff' || bad "mailto again: $(last)"
press $((W - 40)) $((TOP + 41 + 22))                   # Open
i=0; while [ $i -lt 90 ] && [ "$(links)" != 2 ]; do sleep 3; i=$((i+3)); done
[ "$(links)" = 2 ] && ok "the next mailto: link went straight to the default, no sheet" || bad "second hand-off: $(links) links; $(g "tail -3 $ML" | tr '\n' ';')"
g "grep -c 'asking which' $ML" | grep -qx 1 && ok "…asked once" || bad "sheets: $(g "grep -c 'asking which' $ML")"
g "grep -h -e monstratord $S/bootstrap.log $ML 2>/dev/null | grep -q curator" && bad "monstratord logged the link itself" || ok "nothing logged the address but the app"
press 270 $((TOP + 20)) >/dev/null 2>&1; press 270 $((TOP + 20)) >/dev/null 2>&1; true
until_trace 'url=https://atlas.example/wiki/Atrium live=shown' || bad "back on Atrium: $(last)"
press $((PX + 50)) $((TOP + PY + 64 + 20 + 120))
g "grep -q 'in-page link #plan' $UL" && ok "the in-page link stayed in the page" || bad "in-page: $(last)"
g "grep -q 'scrolled to #plan at y=' $S/limend.log" && ok "the page scrolled to #plan" || bad "no scroll: $(g "grep -h -e '#plan' -e anchor $S/limend.log | tail -2 | tr '\n' ';'")"
shot confined-3-plan
# Back on Atrium with a budget of one, Impluvium was parked: its jail
# ended, and one page jail is left (one per live page).
N=$(jails)
[ "${N:-0}" = 1 ] && ok "the parked page's jail ended: one live page, one page jail" || bad "$N page jails for one live page"

sleep 3
# From the host, through the jail's root (the jail has no test(1)).
g "test -s \$(jls -j $NJID path)/var/db/navigatord/trails.json" && ok "navigatord keeps the trails in its jail" || bad "no trails.json in the Navigator's jail"

echo "=== a web link from another app (opening-links.md §5)"
# link-probe, an app of navtest's with a window and open-uri: Enter in its
# window sends the fixture's Impluvium page to Monstrator with that press;
# https is the Navigator's (the system's default), so portcullisd hands it
# to the running Navigator on its document channel, and the UI opens it in
# a trail of its own.
T0=$(last | sed -n 's/.* trails=\([0-9]*\) .*/\1/p')
g "cd $S && daemon -f -o $S/linkprobe.log -u navtest /usr/local/bin/portcullis launch --no-prompt org.test.linkprobe </dev/null"
i=0; while [ $i -lt 60 ] && ! g "grep -q 'link-probe: up' $S/linkprobe.log"; do sleep 3; i=$((i+3)); done
g "grep -q 'link-probe: up' $S/linkprobe.log" && ok "link-probe is up in a jail of its own" || bad "link-probe: $(g "tail -3 $S/linkprobe.log")"
sleep 10
key 0x28
i=0; while [ $i -lt 90 ] && ! g "grep -q -e 'link-probe: opened' -e 'link-probe: not opened' $S/linkprobe.log"; do sleep 3; i=$((i+3)); done
g "grep -q 'link-probe: opened' $S/linkprobe.log" && ok "Monstrator took its link with the press, and portcullisd handed it on" || bad "link-probe: $(g "tail -2 $S/linkprobe.log; tail -2 $ML; grep -h link $PL | tail -2" | tr '\n' ';')"
g "grep -q 'a https link from org.test.linkprobe opened in org.atrium.navigator for navtest' $PL" && ok "…to the Navigator, the default for https" || bad "portcullisd: $(g "grep -h link $PL | tail -3" | tr '\n' ';')"
until_trace "trails=$((${T0:-0} + 1)) .*url=https://atlas.example/wiki/Impluvium live=shown" && ok "the Navigator opened it in a trail of its own" || bad "incoming: $(last); $(g "grep -h 'other app' $UL | tail -2")"
# The Navigator was running: portcullisd asks frescod to bring its window
# forward (once per LINK press it spent), and forum-wm does, over
# link-probe's window.
g "grep -q 'org.atrium.navigator.s window [0-9]* brought forward' $PL" && ok "…and its window came forward" || bad "raise: $(g "grep -h 'brought forward' $PL | tail -2; grep -h 'link was handed' $S/wm.log | tail -1")"
g "grep -q 'a link was handed to surface' $S/wm.log" && ok "…by forum-wm, on frescod's word" || bad "forum-wm: $(g "tail -3 $S/wm.log")"
sleep 3
shot confined-5-incoming
g "pkill -x link-probe; true"

echo "=== a Work Navigator beside the Personal one (personas.md §3)"
# navtest's Work persona: a second Navigator, its own instance (uid, jail,
# overlay), with its own page socket in the shared directory. Its page
# children redeem its tickets there, not the Personal one's.
WL=$S/navigatord-work.log
g "cd $S && daemon -f -o $WL -u navtest /usr/local/bin/portcullis launch --no-prompt --persona work org.atrium.navigator </dev/null"
i=0; while [ $i -lt 90 ] && ! g "grep -q 'url=https://atlas.example/wiki/Atrium live=shown' $WL"; do sleep 3; i=$((i+3)); done
WNAV=$(g "for p in \$(pgrep -x navigatord); do ps -o jid= -o uid= -p \$p; done | awk '\$2 != $NUID {print \$1, \$2; exit}'" | tr -d '\r')
WJID=${WNAV%% *}; WUID=${WNAV#* }
[ -n "$WUID" ] && [ "$WUID" != "$NUID" ] && [ "$WJID" != "$NJID" ] && [ "$WJID" != 0 ] && ok "a second navigatord, in a jail of its own (jid $WJID) as another uid ($WUID)" || bad "Work navigatord: '$WNAV' $(g "tail -3 $WL" | tr '\n' ';')"
g "grep -q \"^$WUID navtest org.atrium.navigator .*persona=work\" /var/run/atrium/app-registry" && ok "the launch registry names it Work's" || bad "registry: $(g "grep -a navigator /var/run/atrium/app-registry" | tr '\n' ';')"
g "grep -q 'navigatord: persona work (blue)' $WL" && ok "portcullisd told it its persona, by its uid" || bad "persona: $(g "grep -h persona $WL | tail -2" | tr '\n' ';')"
g "grep -q 'navigatord: persona Personal' $S/navigatord.log" && ok "…and the first one that it is Personal" || bad "Personal: $(g "grep -h persona $S/navigatord.log | tail -2" | tr '\n' ';')"
g "grep -q 'window persona Work' $WL" && ok "its rail names the window's persona Work" || bad "UI persona: $(g "grep -h 'persona' $WL | tail -2" | tr '\n' ';')"
g "test -S /atrium/sockets/navigator-page/$NUID.sock && test -S /atrium/sockets/navigator-page/$WUID.sock" && ok "two page sockets, one per instance ($NUID, $WUID)" || bad "page sockets: $(g "ls /atrium/sockets/navigator-page/")"
g "test -S /atrium/sockets/navigator/$NUID.sock && test -S /atrium/sockets/navigator/$WUID.sock" && ok "two control sockets" || bad "control sockets: $(g "ls -l /atrium/sockets/navigator/")"
CM=$(g "stat -f '%Lp %u' /atrium/sockets/navigator/$WUID.sock" | tr -d '\r')
[ "$CM" = "600 $WUID" ] && ok "Work's control socket is Work's uid's alone" || bad "control socket mode: '$CM'"
sleep "$GAP"
NL=$(g "grep -c 'laid out \"Atrium (architecture)\"' $WL" | tr -d '\r ')
[ "${NL:-0}" -ge 1 ] && ok "Work's page child redeemed its ticket at Work's socket and laid the page out" || bad "Work's page: $NL layouts; $(g "grep -h -e redeem -e 'unknown ticket' $S/limend.log $WL $S/navigatord.log | tail -3" | tr '\n' ';')"
g "grep -q 'unknown ticket' $S/navigatord.log $WL" && bad "a ticket went to the other instance" || ok "no ticket went to the other instance"
# Work's window and pages run as Work's own uids (per-instance worker rules).
WUU=$(g "awk '\$2==\"navtest\" && \$3==\"org.atrium.navigator.ui\" && \$4==\"persona=work\" {print \$1}' /var/run/atrium/app-registry" | tr -d '\r')
WPU=$(g "awk '\$2==\"navtest\" && \$3==\"$PAGE_ID\" && \$4==\"persona=work\" {print \$1}' /var/run/atrium/app-registry" | tr -d '\r')
[ -n "$WUU" ] && [ "$WUU" != "$UUID" ] && g "pgrep -u $WUU -x navigator-ui >/dev/null" && ok "Work's window runs as Work's own uid ($WUU, Personal's $UUID)" || bad "Work's UI uid: '$WUU' (Personal '$UUID')"
[ -n "$WPU" ] && [ "$WPU" != "$PU" ] && g "grep -q '$PAGE_ID for navtest.s org.atrium.navigator (work) runs as atrium-app-$WPU' /var/log/atrium/portcullisd-navigator.log" && ok "Work's pages run as Work's own uid ($WPU, Personal's $PU)" || bad "Work's page uid: '$WPU' (Personal '$PU') $(g "grep -h '$PAGE_ID for' /var/log/atrium/portcullisd-navigator.log | tail -2" | tr '\n' ';')"
WC=$(g "grep -A5 'conn from uid=$WUID ' /var/log/atrium/portcullisd-navigator.log | grep -c -e '$FETCH_ID authorized' -e '$CONV_ID authorized'" | tr -d '\r ')
[ "${WC:-0}" -ge 2 ] && ok "Work's fetcher and converter jails ran as Work's uid" || bad "Work's workers: $(g "grep -h -e '$FETCH_ID' -e '$CONV_ID' /var/log/atrium/portcullisd-navigator.log | tail -3" | tr '\n' ';')"
g "test -s \$(jls -j $WJID path)/var/db/navigatord/trails.json" && ok "Work keeps its own trails, in its own jail" || bad "no trails.json in Work's jail"
g "grep -q 'taking links from other apps' $WL" && ok "Work's Navigator has a document channel of its own" || bad "Work's channel: $(g "grep -h -i 'document channel' $WL | tail -2" | tr '\n' ';')"
# A link from a Work app opens in the Work Navigator, never the Personal
# one (personas.md §3): link-probe, launched as Work's.
WLAST() { g "grep 'trace' $WL | tail -1"; }
WT0=$(WLAST | sed -n 's/.* trails=\([0-9]*\) .*/\1/p')
PT0=$(last | sed -n 's/.* trails=\([0-9]*\) .*/\1/p')
g "cd $S && daemon -f -o $S/linkprobe-work.log -u navtest /usr/local/bin/portcullis launch --no-prompt --persona work org.test.linkprobe </dev/null"
i=0; while [ $i -lt 60 ] && ! g "grep -q 'link-probe: up' $S/linkprobe-work.log"; do sleep 3; i=$((i+3)); done
g "grep -q 'link-probe: up' $S/linkprobe-work.log" && ok "Work's link-probe is up" || bad "Work link-probe: $(g "tail -3 $S/linkprobe-work.log")"
sleep 10
key 0x28
i=0; while [ $i -lt 90 ] && ! g "grep -q -e 'link-probe: opened' -e 'link-probe: not opened' $S/linkprobe-work.log"; do sleep 3; i=$((i+3)); done
g "grep -q 'link-probe: opened' $S/linkprobe-work.log" && ok "Monstrator took Work's link" || bad "Work link: $(g "tail -2 $S/linkprobe-work.log; grep -h link $PL | tail -2" | tr '\n' ';')"
g "grep -q 'a https link from org.test.linkprobe opened in org.atrium.navigator@work for navtest' $PL" && ok "…and portcullisd handed it to Work's Navigator" || bad "Work delivery: $(g "grep -h 'link from org.test.linkprobe' $PL | tail -2" | tr '\n' ';')"
i=0; while [ $i -lt 60 ] && ! WLAST | grep -q "trails=$((${WT0:-0} + 1)) .*url=https://atlas.example/wiki/Impluvium live=shown"; do sleep 3; i=$((i+3)); done
WLAST | grep -q "trails=$((${WT0:-0} + 1)) .*url=https://atlas.example/wiki/Impluvium live=shown" && ok "the Work window opened it in a trail of its own" || bad "Work incoming: $(WLAST)"
last | grep -q "trails=${PT0:-x} " && ok "the Personal window got nothing" || bad "Personal: $(last)"
[ "$(g "grep -c 'org.atrium.navigator.s window [0-9]* brought forward' $PL" | tr -d '\r ')" -ge 2 ] && ok "Work's window came forward" || bad "Work raise: $(g "grep -h -e 'brought forward' $PL | tail -2" | tr '\n' ';')"
g "pkill -x link-probe; true"
shot confined-6-work

echo "=== end"
g "pkill -x open-probe; pkill -f atrium-portcullisd-bootstrap; sleep 3; true"
g "jls name | grep -qx atrium-monstratord" && bad "monstratord's jail outlived the bootstrap" || ok "monstratord's jail ended with the bootstrap"
g "pkill -x navigator-ui"
sleep 15
g "pgrep -x navigatord >/dev/null" && bad "navigatord outlived its UI" || ok "the Navigator ended with its UI"
N=$(jails)
[ "${N:-0}" = 0 ] && ok "the page jails ended with the Navigator" || bad "$N page jails left"
LEFT=$(g "jls name | grep -cE 'navigator[-_](converter|fetcher)' || true" | tr -d '\r')
[ "${LEFT:-0}" = 0 ] && ok "no converter or fetcher jail left" || bad "$LEFT converter/fetcher jails left"
for f in ui limend navigatord navigatord-work frescod bootstrap; do get $S/$f.log "$OUT/$f.log" 2>/dev/null; done
get /var/log/atrium/portcullisd-navigator.log "$OUT/portcullisd.log" 2>/dev/null
g "pkill -x limend; pkill -x navigatord; pkill -x forum-wm; pkill -x frescod; true"
sleep 5
LEFT=$(g "jls name | grep -c 'fresco-renderer' || true" | tr -d '\r')
[ "${LEFT:-0}" = 0 ] && ok "no renderer jail left after frescod" || bad "$LEFT renderer jails left"
echo "navigator-confined: $fails failed"
[ "$fails" = 0 ]
