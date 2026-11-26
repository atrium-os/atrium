#!/bin/sh
# Commission Vinculum, Atrium's Bluetooth stack (docs/spec/bluetooth.md §4,
# §5), as jailed, non-root services. Runs IN the FreeBSD guest, as root, once
# per host:
#
#   sh vinculum-service.sh <vinculumd binary> <vinculum-hci binary> [<firmware dir>]
#
# vinculumd must be built with `--features atrium` (its Arca key store).
# Creates the _vinculum and _vinculum-hci users and the socket directories,
# installs atrium-vinculumd (with its services.d manifest) and
# atrium-vinculum-hci, installs Vinculum's device rule for dispensatord
# (vinculum/etc/devices.d/25-vinculum.toml) and prepares vinculumd's jail
# root. The firmware dir, if given, holds Realtek's rtl8761bu_fw.bin and
# rtl8761bu_config.bin with their licence (vinculum/firmware/README.md); it
# is copied unmodified. The driver jails are started by dispensatord, one per
# controller; the atrium_bluetooth devfs ruleset (25) comes with
# etc/atrium.devfs.rules (scripts/bootstrap-atrium.sh installs and loads it).
set -e
D=$1
H=$2
FW=$3
[ -x "$D" ] && [ -x "$H" ] || { echo "usage: $0 <vinculumd binary> <vinculum-hci binary> [<firmware dir>]"; exit 2; }

user() { id "$1" >/dev/null 2>&1 || pw useradd "$1" -u "$2" -d /nonexistent -s /usr/sbin/nologin -c "$3"; }
user _vinculum     65024 "Vinculum Bluetooth host"
user _vinculum-hci 65025 "Vinculum Bluetooth controller transport"
# See sigillum-service.sh: nullfs at boot, not on the first driver jail's mount.
kldload -n nullfs
sysrc -n kld_list 2>/dev/null | grep -qw nullfs || sysrc kld_list+=nullfs >/dev/null
# The base netgraph driver (ng_ubt) must not take controllers: devmatch
# reloads it on every attach (a plain kldunload does not last), and it starts
# SCO isochronous transfers on interface 1, the path that panicked a Mac host
# through qemu usb-host. dispensatord still detaches it if it is loaded.
sysrc -n devmatch_blocklist 2>/dev/null | grep -qw ng_ubt || sysrc devmatch_blocklist+=ng_ubt >/dev/null
kldunload -n ng_ubt 2>/dev/null || true

install -d -m 0755 /atrium/sockets
# The transport: only _vinculum-hci may reach it at all; vinculumd also checks the uid.
install -d -o _vinculum -g _vinculum-hci -m 0750 /atrium/sockets/vinculum-hci
# The trusted UI's socket: apps holding bluetooth-control, checked per connection.
install -d -o _vinculum -g _vinculum -m 0755 /atrium/sockets/bluetooth-control
# HID devices for tactusd (65018); its group if Tactus is commissioned, else none but _vinculum.
if pw groupshow _tactus >/dev/null 2>&1; then
    install -d -o _vinculum -g _tactus -m 0750 /atrium/sockets/bluetooth-hid
else
    install -d -o _vinculum -g _vinculum -m 0700 /atrium/sockets/bluetooth-hid
    echo "vinculum-service: no _tactus group yet; run tactus-service.sh, then this again, for Bluetooth keyboards and mice"
fi
install -d -o _vinculum -g _vinculum -m 0700 /var/db/vinculum

install -m 0755 "$D" /usr/local/bin/atrium-vinculumd
install -m 0755 "$H" /usr/local/bin/atrium-vinculum-hci
F=/usr/local/share/atrium/firmware/bluetooth
install -d -m 0755 /usr/local/share/atrium/firmware "$F"
if [ -n "$FW" ]; then
    for f in rtl8761bu_fw.bin rtl8761bu_config.bin LICENCE.rtlwifi_firmware.txt; do
        [ -f "$FW/$f" ] && install -m 0444 "$FW/$f" "$F/"
    done
fi

# arcad must know vinculumd by its uid, or the key store is refused.
A=/etc/atrium/services.d/42-atrium-arcad.toml
if [ -f "$A" ] && ! grep -q 'vinculumd=65024' "$A"; then
    echo "vinculum-service: $A does not name vinculumd=65024 in ARCA_SERVICES (a test copy?); install arca/arcad/etc/services.d/42-atrium-arcad.toml and restart arcad"
fi
HERE=$(dirname "$0")/..
install -d /etc/atrium/services.d
install -m 0644 "$HERE/vinculum/etc/services.d/46-atrium-vinculumd.toml" /etc/atrium/services.d/
install -d -m 0755 /usr/local/etc/atrium/devices.d
install -o root -g wheel -m 0644 "$HERE/vinculum/etc/devices.d/25-vinculum.toml" /usr/local/etc/atrium/devices.d/

R=/var/lib/atrium/jails/vinculumd
# (Skips dirs that exist: a running jail has its mounts over them.)
for d in dev libexec lib usr/lib usr/local/bin var/lib/atrium/apps var/db/vinculum var/run/atrium \
         atrium/sockets/arca atrium/sockets/vinculum-hci atrium/sockets/bluetooth-control atrium/sockets/bluetooth-hid; do
    [ -d "$R/$d" ] || install -d "$R/$d"
done
[ -e "$R/var/run/atrium/app-registry" ] || touch "$R/var/run/atrium/app-registry"
echo "vinculum-service: commissioned. Restart atrium-portcullisd-bootstrap (or run it) to start atrium-vinculumd; plug in a controller for dispensatord to start its driver."
