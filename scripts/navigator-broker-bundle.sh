#!/bin/sh
# Build, sign and install navigatord as the Navigator app, a Portcullis
# bundle (org.atrium.navigator, docs/spec/navigator-ui.md §2.3a, J2). Runs
# IN the FreeBSD guest:
#
#   sh navigator-broker-bundle.sh <navigatord binary> <navigatord.conf> <signing key.pem> [opifex]
#
# A person launches it (portcullisd's Launch), so it runs as the Navigator's
# per-app uid for that person, in the Navigator's jail. Its one capability is
# `navigator-broker`, restricted: the directories of the sockets it serves
# (navigator/, navigator-page/) and uses (fresco-chrome/, fresco-activation/,
# limen/, portcullis/, arca/, arca-courier/). No network: its pages come
# through fetcher jails. A jail gets no environment of its caller's, so its
# settings are etc/navigatord.conf in the bundle.
set -e
BIN=$1 CONF=$2 KEY=$3 OPIFEX=${4:-./opifex}
[ -x "$BIN" ] && [ -f "$CONF" ] && [ -f "$KEY" ] || { echo "usage: $0 <navigatord> <navigatord.conf> <key.pem> [opifex]"; exit 2; }
APP=org.atrium.navigator
B=/root/navigator-broker-bundle
rm -rf $B && mkdir -p $B/bin $B/etc
cp "$BIN" $B/bin/navigatord
cp "$CONF" $B/etc/navigatord.conf
cat > $B/atrium.toml <<T
[app]
id = "$APP"
name = "Navigator"
version = "1"
entry = "bin/navigatord"

[capabilities]
navigator-broker = true
# The captive portal's sign-in window (atrium-net.md §8.1 S3), restricted.
network-portal = true

# Web links other apps open come here (opening-links.md §4.1, D5).
[[links]]
schemes = ["http", "https"]

[resources]
memory = "1G"
cpu = 100
T
openssl dgst -sha256 -sign "$KEY" -out $B/atrium.toml.sig $B/atrium.toml
"$OPIFEX" install $B
# Restricted: granted in the system policy, never from Settings.
PATH=$(dirname "$OPIFEX"):$PATH portcullis policy grant --system $APP
echo "navigator-broker-bundle: installed $(du -sh $B | cut -f1)"
