#!/bin/sh
# Commission Tactus's input service as a jailed, non-root service
# (docs/spec/atrium-input.md §2, tactus/etc/services.d/38-atrium-tactusd.toml).
# Runs IN the FreeBSD guest, as root, once per host:
#
#   sh tactus-service.sh <tactusd binary> [<tactus-console binary>]
#
# Creates the _tactus user, makes tactusd's socket directory (readable by
# _frescod only), installs atrium-tactusd with its services.d manifest and
# prepares the jail root. The atrium_input devfs ruleset (26) comes with
# etc/atrium.devfs.rules (scripts/bootstrap-atrium.sh installs and loads it);
# hidraw(4) is loaded now and added to kld_list.
#
# With tactus-console it is installed and enabled too: it releases the PS/2
# keyboard (atkbd0) from the console's kbdmux while tactusd runs and gives it
# back when tactusd stops (atrium-input.md §3.2).
set -e
BIN=$1
CON=${2:-}
[ -x "$BIN" ] || { echo "usage: $0 <tactusd binary> [<tactus-console binary>]"; exit 2; }

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
# Which hidraw nodes tactusd is shown: dispensatord's rule (device-assignment.md).
install -d -m 0755 /usr/local/etc/atrium/devices.d
install -o root -g wheel -m 0644 "$HERE/tactus/etc/devices.d/30-tactus.toml" /usr/local/etc/atrium/devices.d/

if [ -n "$CON" ]; then
  install -m 0755 "$CON" /usr/local/sbin/atrium-tactus-console
  install -m 0555 "$HERE/tactus/etc/rc.d/atrium_tactus_console" /usr/local/etc/rc.d/
  sysrc atrium_tactus_console_enable=YES >/dev/null
  service atrium_tactus_console restart >/dev/null 2>&1 || service atrium_tactus_console start
fi

R=/var/lib/atrium/jails/tactusd
# (Skips dirs that exist: a running jail has its mounts over them.)
for d in dev libexec lib usr/lib usr/local/bin atrium/sockets; do [ -d "$R/$d" ] || install -d "$R/$d"; done
echo "tactus-service: commissioned. Restart atrium-portcullisd-bootstrap (or run it) to start atrium-tactusd."
