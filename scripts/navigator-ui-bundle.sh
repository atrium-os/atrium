#!/bin/sh
# Build, sign and install the Navigator's UI, navigator-ui, as a Portcullis
# bundle (org.atrium.navigator.ui, docs/spec/navigator-ui.md §2.3a, J3). Runs
# IN the FreeBSD guest:
#
#   sh navigator-ui-bundle.sh <navigator-ui binary> <signing key.pem> [opifex]
#
# navigatord (the Navigator app, in its own jail) starts it with
# portcullisd's ExecInstance, as _navui (never as the Navigator's own uid):
# the system policy's worker rule names the Navigator by app id. It has no
# capabilities: its stdin is its channel to navigatord, which passes it its
# frescod connections and its limend connection, and keeps its trails. No
# network, no sockets, no files of the host's: its fonts are in the binary.
set -e
BIN=$1 KEY=$2 OPIFEX=${3:-./opifex}
[ -x "$BIN" ] && [ -f "$KEY" ] || { echo "usage: $0 <navigator-ui> <key.pem> [opifex]"; exit 2; }
APP=org.atrium.navigator.ui
B=/root/navigator-ui-bundle
rm -rf $B && mkdir -p $B/bin
cp "$BIN" $B/bin/navigator-ui
cat > $B/atrium.toml <<T
[app]
id = "$APP"
name = "Navigator window"
version = "1"
entry = "bin/navigator-ui"

[resources]
memory = "512M"
cpu = 100
T
openssl dgst -sha256 -sign "$KEY" -out $B/atrium.toml.sig $B/atrium.toml
"$OPIFEX" install $B

# Its own uid, clear of the other system workers (_navpage 65035,
# _atrium-netconf 65036).
pw groupshow _navui >/dev/null 2>&1 || pw groupadd _navui -g 65037
id _navui >/dev/null 2>&1 || pw useradd _navui -u 65037 -g _navui -d /nonexistent -s /usr/sbin/nologin -c "Navigator window"
P=/etc/atrium/policy.toml
if ! grep -q "workers.\"$APP\"" $P 2>/dev/null; then
    cat >> $P <<T

[workers."$APP"]
caller_apps = ["org.atrium.navigator"]
run_as      = "_navui"
concurrent  = 8
T
    chown root:wheel $P && chmod 644 $P
fi
echo "navigator-ui-bundle: installed $(du -sh $B | cut -f1)"
