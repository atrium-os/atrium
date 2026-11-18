#!/bin/sh
# Commission Monstrator, the link opener (docs/spec/opening-links.md §6), as a
# jailed, non-root service. Runs IN the FreeBSD guest, as root, once per host:
#
#   sh monstrator-service.sh <monstratord binary> [services.d manifest]
#
# Creates the _monstrator user (65038), its socket directory, installs
# atrium-monstratord with its services.d manifest, and prepares the jail
# root. atrium-portcullisd-bootstrap starts it.
set -e
D=$1
[ -x "$D" ] || { echo "usage: $0 <monstratord binary>"; exit 2; }
id _monstrator >/dev/null 2>&1 || pw useradd _monstrator -u 65038 -d /nonexistent -s /usr/sbin/nologin -c "Monstrator, the link opener"
install -d -m 0755 /atrium/sockets
# Anyone whose jail mounts it may ask; monstratord knows each by peer uid.
install -d -o _monstrator -g _monstrator -m 0755 /atrium/sockets/monstrator
# Tabula's (tabulad binds clipboard.sock there): Copy address.
install -d -m 0755 /atrium/sockets/clipboard
# The install sheet's (opifex-sheet-service.sh owns it): atrium-app: links.
[ -d /atrium/sockets/opifex-sheet ] || install -d -m 0755 /atrium/sockets/opifex-sheet
install -m 0755 "$D" /usr/local/bin/atrium-monstratord
HERE=$(dirname "$0")/..
install -d /etc/atrium/services.d
M=${2:-$HERE/monstrator/etc/services.d/38-atrium-monstratord.toml}
install -m 0644 "$M" /etc/atrium/services.d/38-atrium-monstratord.toml
R=/var/lib/atrium/jails/monstratord
for d in dev libexec lib usr/lib usr/local/bin atrium/sockets/monstrator atrium/sockets/portcullis atrium/sockets/fresco atrium/sockets/fresco-activation atrium/sockets/clipboard atrium/sockets/opifex-sheet; do [ -d "$R/$d" ] || install -d "$R/$d"; done
echo "monstrator-service: commissioned. Restart atrium-portcullisd-bootstrap (or run it) to start atrium-monstratord."
