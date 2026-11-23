#!/bin/sh
# On the FreeBSD machine, as root, after installing the current Portcullis
# and jaild policy. The P-256 signing key must already be trusted by Opifex.
set -eu
if [ "$#" != 4 ]; then
    echo "usage: $0 atrium-calendar fastid atrium.toml signing-key.pem" >&2
    exit 2
fi
test "$(id -u)" = 0
stage=$(mktemp -d)
trap 'rm -rf "$stage"' EXIT
openssl pkey -in "$4" -pubout -outform DER -out "$stage/publisher.der"
sha256 -q "$stage/publisher.der" >"$stage/publisher"
pin=/etc/atrium/calendar-publisher
if [ -e "$pin" ] && ! cmp -s "$pin" "$stage/publisher"; then
    echo "Calendar publisher differs from the installed grant" >&2
    exit 1
fi
pw groupshow _fasti >/dev/null 2>&1 || pw groupadd _fasti -g 65055
id _fasti >/dev/null 2>&1 || pw useradd _fasti -u 65055 -g _fasti -d /nonexistent -s /usr/sbin/nologin
test "$(id -u _fasti):$(id -g _fasti)" = 65055:65055
app="$stage/org.atrium.calendar"
mkdir -p "$app/bin"
install -m 0555 "$1" "$app/bin/atrium-calendar"
install -m 0555 "$(dirname "$1")/calendar-ical" "$app/bin/calendar-ical"
install -m 0444 "$3" "$app/atrium.toml"
opifex pack "$app"
openssl dgst -sha256 -sign "$4" -out "$app/atrium.toml.sig" "$app/atrium.toml"
opifex install "$app"
install -d -o root -g wheel -m 0755 /usr/local/libexec/atrium/fasti
install -o root -g wheel -m 0555 "$2" /usr/local/libexec/atrium/fasti/atrium-fastid
install -o root -g wheel -m 0444 "$stage/publisher" "$pin"
