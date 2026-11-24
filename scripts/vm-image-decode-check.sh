#!/bin/sh
# The system's image decoder (app-composition.md C6, dev-tools.md §3.5
# step 3). Runs on the HOST after
#   ARCH=amd64 sh scripts/cloud-vm.sh up
#   sh scripts/vm-image-decode-check.sh
# Ends "image-decode: N failed". No frescod.
#
# org.atrium.image-decode is installed with image-decode-bundle.sh. An app
# built with atrium-assets (sdk/atrium-assets/examples/assets_gate.rs),
# whose manifest names `decoders = ["image"]`, decodes its bundle's
# pictures through it and prints what came back:
#   - an SVG icon drawn at 48×48 and a 200×100 PNG fitted into 64×64
#     (64×32), with the colours they were made with;
#   - a broken PNG and an SVG with a script are the decoder's refusals;
#     a name out of assets/ is refused before anything is opened;
#   - the decoder runs as _imagedec, never as the app, and no decoder
#     jail is left after;
#   - an app without `decoders` has no worker-call socket, so no decoder;
#   - five decodes are timed, a jail each and then one Decoder for all
#     five (TCG: not a product number).
set -u
ROOT=$(cd "$(dirname "$0")/.." && pwd)
export ARCH=amd64
set -- env
. "$ROOT/scripts/cloud-vm.sh" >/dev/null
set --
K="-i $HOME/.ssh/fresco_bsd_ed25519 -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o LogLevel=ERROR"
V() { VSSH_CONNECT_TIMEOUT=30 "$ROOT/scripts/vssh" "$@"; }
fails=0
ok() { echo "  ok $1"; }
bad() { echo "  FAIL $1"; fails=$((fails+1)); }
die() { bad "$1"; echo "image-decode: $fails failed"; exit 1; }

echo "=== cross-build"
(cd "$ROOT/portcullis" && RUSTFLAGS="$CROSS_RUSTFLAGS" cargo build -q --release --target "$TARGET" \
    -p portcullisd -p portcullis-cli -p jaild -p opifex --bin portcullisd --bin portcullis --bin atrium-jaild --bin opifex) || die "portcullis build"
(cd "$ROOT/sdk" && RUSTFLAGS="$CROSS_RUSTFLAGS" cargo build -q --release --target "$TARGET" -p atrium-assets --example assets_gate) || die "sdk build"
(cd "$ROOT/atrium-image-decode" && RUSTFLAGS="$CROSS_RUSTFLAGS" cargo build -q --release --target "$TARGET") || die "decoder build"

echo "=== pictures"
FIX=$(mktemp -d)
python3 - "$FIX" <<'PY'
import struct, sys, zlib
d = sys.argv[1]
def png(w, h, px):
    raw = b"".join(b"\0" + b"".join(bytes(px(x, y)) for x in range(w)) for y in range(h))
    chunk = lambda t, b: struct.pack(">I", len(b)) + t + b + struct.pack(">I", zlib.crc32(t + b) & 0xffffffff)
    return b"\x89PNG\r\n\x1a\n" + chunk(b"IHDR", struct.pack(">IIBBBBB", w, h, 8, 6, 0, 0, 0)) + chunk(b"IDAT", zlib.compress(raw)) + chunk(b"IEND", b"")
open(d + "/photo.png", "wb").write(png(200, 100, lambda x, y: (0x30, 0x90, 0x40, 0xff)))
open(d + "/junk.png", "wb").write(b"\x89PNG\r\n\x1a\n" + b"\0" * 64)
open(d + "/app.svg", "w").write('<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24"><rect x="2" y="2" width="20" height="20" rx="4" fill="#c06030"/></svg>')
open(d + "/script.svg", "w").write('<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24"><script>alert(1)</script></svg>')
PY
[ -s "$FIX/photo.png" ] && ok "a PNG, a broken PNG, an icon and an SVG with a script" || die "fixtures"

echo "=== install"
B=/var/tmp/image-decode-gate
V "pkill -x portcullisd; sleep 1; rm -rf $B; mkdir -p $B; chmod 755 $B"
for f in "$ROOT/portcullis/target/$TARGET/release/portcullisd" "$ROOT/portcullis/target/$TARGET/release/portcullis" \
         "$ROOT/portcullis/target/$TARGET/release/atrium-jaild" "$ROOT/portcullis/target/$TARGET/release/opifex" \
         "$ROOT/sdk/target/$TARGET/release/examples/assets_gate" \
         "$ROOT/atrium-image-decode/target/$TARGET/release/atrium-image-decode" \
         "$ROOT/scripts/image-decode-bundle.sh" "$ROOT/etc/jaild.policy.toml" \
         "$FIX/photo.png" "$FIX/junk.png" "$FIX/app.svg" "$FIX/script.svg"; do
    scp -q $K -P 2222 "$f" root@localhost:$B/ || die "copy $f"
