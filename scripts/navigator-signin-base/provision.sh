#!/bin/sh
# In the guest, as root, once: turn a fresh overlay of
# ~/atrium-vm/navigator-i0-tessera/root.raw into the sign-in gate base
# (vpn.md "Installed Sign In gate fixture"). Run by
# scripts/navigator-signin-base.sh, which stages $1 with:
#   arcad arca-probe arca-courierd portcullis   main's arm64 builds
#   navigator-fetchd                            navigatord needs a fetcher to start
#   mail-page.toml                              navigator-ladybird/mail-page/atrium.toml
#   fonts/                                      the repo's IBM Plex and Phosphor faces
# What the gate's services.sh expects under /tmp/mail-button (M): arcad,
# arca-probe, arca-courierd, portcullis, gate-key.pem (a publisher the
# guest trusts), the mail-page bundle template page/, nav.conf; the users
# mailgate (2002) and gatevest; the signed renderer bundle the base already
# has under /tmp/i0display.*.
set -eu
D=$1
M=/tmp/mail-button
test "$(mount -p | awk '$2 == "/" {print $3}')" = tessera
test ! -e "$M"
ls -d /tmp/i0display.*/renderer >/dev/null

id mailgate >/dev/null 2>&1 || pw useradd mailgate -u 2002 -m -s /bin/sh -c 'sign-in gate person'
id gatevest >/dev/null 2>&1 || pw useradd gatevest -u 2003 -d /nonexistent -s /usr/sbin/nologin -c 'sign-in gate vestibulum'
pw groupmod audio -m _choragus

# frescod's system-sans, and the icon face.
install -d /usr/local/share/fonts/ibm-plex /usr/local/share/fonts/phosphor
install -m 0444 "$D"/fonts/IBMPlex*.ttf /usr/local/share/fonts/ibm-plex/
install -m 0444 "$D"/fonts/Phosphor.ttf /usr/local/share/fonts/phosphor/

# Socket directories that current manifests mount (Navigator's audio,
# Sigillum's, the workers socket), and their jaild mounts.
for d in audio workers audio-attribution aura aura-keys clipboard opifex-sheet sigillum sigillum-dev sigillum-ui ssh-agent lyrad notify arca arca-courier limen monstrator opens opifex portal network-portal fresco fresco-activation fresco-chrome portcullis; do
    install -d -m 0755 /atrium/sockets/$d
done
if ! grep -q '"/atrium/sockets/workers"' /etc/atrium/jaild.policy.toml; then
    sed -i '' 's|^    "/atrium/sockets/audio-attribution",|    "/atrium/sockets/audio-attribution", "/atrium/sockets/audio", "/atrium/sockets/workers",|' /etc/atrium/jaild.policy.toml
fi
grep -q '"/atrium/sockets/workers"' /etc/atrium/jaild.policy.toml

install -d -m 0755 "$M" "$M/page/bin"
for b in arcad arca-probe arca-courierd portcullis; do install -m 0755 "$D/$b" "$M/$b"; done

# The gate's publisher: a fresh key whose public half the guest trusts.
umask 077
openssl ecparam -name prime256v1 -genkey -noout -out "$M/gate-key.pem"
umask 022
openssl ec -in "$M/gate-key.pem" -pubout -out /etc/atrium/publishers/signin-gate.pem 2>/dev/null
chmod 0644 /etc/atrium/publishers/signin-gate.pem

# The mail-page bundle template: the gate installs its worker as bin/mail-page.
install -m 0644 "$D/mail-page.toml" "$M/page/atrium.toml"
printf '#!/bin/sh\nexit 1\n' > "$M/page/bin/mail-page"
chmod 0755 "$M/page/bin/mail-page"
openssl dgst -sha256 -sign "$M/gate-key.pem" -out "$M/page/atrium.toml.sig" "$M/page/atrium.toml"

# Navigator's fetcher (navigatord refuses to start without one; a VPN
# window's pages still go only through Arca's couriers).
F=/root/signin-fetch-bundle
rm -rf $F && install -d $F/bin $F/etc/ssl
install -m 0755 "$D/navigator-fetchd" $F/bin/navigator-fetchd
cp /etc/ssl/cert.pem $F/etc/ssl/cert.pem
cp /etc/resolv.conf /etc/hosts /etc/nsswitch.conf $F/etc/ 2>/dev/null || true
printf '[app]\nid = "org.atrium.navigator.fetcher"\nname = "Navigator fetcher"\nversion = "1"\nentry = "bin/navigator-fetchd"\n\n[capabilities]\nnetwork = "full"\n\n[resources]\nmemory = "256M"\ncpu = 100\n' > $F/atrium.toml
openssl dgst -sha256 -sign "$M/gate-key.pem" -out $F/atrium.toml.sig $F/atrium.toml
opifex install $F
portcullis policy grant --system org.atrium.navigator.fetcher

# limend starts the sign-in page worker per instance (the window's uid
# space), never as itself (navigator-islands.md, "mail-page worker").
if ! grep -q '^\[workers."org.atrium.navigator.mail-page"\]' /etc/atrium/policy.toml; then
    printf '\n[workers."org.atrium.navigator.mail-page"]\ncallers = ["_limen"]\ncaller_apps = []\nrun_as = "_navpage"\nper_instance = true\nconcurrent = 64\n' >> /etc/atrium/policy.toml
fi

# navigatord's settings in its bundle (services.sh adds its UI lines).
S=/atrium/sockets
printf '%s\n' \
    "NAVIGATORD_SOCKET=$S/navigator/{instance}.sock" \
    "NAVIGATORD_ACTIVATION=$S/fresco-activation/fresco-activation.sock" \
    "NAVIGATORD_LIMEN=$S/limen/limend.sock" \
    "PORTCULLIS_SOCKET=$S/portcullis/portcullis.sock" \
    "NAVIGATORD_CONVERTER=jail:org.atrium.navigator.converter" \
    "NAVIGATORD_FETCHD=jail:org.atrium.navigator.fetcher" \
    "NAVIGATORD_ARCA=$S/arca/arcad.sock" \
    "NAVIGATORD_COURIERD=$S/arca-courier/courierd.sock" \
    "NAVIGATORD_STORE=/var/db/navigatord/store" \
    "NAVIGATORD_TRAILS=/var/db/navigatord/trails.json" > "$M/nav.conf"
for d in navigator navigator-page; do install -d -m 1777 $S/$d; done
sync
echo "navigator-signin-base: provisioned $M"
