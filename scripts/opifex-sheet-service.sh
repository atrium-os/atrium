#!/bin/sh
# Commission the install sheet, opifex-sheetd (docs/spec/opifex.md §7.1), as a
# jailed, non-root service. Runs IN the FreeBSD guest, as root, once per host:
#
#   sh opifex-sheet-service.sh <opifex-sheetd binary> [services.d manifest]
#
# Creates the _opifexsheet user (65042), its socket directory and opifexd's,
# installs atrium-opifex-sheetd with its services.d manifest, and prepares the
# jail root. atrium-portcullisd-bootstrap starts it; opifexd takes
# InstallApproved from uid 65042 (OPIFEX_SHEET_UID).
set -e
D=$1
[ -x "$D" ] || { echo "usage: $0 <opifex-sheetd binary>"; exit 2; }
id _opifexsheet >/dev/null 2>&1 || pw useradd _opifexsheet -u 65042 -d /nonexistent -s /usr/sbin/nologin -c "Opifex install sheet"
install -d -m 0755 /atrium/sockets /atrium/sockets/opifex
# Anyone whose jail mounts it may ask; the person decides on the sheet.
install -d -o _opifexsheet -g _opifexsheet -m 0755 /atrium/sockets/opifex-sheet
install -m 0755 "$D" /usr/local/bin/atrium-opifex-sheetd
HERE=$(dirname "$0")/..
install -d /etc/atrium/services.d
M=${2:-$HERE/opifex-sheet/etc/services.d/39-atrium-opifex-sheetd.toml}
install -m 0644 "$M" /etc/atrium/services.d/39-atrium-opifex-sheetd.toml
R=/var/lib/atrium/jails/opifex-sheetd
for d in dev libexec lib usr/lib usr/local/bin atrium/sockets/opifex-sheet atrium/sockets/opifex atrium/sockets/fresco atrium/sockets/fresco-activation; do [ -d "$R/$d" ] || install -d "$R/$d"; done
echo "opifex-sheet-service: commissioned. Restart atrium-portcullisd-bootstrap (or run it) to start atrium-opifex-sheetd."