done
rm -rf "$FIX"
HL=$(mktemp)
PSOCK=/atrium/sockets/portcullis/portcullis.sock
LOG=$B/portcullisd.log
V 'sh -s' > "$HL" 2>&1 <<EOS
set -e
install -m 0755 $B/portcullisd /usr/local/bin/portcullisd
install -m 0755 $B/portcullis /usr/local/bin/portcullis
install -m 0755 $B/atrium-jaild /usr/local/bin/atrium-jaild
install -m 0755 $B/opifex /usr/local/bin/opifex
mkdir -p /etc/atrium /etc/atrium/publishers
cp $B/jaild.policy.toml /etc/atrium/jaild.policy.toml
# The decoder's memory and CPU limits are rctl rules.
grep -q '^allow_rctl' /etc/atrium/jaild.policy.toml || printf '\n[resource_control]\nallow_rctl = true\n' >> /etc/atrium/jaild.policy.toml
service atrium-jaild restart >/dev/null 2>&1 || service atrium-jaild start >/dev/null 2>&1 || true
test -f /root/e2e-key.pem || openssl ecparam -name prime256v1 -genkey -noout -out /root/e2e-key.pem
openssl ec -in /root/e2e-key.pem -pubout -out /etc/atrium/publishers/e2e.pem 2>/dev/null
cd $B && sh image-decode-bundle.sh $B/atrium-image-decode /root/e2e-key.pem /usr/local/bin/opifex
for a in org.example.pictures:'decoders = ["image"]' org.example.nopictures:''; do
    id=\${a%%:*}; cap=\${a#*:}; A=$B/bundle-\$id
    rm -rf \$A && mkdir -p \$A/bin \$A/assets/icons
    install -m 0755 $B/assets_gate \$A/bin/assets-gate
    cp $B/photo.png $B/junk.png $B/script.svg \$A/assets/; cp $B/app.svg \$A/assets/icons/
    printf '[app]\nid = "%s"\nname = "Pictures gate"\nversion = "1"\nentry = "bin/assets-gate"\n\n[capabilities]\n%s\n' "\$id" "\$cap" > \$A/atrium.toml
    openssl dgst -sha256 -sign /root/e2e-key.pem -out \$A/atrium.toml.sig \$A/atrium.toml
    /usr/local/bin/opifex install \$A
done
mkdir -p /atrium/sockets/portcullis; rm -f $PSOCK
: > $LOG
daemon -f -o $LOG /usr/local/bin/portcullisd --socket $PSOCK
sleep 3; test -S $PSOCK; chmod 666 $PSOCK
test -S /atrium/sockets/workers/workers.sock
EOS
[ $? = 0 ] && ok "the decoder and two apps installed; portcullisd up" || die "install: $(tail -5 "$HL")"

run() {  # app id -> its output
    V "rm -f $B/\$1.out" >/dev/null
    V "timeout 300 /usr/local/bin/portcullis launch --no-prompt $1 > $B/$1.out 2>&1; cat $B/$1.out"
}
line() { echo "$OUT" | grep "assets-gate $1 " | tail -1 | sed "s/.*assets-gate $1 //"; }

echo "=== an app with decoders = [\"image\"]"
OUT=$(run org.example.pictures)
echo "$OUT" | grep -q 'assets-gate done' && ok "it ran to the end" || bad "the app: $(echo "$OUT" | tail -3) $(V "grep -i 'system worker\|image-decode' $LOG | tail -3")"
r=$(line icon);   case "$r" in "ok 48x48 c06030ff") ok "the SVG icon at 48x48: $r" ;; *) bad "icon: $r" ;; esac
r=$(line photo);  case "$r" in "ok 64x32 309040ff") ok "the 200x100 PNG fitted into 64x64: $r" ;; *) bad "photo: $r" ;; esac
r=$(line junk);   case "$r" in "refused "*) ok "a broken PNG is refused: ${r#refused }" ;; *) bad "junk: $r" ;; esac
r=$(line script); case "$r" in "refused "*) ok "an SVG with a script is refused: ${r#refused }" ;; *) bad "script: $r" ;; esac
r=$(line escape); case "$r" in "error "*"not a file name"*) ok "a name out of assets/ is refused here" ;; *) bad "escape: $r" ;; esac
r=$(line timing); [ -n "$r" ] && ok "five decodes, a jail each: $r (TCG)" || bad "timing"
r=$(line session); case "$r" in "ok 5 pictures"*) ok "one Decoder for five: ${r#ok } (TCG)" ;; *) bad "session: $r" ;; esac
runs=$(V "grep -c 'system worker org.atrium.image-decode .* runs as _imagedec' $LOG")
[ "${runs:-0}" -ge 9 ] 2>/dev/null && ok "the decoder ran as _imagedec, a jail per call ($runs)" || bad "decoder runs: $runs $(V "grep -i 'system worker' $LOG | tail -3")"
core=$(line core-uid)
[ -n "$core" ] && [ "$core" != 65056 ] && ok "the app is uid $core, not the decoder's" || bad "core uid $core"
sleep 2
left=$(V "jls name | grep -c image-decode")
[ "${left:-1}" = 0 ] && ok "no decoder jail is left" || bad "jails left: $(V "jls name | grep image-decode")"

echo "=== an app without decoders"
OUT=$(run org.example.nopictures)
r=$(line icon); case "$r" in "error "*"socket is not there"*) ok "it has no worker-call socket: no decoder" ;; *) bad "no-decoders app: $r" ;; esac
n=$(V "grep -c 'image-decode .* for org.example.nopictures .*runs as' $LOG")
[ "${n:-1}" = 0 ] && ok "and portcullisd started nothing for it" || bad "started for it: $n"

echo "image-decode: $fails failed"
[ $fails = 0 ]
