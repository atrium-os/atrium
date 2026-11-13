#!/bin/sh
# Commission Capsa, Atrium's removable-storage FUSE server
# (docs/spec/device-assignment.md §4.6, §6.4). Runs IN the FreeBSD guest, as
# root, once per host:
#
#   sh capsa-service.sh <atrium-capsa-fuse binary>
#
# Creates the _capsa user, loads fusefs (now and at boot), installs
# atrium-capsa-fuse and Capsa's device rule for dispensatord
# (capsa/etc/devices.d/50-capsa.toml), and makes /media. dispensatord then
# serves each USB-disk partition in a jail of its own while a human session
# is active and unlocked. The atrium_capsa devfs ruleset (31) comes with
# etc/atrium.devfs.rules, and jaild's pin 31 with etc/jaild.policy.toml.
set -e
F=$1
[ -x "$F" ] || { echo "usage: $0 <atrium-capsa-fuse binary>"; exit 2; }
HERE=$(dirname "$0")/..

id _capsa >/dev/null 2>&1 || pw useradd _capsa -u 65031 -d /nonexistent -s /usr/sbin/nologin -c "Capsa removable storage"
for m in fusefs nullfs; do
  kldload -n $m
  sysrc -n kld_list 2>/dev/null | grep -qw $m || sysrc kld_list+=$m >/dev/null
done
install -m 0755 "$F" /usr/local/bin/atrium-capsa-fuse
install -d -m 0755 /media /usr/local/etc/atrium/devices.d
install -o root -g wheel -m 0644 "$HERE/capsa/etc/devices.d/50-capsa.toml" /usr/local/etc/atrium/devices.d/
service atrium-dispensatord restart >/dev/null 2>&1 || true
echo "capsa: installed; USB-disk partitions are served under /media for the logged-in user"
