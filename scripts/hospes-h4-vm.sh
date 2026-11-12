#!/bin/sh
# Hospes H4 in a FreeBSD VM (cloud-vm.sh): Widevine end to end. Shaka's Widevine test stream
# (angel-one) plays through the player as `nobody`, the CDM installed by hospes-install from
# Google's signed Chrome package and the licence POSTed through Nuntius to Shaka's proxy.
#
#   ARCH=amd64 sh scripts/hospes-h4-vm.sh        # (ARCH=aarch64 works the same way)
#
# The container has no direct route out, so the guest's network gets a fixture address,
# 1.2.3.100, as scripts/nuntius-vm-check.sh does: its port 443 is a host-side relay holding a
# throwaway CA's certificate for the proxy's name, which forwards each POST to the real proxy
# through this environment's own HTTPS proxy. The guest maps the name there and trusts the CA.
#
# The script (re)boots the VM with that network if it runs without it, then boots the fork
# kernel (hospes-k1-vm.sh kernel, which needs SRC= the fork checkout; nextboot is one-shot). hospes_trap.ko must be built
# (hospes-k1-vm.sh test or hospes-kmod.sh). Downloads (not in the tree, never committed):
# Shaka's media into $VMDIR/shaka and Google's InRelease, Packages and Chrome .deb into
# $VMDIR/chrome (CHROME_DEB=path reuses a .deb that matches the Packages fetched).
#
# Nothing here runs the CDM on the host: it runs only in the guest, under Hospes.
# INSTALL_ONLY=1 stops after the install step, before anything runs the CDM.
set -eu
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
ARCH="${ARCH:-amd64}"
case "$ARCH" in
    aarch64|arm64) ARCH=aarch64; T=aarch64-unknown-freebsd; A=aarch64; KT=arm64; DEBARCH=arm64; VMDIR="${VMDIR:-$HOME/atrium-vm}" ;;
    *) ARCH=amd64; T=x86_64-unknown-freebsd; A=x86_64; KT=amd64; DEBARCH=amd64; VMDIR="${VMDIR:-$HOME/atrium-vm-amd64}" ;;
esac
S="$VMDIR/sysroot"
TU=$(echo "$T" | tr - _)
V="$ROOT/scripts/vssh"
PUB=1.2.3.100
PROXY=cwip-shaka-proxy.appspot.com
SHAKA=https://storage.googleapis.com/shaka-demo-assets/angel-one-widevine
APT=https://dl.google.com/linux/chrome/deb
MEDIA="v-0144p-0100k-libx264.mp4 v-0144p-0100k-vp9.webm a-eng-0096k-libopus-2c.webm dash.mpd"
FIX="$VMDIR/h4fix"
say() { printf '\n=== %s\n' "$*" >&2; }
die() { printf '\nFAILED: %s\n' "$*" >&2; exit 1; }

# ---- downloads ----------------------------------------------------------------
say "downloads"
mkdir -p "$VMDIR/shaka" "$VMDIR/chrome"
for f in $MEDIA; do
    [ -s "$VMDIR/shaka/$f" ] || curl -sSf -o "$VMDIR/shaka/$f" "$SHAKA/$f" || die "fetch $f"
done
curl -sSf -o "$VMDIR/chrome/InRelease" "$APT/dists/stable/InRelease" || die "fetch InRelease"
curl -sSf -o "$VMDIR/chrome/Packages" "$APT/dists/stable/main/binary-$DEBARCH/Packages" || die "fetch Packages"
# Which .deb the signed index names (verified on the host too: a bad index stops here).
(cd "$ROOT/hospes" && cargo build -q --release -p hospes-install) || die "hospes-install (host)"
IDX=$("$ROOT/hospes/target/release/hospes-install" widevine-index "$VMDIR/chrome/InRelease" "$VMDIR/chrome/Packages" "$DEBARCH") || die "index"
set -- $IDX
DEBPATH=$1 DEBSIZE=$2 DEBSHA=$3 DEBVER=$4
if [ -n "${CHROME_DEB:-}" ] && [ "$(sha256sum "$CHROME_DEB" | cut -d' ' -f1)" = "$DEBSHA" ]; then
    cp "$CHROME_DEB" "$VMDIR/chrome/chrome.deb"
elif [ ! -s "$VMDIR/chrome/chrome.deb" ] || [ "$(sha256sum "$VMDIR/chrome/chrome.deb" | cut -d' ' -f1)" != "$DEBSHA" ]; then
    curl -sSf -o "$VMDIR/chrome/chrome.deb" "$APT/$DEBPATH" || die "fetch $DEBPATH"
