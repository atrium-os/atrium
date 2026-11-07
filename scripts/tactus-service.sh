#!/bin/sh
# Commission Tactus's input service as a jailed, non-root service
# (docs/spec/atrium-input.md §2, tactus/etc/services.d/38-atrium-tactusd.toml).
# Runs IN the FreeBSD guest, as root, once per host:
#
#   sh tactus-service.sh <tactusd binary>
#
# Creates the _tactus user, makes tactusd's socket directory (readable by
# _frescod only), installs atrium-tactusd with its services.d manifest and
# prepares the jail root. The atrium_input devfs ruleset (26) comes with
# etc/atrium.devfs.rules (scripts/bootstrap-atrium.sh installs and loads it);
# hidraw(4) is loaded now and added to kld_list.
set -e
BIN=$1
[ -x "$BIN" ] || { echo "usage: $0 <tactusd binary>"; exit 2; }

id _tactus >/dev/null 2>&1 || pw useradd _tactus -u 65018 -d /nonexistent -s /usr/sbin/nologin -c "Tactus input service"
kldload -n hidraw
sysrc -n kld_list 2>/dev/null | grep -qw hidraw || sysrc kld_list+=hidraw >/dev/null

install -d -m 0755 /atrium/sockets
# tactusd binds here; only _frescod (group) may reach it.
GRP=_frescod; id _frescod >/dev/null 2>&1 || GRP=wheel
install -d -o _tactus -g $GRP -m 0750 /atrium/sockets/tactus

install -m 0755 "$BIN" /usr/local/bin/atrium-tactusd
HERE=$(dirname "$0")/..
install -d /etc/atrium/services.d
install -m 0644 "$HERE/tactus/etc/services.d/38-atrium-tactusd.toml" /etc/atrium/services.d/

R=/var/lib/atrium/jails/tactusd
# (Skips dirs that exist: a running jail has its mounts over them.)
for d in dev libexec lib usr/lib usr/local/bin atrium/sockets; do [ -d "$R/$d" ] || install -d "$R/$d"; done
echo "tactus-service: commissioned. Restart atrium-portcullisd-bootstrap (or run it) to start atrium-tactusd."
