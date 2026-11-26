#!/bin/sh
# Commission Fenestra's host side as a boot service (docs/spec/fenestra.md
# §19b). Runs IN the FreeBSD guest, as root, once per host:
#
#   sh fenestra-service.sh <fenestrad> <fenestra-shell> <fenestra-session> IFACE SUBNET PUBLIC_IP
#
# Creates the _fenestra user (65057), makes the directory frescod serves
# fenestrad's link in (owned _frescod, group _fenestra, 0750: only the two
# reach it), installs the binaries and the atrium_fenestrad rc.d script and
# enables it for the network viewers come from. frescod's services.d manifest
# already names the link; restart frescod (atrium-portcullisd-bootstrap) so
# it serves it. jaild with this tree's policy ([fenestra_sessions]) must be
# installed (scripts/deploy-portcullis.sh).
set -e
[ $# -eq 6 ] || { echo "usage: $0 <fenestrad> <fenestra-shell> <fenestra-session> IFACE SUBNET PUBLIC_IP"; exit 2; }
D=$1; SH=$2; SESS=$3; IF=$4; NET=$5; IP=$6
for b in "$D" "$SH" "$SESS"; do [ -x "$b" ] || { echo "fenestra-service: $b is not an executable"; exit 2; }; done
case "$IF" in *[!A-Za-z0-9._]*|'') echo "fenestra-service: bad interface $IF"; exit 2;; esac
case "$NET" in *[!0-9./]*|'') echo "fenestra-service: bad subnet $NET"; exit 2;; esac
case "$IP" in *[!0-9.]*|'') echo "fenestra-service: bad address $IP"; exit 2;; esac

id _fenestra >/dev/null 2>&1 || pw useradd _fenestra -u 65057 -d /nonexistent -s /usr/sbin/nologin -c "Fenestra session daemon"
[ "$(id -u _fenestra)" = 65057 ] || { echo "fenestra-service: _fenestra is not uid 65057"; exit 1; }
id _frescod >/dev/null 2>&1 || { echo "fenestra-service: no _frescod user (scripts/fresco-renderer-bundle.sh)"; exit 1; }

install -d -m 0755 /atrium/sockets
install -d -o _frescod -g _fenestra -m 0750 /atrium/sockets/fenestra
# In /usr/local/bin: the unprivileged side's jail mounts it read-only.
install -m 0755 "$D" /usr/local/bin/atrium-fenestrad
rm -f /usr/local/sbin/atrium-fenestrad
install -m 0755 "$SH" /usr/local/bin/fenestra-shell
install -m 0755 "$SESS" /usr/local/bin/atrium-fenestra-session
HERE=$(dirname "$0")/..
install -m 0555 "$HERE/fenestrad/etc/rc.d/atrium_fenestrad" /usr/local/etc/rc.d/
sysrc atrium_fenestrad_enable=YES atrium_fenestrad_lan="$IF $NET" atrium_fenestrad_public_ip="$IP" >/dev/null
echo "fenestra-service: commissioned. Restart frescod so it serves the link; atrium_fenestrad starts at boot (service atrium_fenestrad start now)."