fi
echo "  chrome $DEBVER ($DEBSIZE bytes)" >&2

# ---- the licence relay ----------------------------------------------------------
say "licence relay ($PROXY at $PUB:443 -> the real proxy)"
mkdir -p "$FIX"
if [ ! -f "$FIX/ca.pem" ]; then
    openssl req -x509 -newkey ec -pkeyopt ec_paramgen_curve:prime256v1 -nodes -days 30 \
        -subj "/CN=atrium h4 fixture CA" -keyout "$FIX/ca.key" -out "$FIX/ca.pem" 2>/dev/null
    openssl req -newkey ec -pkeyopt ec_paramgen_curve:prime256v1 -nodes \
        -subj "/CN=$PROXY" -keyout "$FIX/srv.key" -out "$FIX/srv.csr" 2>/dev/null
    printf 'subjectAltName=DNS:%s\nbasicConstraints=CA:FALSE\n' "$PROXY" > "$FIX/ext"
    openssl x509 -req -in "$FIX/srv.csr" -CA "$FIX/ca.pem" -CAkey "$FIX/ca.key" -CAcreateserial \
        -days 30 -extfile "$FIX/ext" -out "$FIX/srv.pem" 2>/dev/null || die "fixture certs"
fi
cat > "$FIX/relay.py" <<'EOF'
import http.server, os, socketserver, ssl, sys, urllib.request
d, upstream = sys.argv[1], sys.argv[2]
cafile = os.environ.get("SSL_CERT_FILE") or "/root/.ccr/ca-bundle.crt"
class H(http.server.BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"
    def do_POST(self):
        body = self.rfile.read(int(self.headers.get("Content-Length", "0")))
        req = urllib.request.Request(upstream, data=body, method="POST",
                                     headers={"Content-Type": self.headers.get("Content-Type", "application/octet-stream")})
        try:
            with urllib.request.urlopen(req, timeout=20, context=ssl.create_default_context(cafile=cafile)) as r:
                status, out = r.status, r.read()
        except urllib.error.HTTPError as e:
            status, out = e.code, e.read()
        sys.stderr.write("POST %s %d bytes -> %d, %d bytes\n" % (self.path, len(body), status, len(out))); sys.stderr.flush()
        self.send_response(status); self.send_header("Content-Type", "application/octet-stream")
        self.send_header("Content-Length", str(len(out))); self.send_header("Connection", "close")
        self.end_headers(); self.wfile.write(out)
    def do_GET(self):
        self.send_response(404); self.send_header("Content-Length", "0"); self.send_header("Connection", "close"); self.end_headers()
    def log_message(self, f, *a): pass
class S(socketserver.ThreadingMixIn, http.server.HTTPServer): daemon_threads = True; allow_reuse_address = True
srv = S(("127.0.0.1", 18445), H)
ctx = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER); ctx.load_cert_chain(d + "/srv.pem", d + "/srv.key")
srv.socket = ctx.wrap_socket(srv.socket, server_side=True)
srv.serve_forever()
EOF
pkill -f "$FIX/relay.py" 2>/dev/null || true
python3 -I "$FIX/relay.py" "$FIX" "https://$PROXY/no_auth" > "$FIX/relay.log" 2>&1 &
RELAY=$!
trap 'kill $RELAY 2>/dev/null' EXIT INT TERM
printf '#!/bin/sh\nexec nc 127.0.0.1 18445\n' > "$FIX/fwd443"; chmod +x "$FIX/fwd443"
sleep 1
kill -0 "$RELAY" 2>/dev/null || die "relay: $(cat "$FIX/relay.log")"

# ---- the VM, with the fixture network and the fork kernel -----------------------------
say "boot"
# One VM at a time: the other architecture's goes down first.
case $ARCH in amd64) OTHER=aarch64 ;; *) OTHER=amd64 ;; esac
if pgrep -f "qemu-system" >/dev/null && ! pgrep -f "file=$VMDIR/vm.qcow2" >/dev/null; then
    ARCH=$OTHER sh "$ROOT/scripts/cloud-vm.sh" down >&2
fi
if pgrep -f "file=$VMDIR/vm.qcow2" >/dev/null && ! pgrep -f "guestfwd=tcp:$PUB:443-cmd:$FIX" >/dev/null; then
    ARCH=$ARCH sh "$ROOT/scripts/cloud-vm.sh" down >&2
