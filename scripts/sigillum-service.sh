#!/bin/sh
# Commission Sigillum, Atrium's identity verification service
# (docs/spec/sigillum.md §4), as jailed, non-root services. Runs IN the FreeBSD
# guest, as root, once per host:
#
#   sh sigillum-service.sh <sigillumd binary> <sigillum-fido binary>
#
# Creates the _sigillum, _sigillum-dev and _sigillum-ui users, the three socket
# directories, installs atrium-sigillumd (with its services.d manifest) and
# atrium-sigillum-fido, and prepares the jail roots. The driver jails
# themselves are started by portcullisd's identity-device agent, one per
# security key; the atrium_identity devfs ruleset (27) comes with
# etc/atrium.devfs.rules (scripts/bootstrap-atrium.sh installs and loads it).
set -e
D=$1
F=$2
[ -x "$D" ] && [ -x "$F" ] || { echo "usage: $0 <sigillumd binary> <sigillum-fido binary>"; exit 2; }

user() { id "$1" >/dev/null 2>&1 || pw useradd "$1" -u "$2" -d /nonexistent -s /usr/sbin/nologin -c "$3"; }
user _sigillum     65028 "Sigillum broker"
user _sigillum-dev 65029 "Sigillum device drivers"
user _sigillum-ui  65030 "Sigillum trusted UI"
kldload -n hidraw
sysrc -n kld_list 2>/dev/null | grep -qw hidraw || sysrc kld_list+=hidraw >/dev/null

install -d -m 0755 /atrium/sockets
# Clients: anyone may reach it; sigillumd labels each by peer uid.
install -d -o _sigillum -g _sigillum -m 0755 /atrium/sockets/sigillum
# Drivers and the UI: only their group may reach the socket at all.
install -d -o _sigillum -g _sigillum-dev -m 0750 /atrium/sockets/sigillum-dev
install -d -o _sigillum -g _sigillum-ui -m 0750 /atrium/sockets/sigillum-ui

install -m 0755 "$D" /usr/local/bin/atrium-sigillumd
install -m 0755 "$F" /usr/local/bin/atrium-sigillum-fido
HERE=$(dirname "$0")/..
install -d /etc/atrium/services.d
install -m 0644 "$HERE/sigillum/etc/services.d/37-atrium-sigillumd.toml" /etc/atrium/services.d/

R=/var/lib/atrium/jails/sigillumd
# (Skips dirs that exist: a running jail has its mounts over them.)
for d in dev libexec lib usr/lib usr/local/bin atrium/sockets var/run/atrium; do [ -d "$R/$d" ] || install -d "$R/$d"; done
[ -e "$R/var/run/atrium/app-registry" ] || touch "$R/var/run/atrium/app-registry"
echo "sigillum-service: commissioned. Restart atrium-portcullisd-bootstrap (or run it) to start atrium-sigillumd."
