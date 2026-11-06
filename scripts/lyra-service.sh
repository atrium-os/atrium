#!/bin/sh
# Commission Lyra's audio engine as a jailed, non-root service
# (docs/spec/lyra-v1.md §3, lyra/etc/services.d/35-atrium-lyrad.toml).
# Runs IN the FreeBSD guest, as root, once per host:
#
#   sh lyra-service.sh <lyrad binary> <choragusd binary>
#
# Creates the _lyra and _choragus users, puts sound(4) in bit-perfect mode
# (the kernel neither mixes nor resamples; lyrad owns the mix), makes the
# socket directories, installs atrium-lyrad and atrium-choragusd with their
# services.d manifests, and prepares both jail roots. The atrium_audio devfs ruleset (24) comes with
# etc/atrium.devfs.rules (scripts/bootstrap-atrium.sh installs and loads it).
set -e
BIN=$1 CHOR=$2
[ -x "$BIN" ] && [ -x "$CHOR" ] || { echo "usage: $0 <lyrad binary> <choragusd binary>"; exit 2; }

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
# choragusd's front doors, one per audio capability (Portcullis mounts each
# only into jails that hold it).
for d in audio audio-capture audio-monitor; do
    install -d -o _choragus -g _choragus -m 0755 /atrium/sockets/$d
done

install -m 0755 "$BIN" /usr/local/bin/atrium-lyrad
install -m 0755 "$CHOR" /usr/local/bin/atrium-choragusd
HERE=$(dirname "$0")/..
install -d /etc/atrium/services.d
install -m 0644 "$HERE/lyra/etc/services.d/35-atrium-lyrad.toml" /etc/atrium/services.d/
install -m 0644 "$HERE/choragus/etc/services.d/36-atrium-choragusd.toml" /etc/atrium/services.d/

R=/var/lib/atrium/jails/lyrad
for d in dev libexec lib usr/lib usr/local/bin atrium/sockets; do install -d "$R/$d"; done
R=/var/lib/atrium/jails/choragusd
for d in dev libexec lib usr/lib usr/local/bin atrium/sockets var/db/atrium var/run/atrium; do install -d "$R/$d"; done
[ -e "$R/var/run/atrium/app-registry" ] || : > "$R/var/run/atrium/app-registry"
# The registry itself must exist for the file mount; portcullisd fills it.
install -d /var/run/atrium /var/db/atrium
[ -e /var/run/atrium/app-registry ] || : > /var/run/atrium/app-registry
echo "lyra-service: commissioned. Restart atrium-portcullisd-bootstrap (or run it) to start atrium-lyrad and atrium-choragusd."