fi
NETDEV_EXTRA="net=1.2.3.0/24,host=1.2.3.2,dns=1.2.3.3,guestfwd=tcp:$PUB:443-cmd:$FIX/fwd443" \
    ARCH=$ARCH sh "$ROOT/scripts/cloud-vm.sh" up >&2 || die "boot"
if [ "$("$V" 'sysctl -n kern.bootfile' | tr -d '\r')" != /boot/kernel.atrium/kernel ]; then
    ARCH=$ARCH sh "$ROOT/scripts/hospes-k1-vm.sh" kernel >&2 || die "fork kernel"
fi

# ---- cross-build ------------------------------------------------------------------
say "cross-build ($T)"
X="-C link-arg=--sysroot=$S -C link-arg=-L$S/lib -C link-arg=-L$S/usr/lib -C link-arg=--dynamic-linker=/libexec/ld-elf.so.1 -C link-arg=$S/usr/lib/Scrt1.o -C link-arg=$S/usr/lib/crti.o -C link-arg=$S/usr/lib/crtbeginS.o -C link-arg=-lc -C link-arg=$S/usr/lib/crtendS.o -C link-arg=$S/usr/lib/crtn.o"
sh "$ROOT/scripts/hospes-build.sh" cdm "$T" >/dev/null
(cd "$ROOT/hospes" && RUSTFLAGS="$X" cargo build --release -p hospes-install --target "$T" >&2)
(cd "$ROOT/atrium-player" && env PKG_CONFIG_ALLOW_CROSS=1 PKG_CONFIG_SYSROOT_DIR="$S" PKG_CONFIG_LIBDIR="$S/usr/local/libdata/pkgconfig" \
    "CC_$TU=clang --target=$T --sysroot=$S" "CFLAGS_$TU=-I$S/usr/local/include" RUSTFLAGS="$X -C link-arg=-L$S/usr/local/lib" \
    cargo build --release --target "$T" -p atrium-player -p atrium-player-decode >&2)
export "CC_$TU=clang" "AR_$TU=llvm-ar" "CFLAGS_$TU=--target=$T --sysroot=$S"
(cd "$ROOT/navigator-fetch" && RUSTFLAGS="$X" cargo +nightly build --quiet --release --target "$T" -Z build-std=std,panic_abort --bin nuntiusd >&2)
(cd "$ROOT/nuntius" && RUSTFLAGS="$X" cargo +nightly build --quiet --release --target "$T" -Z build-std=std,panic_abort --example fetcher-standin >&2)

# ---- copy and run -------------------------------------------------------------------
say "run in the guest"
B=$(mktemp -d)/h4vm
mkdir -p "$B/bin" "$B/media" "$B/chrome"
for f in hospes/target/$T/release/hospes-cdm hospes/target/$T/release/hospes-install \
         atrium-player/target/$T/release/atrium-player atrium-player/target/$T/release/atrium-player-decode \
         navigator-fetch/target/$T/release/nuntiusd nuntius/target/$T/release/examples/fetcher-standin; do
    cp "$ROOT/$f" "$B/bin/"
done
for f in "$B"/bin/*; do llvm-strip "$f" 2>/dev/null || true; done
for f in $MEDIA; do cp "$VMDIR/shaka/$f" "$B/media/"; done
cp "$VMDIR/chrome/InRelease" "$VMDIR/chrome/Packages" "$VMDIR/chrome/chrome.deb" "$B/chrome/"
cp "$FIX/ca.pem" "$ROOT/hospes/vm/h4-run.sh" "$ROOT/hospes/kmod/out/$KT-${KERNCONF:-ATRIUM-NODEBUG}/hospes_trap.ko" "$B/"
tar czf - -C "$(dirname "$B")" h4vm | "$V" 'rm -rf /tmp/h4vm && tar xzf - -C /tmp'
# A full host disk has cut a copy short without an error: check the largest file arrived whole.
[ "$("$V" 'sha256 -q /tmp/h4vm/chrome/chrome.deb' | tr -d '\r')" = "$DEBSHA" ] || die "the Chrome package did not reach the guest whole (host disk full?)"
rm -rf "$(dirname "$B")"
rc=0
"$V" "cd /tmp/h4vm && PUB=$PUB INSTALL_ONLY=${INSTALL_ONLY:-0} ${VIDEOS:+VIDEOS='$VIDEOS'} TRACE=${TRACE:-0} PLAY_TIMEOUT=${PLAY_TIMEOUT:-900} sh h4-run.sh" || rc=$?
echo "relay: $(grep -c '^POST' "$FIX/relay.log") licence POSTs"; grep '^POST' "$FIX/relay.log" | head -5
exit $rc
