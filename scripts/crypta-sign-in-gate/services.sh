set -eu
D=/tmp/i3-vpn
cd "$D"
M=/tmp/mail-button
S=/atrium/sockets
key=$M/gate-key.pem
renderer=$1
test -x "$renderer"
! jls -j org_atrium_navigator__p_vpn jid >/dev/null 2>&1 || exit 1
printf '10.0.2.100 sso.crypta.test\n10.0.2.101 idp.crypta.test\n' >> /etc/hosts
mkdir -p /var/run/atrium
touch /var/run/atrium/app-registry
export ARCA_WEB_BROKER="org.atrium.navigator $(openssl pkey -in "$key" -pubout -outform DER | sha256 -q)"
for dir in fresco fresco-chrome fresco-activation limen arca arca-courier sigillum; do mkdir -p "$S/$dir"; done
for path in "$S/fresco/fresco.sock" "$S/fresco-chrome/fresco-chrome.sock" "$S/fresco-activation/fresco-activation.sock" "$S/arca/arcad.sock" "$S/arca-courier/courierd.sock" "$S/limen/limend.sock"; do [ ! -S "$path" ] || unlink "$path"; done
chown _limen "$S/limen"
env ARCA_SERVICES=cryptad=65052 ARCA_UI_UID=65021 ARCA_DIR="$M/arca-state" ARCA_VESTIBULUM_UID="$(id -u gatevest)" daemon -f -o "$D/arca.log" "$M/arcad"
for i in $(jot 25); do test -S "$S/arca/arcad.sock" && break; sleep .2; done
test -S "$S/arca/arcad.sock"
su -m gatevest -c "$M/arca-probe $S/arca/arcad.sock unlock mailgate local-fixture" | grep unlocked
env ARCA_COURIERD_CA="$D/ca.pem" ARCA_COURIERD_GRANT_PRIVATE_NETWORK=1 daemon -f -o "$D/courier.log" "$M/arca-courierd"
install -m755 "$D/mail-page" "$M/page/bin/mail-page"
opifex install "$M/page"
test "$(sha256 -q "$D/mail-page")" = "$(sha256 -q /var/lib/atrium/apps/org.atrium.navigator.mail-page/bin/mail-page)"
sh navigator-ui-bundle.sh "$D/navigator-ui" "$key" /usr/local/bin/opifex
sh navigator-page-bundle.sh "$D/navigator-page" "$key" /usr/local/bin/opifex
mkdir -p converter/bin
cp prerender converter/bin/
cat >converter/atrium.toml <<EOF
[app]
id="org.atrium.navigator.converter"
name="Navigator converter"
version="1"
entry="bin/prerender"
[resources]
memory="1G"
cpu=100
EOF
openssl dgst -sha256 -sign "$key" -out converter/atrium.toml.sig converter/atrium.toml
opifex install "$D/converter"
installed=/var/lib/atrium/apps/org.atrium.navigator.converter/bin/prerender
if ! cmp -s prerender "$installed"; then
    install -m755 prerender "$installed.gate"
    cmp -s prerender "$installed.gate"
    mv "$installed.gate" "$installed"
    echo "Restored installed converter from the staged fixture"
fi
cmp -s prerender "$installed"
cp "$M/nav.conf" "$D/nav.conf"
printf 'NAVIGATORD_UI=jail:org.atrium.navigator.ui\nFRESCOD_CHROME_SOCK=/atrium/sockets/fresco-chrome/fresco-chrome.sock\nNAVIGATOR_UI_TRACE=1\n' >> "$D/nav.conf"
sh navigator-broker-bundle.sh "$D/navigatord" "$D/nav.conf" "$key" /usr/local/bin/opifex
install -m644 "$D/nav.conf" /var/lib/atrium/apps/org.atrium.navigator/etc/navigatord.conf
A="$S/fresco-activation/fresco-activation.sock"
export FRESCOD_SOCK="$S/fresco/fresco.sock" FRESCOD_CHROME_SOCK="$S/fresco-chrome/fresco-chrome.sock" FRESCOD_ACTIVATION_SOCK="$A" FRESCOD_WM_SOCK="$D/wm.sock" FRESCOD_INPUT_SOCK="$D/input.sock" FRESCOD_HEADLESS_OUTPUTS=1280x840 FRESCOD_HEADLESS_PNG="$D/screen" FRESCOD_SPLIT=1 FRESCOD_SPLIT_PIXELS=1 FRESCOD_RENDERER=compute FRESCOD_ACTIVATION_UIDS=65021 FRESCOD_SLOT_UIDS=65022 FRESCOD_INJECT_ACTIVATION=1
export FRESCOD_RENDERER_LAUNCHER="/usr/local/bin/portcullis exec --user _fresco-render --require-memory-limit --instance vr{client} $renderer"
daemon -f -o "$D/fresco.log" "$D/frescod"
for i in $(jot 45); do test -S "$D/input.sock" && break; sleep .5; done
test -S "$D/input.sock"
chmod 666 "$A" "$FRESCOD_SOCK" "$FRESCOD_CHROME_SOCK"
B="$S/portcullis/portcullis.sock"; test ! -S "$B" || unlink "$B"
env PORTCULLISD_CRYPTA_SOCK=/tmp/i3-net/crypta.sock PORTCULLISD_FRESCO_ACTIVATION="$A" daemon -f -o /tmp/i3-portcullisd.log "$D/portcullisd" --socket "$B"
for i in $(jot 50); do test -S "$B" && break; sleep .2; done
test -S "$B"
env LIMEN_DEFAULTS=doc-viewer=org.atrium.navigator.page LIMEN_FRESCOD_ACTIVATION="$A" daemon -f -u _limen -o "$D/limen.log" "$D/limend"
su -m mailgate -c '/tmp/mail-button/portcullis daemon reload'
for i in $(jot 50); do test -S "$S/limen/limend.sock" && break; sleep .2; done
test -S "$S/limen/limend.sock"
env FRESCO_ACTIVATION="$A" FRESCO_SOCKET="$FRESCOD_SOCK" LIMEN_SOCKET="$S/limen/limend.sock" daemon -f -o "$D/arca-ui.log" -u _arca-ui "$D/arca-ui"
