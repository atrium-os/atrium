#!/bin/sh
# Commission the share sheet (docs/spec/share-sheet.md) as a jailed, non-root
# service. Runs IN the FreeBSD guest, as root, once per host:
#
#   sh share-sheet-service.sh <share-sheet binary> [services.d manifest]
#
# Creates the _share user (65045), installs atrium-share-sheet with its
# services.d manifest, and prepares the jail root. atrium-portcullisd-bootstrap
# starts it; limend takes `PROVIDE share` and CHOOSE from uid 65045
# (LIMEN_SHARE_UID) and frescod lets that uid spend SHARE presses.
set -e
D=$1
[ -x "$D" ] || { echo "usage: $0 <share-sheet binary>"; exit 2; }
id _share >/dev/null 2>&1 || pw useradd _share -u 65045 -d /nonexistent -s /usr/sbin/nologin -c "Atrium share sheet"
install -d -m 0755 /atrium/sockets /var/lib/atrium/apps /var/run/atrium
install -m 0755 "$D" /usr/local/bin/atrium-share-sheet
HERE=$(dirname "$0")/..
install -d /etc/atrium/services.d
M=${2:-$HERE/share-sheet/etc/services.d/42-atrium-share-sheet.toml}
install -m 0644 "$M" /etc/atrium/services.d/42-atrium-share-sheet.toml
R=/var/lib/atrium/jails/share-sheet
for d in dev libexec lib usr/lib usr/local/bin atrium/sockets/limen atrium/sockets/fresco atrium/sockets/fresco-activation var/lib/atrium/apps var/run/atrium; do
    [ -d "$R/$d" ] || install -d "$R/$d"
done
[ -e "$R/var/run/atrium/app-registry" ] || : > "$R/var/run/atrium/app-registry"
echo "share-sheet-service: commissioned. Restart atrium-portcullisd-bootstrap (or run it) to start atrium-share-sheet."
