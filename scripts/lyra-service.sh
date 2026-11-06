#!/bin/sh
# Commission Lyra's audio engine as a jailed, non-root service
# (docs/spec/lyra-v1.md §3, lyra/etc/services.d/35-atrium-lyrad.toml).
# Runs IN the FreeBSD guest, as root, once per host:
#
#   sh lyra-service.sh <lyrad binary>
#
# Creates the _lyra and _choragus users, puts sound(4) in bit-perfect mode
# (the kernel neither mixes nor resamples; lyrad owns the mix), makes lyrad's
# socket directory, installs atrium-lyrad and its services.d manifest, and
# prepares the jail root. The atrium_audio devfs ruleset (24) comes with
# etc/atrium.devfs.rules (scripts/bootstrap-atrium.sh installs and loads it).
set -e
BIN=$1
[ -x "$BIN" ] || { echo "usage: $0 <lyrad binary>"; exit 2; }

id _lyra >/dev/null 2>&1 || pw useradd _lyra -u 65016 -d /nonexistent -s /usr/sbin/nologin -c "Lyra audio engine"
id _choragus >/dev/null 2>&1 || pw useradd _choragus -u 65017 -d /nonexistent -s /usr/sbin/nologin -c "Choragus audio policy"

# Bit-perfect sound(4): no in-kernel vchan mixing or feeder resampling.
sysctl hw.snd.maxautovchans=0 >/dev/null
for u in $(sysctl -N dev.pcm 2>/dev/null | sed -n 's/^dev\.pcm\.\([0-9]*\)\.bitperfect$/\1/p'); do
    sysctl dev.pcm.$u.bitperfect=1 >/dev/null
    grep -q "^dev.pcm.$u.bitperfect=" /etc/sysctl.conf 2>/dev/null || echo "dev.pcm.$u.bitperfect=1" >> /etc/sysctl.conf
done
grep -q '^hw.snd.maxautovchans=' /etc/sysctl.conf 2>/dev/null || echo 'hw.snd.maxautovchans=0' >> /etc/sysctl.conf

# lyrad binds here; only _choragus (group) may reach it.
install -d -m 0755 /atrium/sockets
install -d -o _lyra -g _choragus -m 0750 /atrium/sockets/lyrad

install -m 0755 "$BIN" /usr/local/bin/atrium-lyrad
HERE=$(dirname "$0")/..
install -d /etc/atrium/services.d
install -m 0644 "$HERE/lyra/etc/services.d/35-atrium-lyrad.toml" /etc/atrium/services.d/

R=/var/lib/atrium/jails/lyrad
for d in dev libexec lib usr/lib usr/local/bin atrium/sockets; do install -d "$R/$d"; done
echo "lyra-service: commissioned. Restart atrium-portcullisd-bootstrap (or run it) to start atrium-lyrad."
