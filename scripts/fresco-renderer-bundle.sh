#!/bin/sh
# Build, sign and install the Fresco window renderer's Portcullis bundle
# (org.atrium.fresco.renderer, docs/spec/fresco-f4-split.md F4b). Runs IN the
# FreeBSD guest:
#
#   sh fresco-renderer-bundle.sh <fresco-renderer binary> <fonts dir> <signing key.pem> [opifex]
#
# The bundle root is the jail's root, so the fonts land where the renderer's
# server-side text looks (/usr/local/share/fonts/...). The jail gets no
# capabilities: no network, no GPU or HID node, nothing of the host's files.
# Its memory is capped: a renderer touches its windows' frame slots (two
# 24 MiB slots a window, created by frescod) through its own mappings, so its
# RSS, and with it the cap, covers them (F3).
set -e
BIN=$1 FONTS=$2 KEY=$3 OPIFEX=${4:-./opifex}
[ -x "$BIN" ] && [ -d "$FONTS" ] && [ -f "$KEY" ] || { echo "usage: $0 <fresco-renderer> <fonts dir> <key.pem> [opifex]"; exit 2; }
B=/root/fr-bundle
rm -rf $B && mkdir -p $B/bin $B/usr/local/share/fonts
cp "$BIN" $B/bin/fresco-renderer
cp -R "$FONTS"/* $B/usr/local/share/fonts/
cat > $B/atrium.toml <<'T'
[app]
id = "org.atrium.fresco.renderer"
name = "Fresco window renderer"
version = "1"
entry = "bin/fresco-renderer"

[resources]
memory = "512M"
cpu = 100

[resources.pool]
weight = 1.0
floor = "64M"
T
openssl dgst -sha256 -sign "$KEY" -out $B/atrium.toml.sig $B/atrium.toml
"$OPIFEX" install $B

# The renderer runs as a uid of its own, never as frescod: frescod (as
# _frescod) asks portcullisd to start it, and the system policy's worker
# rule says it runs as _fresco-render. Both uids sit above the jaild policy's
# user range (65000) and below nobody, clear of base and ports.
id _frescod >/dev/null 2>&1 || pw useradd _frescod -u 65010 -d /nonexistent -s /usr/sbin/nologin -c "Fresco compositor"
id _fresco-render >/dev/null 2>&1 || pw useradd _fresco-render -u 65011 -d /nonexistent -s /usr/sbin/nologin -c "Fresco window renderer"
P=/etc/atrium/policy.toml
if ! grep -q 'workers."org.atrium.fresco.renderer"' $P 2>/dev/null; then
    cat >> $P <<'T'

[workers."org.atrium.fresco.renderer"]
callers    = ["_frescod"]
run_as     = "_fresco-render"
concurrent = 255
T
    chown root:wheel $P && chmod 644 $P
fi
echo "fresco-renderer-bundle: installed $(du -sh $B | cut -f1)"
