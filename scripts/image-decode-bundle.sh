#!/bin/sh
# Build, sign and install the system's image decoder as a Portcullis bundle
# (org.atrium.image-decode; app-composition.md C6, dev-tools.md §3.5).
# Runs IN the FreeBSD guest:
#
#   sh image-decode-bundle.sh <atrium-image-decode binary> <signing key.pem> [opifex]
#
# An app whose manifest names `decoders = ["image"]` asks portcullisd for it
# (SystemWorker, through atrium_assets::image); portcullisd starts this
# bundle in a one-shot jail of its own, as _imagedec (never as the app),
# with the app's call on its stdin. It holds no capability: the jail has
# no network and no files but the bundle, and the decoder enters capability
# mode before it reads a call. ExecInstance can't start it as anyone else:
# the worker rule has no callers.
set -e
BIN=$1 KEY=$2 OPIFEX=${3:-./opifex}
[ -x "$BIN" ] && [ -f "$KEY" ] || { echo "usage: $0 <atrium-image-decode> <key.pem> [opifex]"; exit 2; }
B=/root/image-decode-bundle
rm -rf $B && mkdir -p $B/bin
cp "$BIN" $B/bin/atrium-image-decode
cat > $B/atrium.toml <<'T'
[app]
id = "org.atrium.image-decode"
name = "Image decoder"
version = "1"
entry = "bin/atrium-image-decode"

[resources]
memory = "256M"
cpu = 100
T
openssl dgst -sha256 -sign "$KEY" -out $B/atrium.toml.sig $B/atrium.toml
"$OPIFEX" install $B

# Its own uid, above the jaild policy's user range (65000) and below nobody.
pw groupshow _imagedec >/dev/null 2>&1 || pw groupadd _imagedec -g 65056
id _imagedec >/dev/null 2>&1 || pw useradd _imagedec -u 65056 -g _imagedec -d /nonexistent -s /usr/sbin/nologin -c "Image decoder"
test "$(id -u _imagedec):$(id -g _imagedec)" = 65056:65056
P=/etc/atrium/policy.toml
if ! grep -q 'workers."org.atrium.image-decode"' $P 2>/dev/null; then
    # No callers: no other user or app is delegated to start it as
    # _imagedec. SystemWorker uses only run_as and the concurrency cap; a
    # person's own CLI can still start the bundle, as that person.
    cat >> $P <<'T'

[workers."org.atrium.image-decode"]
callers    = []
run_as     = "_imagedec"
concurrent = 64
T
    chown root:wheel $P && chmod 644 $P
fi
echo "image-decode-bundle: installed $(du -sh $B | cut -f1)"
