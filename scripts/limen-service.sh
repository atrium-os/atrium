#!/bin/sh
# Commission limend, the embed broker (docs/spec/limen.md), as a jailed,
# non-root service. Runs IN the FreeBSD guest, as root, once per host:
#
#   sh limen-service.sh <limend binary> [services.d manifest]
#
# Creates the _limen user (65022), its socket directory, installs
# atrium-limend with its services.d manifest, and prepares the jail root.
# atrium-portcullisd-bootstrap binds /atrium/sockets/limen/limend.sock and
# starts it; app jails that embed a role mount that directory.
set -e
D=$1
[ -x "$D" ] || { echo "usage: $0 <limend binary> [manifest]"; exit 2; }
id _limen >/dev/null 2>&1 || pw useradd _limen -u 65022 -d /nonexistent -s /usr/sbin/nologin -c "Limen embed broker"
install -d -m 0755 /atrium/sockets /atrium/sockets/portcullis /atrium/sockets/fresco-activation /atrium/sockets/audio-attribution
install -d -o _limen -g _limen -m 0755 /atrium/sockets/limen
install -d -m 0755 /var/lib/atrium/apps /var/run/atrium /etc/atrium
[ -e /var/run/atrium/app-registry ] || : > /var/run/atrium/app-registry
install -m 0755 "$D" /usr/local/bin/atrium-limend
HERE=$(dirname "$0")/..
install -d /etc/atrium/services.d
M=${2:-$HERE/limen/etc/services.d/41-atrium-limend.toml}
install -m 0644 "$M" /etc/atrium/services.d/41-atrium-limend.toml
R=/var/lib/atrium/jails/limend
for d in dev libexec lib usr/lib usr/local/bin etc/atrium var/lib/atrium/apps var/run/atrium \
         atrium/sockets/limen atrium/sockets/portcullis atrium/sockets/fresco-activation atrium/sockets/audio-attribution; do
    [ -d "$R/$d" ] || install -d "$R/$d"
done
for f in var/run/atrium/app-registry etc/pwd.db; do [ -e "$R/$f" ] || : > "$R/$f"; done
# A limend started by hand (tests) would hold the socket; the supervisor binds it.
pkill -x limend 2>/dev/null || true
echo "limen-service: commissioned. Restart atrium-portcullisd-bootstrap (or run it) to start atrium-limend."
