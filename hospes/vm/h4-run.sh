#!/bin/sh
# H4 on FreeBSD (copied in by scripts/hospes-h4-vm.sh, run as root from its directory):
# install Widevine from Google's signed Chrome package with hospes-install, then play Shaka's
# Widevine test stream (angel-one: H.264 MP4 video, Opus WebM audio) through the player as
# `nobody`, the licence going by POST through Nuntius to Shaka's licence proxy.
#
# The guest has no route out: the proxy's name resolves to the fixture address, where the
# host relays to the real proxy (scripts/hospes-h4-vm.sh). Nuntius is started by
# fetcher-standin, which stands in for portcullisd's OpenFetcher because the player runs
# outside an app jail here; nuntius-vm-check.sh checks the real one.
set -u
D=$(cd "$(dirname "$0")" && pwd)
PROXY=cwip-shaka-proxy.appspot.com
PUB=${PUB:-1.2.3.100}
PSOCK=/atrium/sockets/portcullis/portcullis.sock
rc=0
if ! kldstat -q -n hospes_trap; then
    kldload "$D/hospes_trap.ko" || { echo "hospes_trap.ko did not load (the fork kernel, kernel.atrium, must be booted)"; exit 1; }
fi

# 1. The install route: InRelease (clear-signed by Google's key, pinned) -> Packages -> .deb.
rm -rf "$D/widevine"
if "$D/bin/hospes-install" widevine-deb "$D/chrome/InRelease" "$D/chrome/Packages" "$D/chrome/chrome.deb" "$D/widevine" > "$D/install.out" 2>&1; then
    echo "install: $(cat "$D/install.out")"
else
    echo "install: FAILED"; cat "$D/install.out"; exit 1
fi
LIB="$D/widevine/current/libwidevinecdm.so"
# INSTALL_ONLY=1: stop before anything runs the CDM.
[ "${INSTALL_ONLY:-0}" = 1 ] && { ls -l "$D/widevine" "$D/widevine/current/"; exit 0; }

# 2. The licence route: the proxy's name at the fixture address, the fixture CA trusted.
grep -q " $PROXY" /etc/hosts || printf '%s %s\n' "$PUB" "$PROXY" >> /etc/hosts
cat /etc/ssl/cert.pem "$D/ca.pem" > "$D/trust.pem"
pkill -x portcullisd 2>/dev/null
pkill -f fetcher-standin 2>/dev/null
sleep 1
daemon -f -o "$D/standin.log" "$D/bin/fetcher-standin" "$PSOCK" "$D/bin/nuntiusd" "$D/trust.pem" 65023
sleep 2
test -S "$PSOCK" || { echo "fetcher-standin did not start"; cat "$D/standin.log"; exit 1; }

# 3. Play.
mkdir -p "$D/store"; chown nobody "$D/store"
chmod -R a+rX "$D"
export ATRIUM_PLAYER_DECODE="$D/bin/atrium-player-decode" ATRIUM_PLAYER_HOSPES_CDM="$D/bin/hospes-cdm" ATRIUM_PLAYER_CDM="$LIB"
export ATRIUM_PLAYER_LICENCE_URL="https://$PROXY/no_auth" ATRIUM_PLAYER_CDM_STORE="$D/store"
# The VP9 WebM carries only key ids (no PSSH): Shaka's proxy refuses a request made from one,
# and its player takes the PSSH from the manifest, which this check does not read.
for v in ${VIDEOS:-v-0144p-0100k-libx264.mp4}; do
    t0=$(date +%s)
    out=$(su -m nobody -c "$D/bin/atrium-player --headless --feed $D/media/$v $D/media/a-eng-0096k-libopus-2c.webm" 2>"$D/player-$v.err")
    t1=$(date +%s)
    echo "$out" | grep -v '^video ' > "$D/player-$v.out"
    frames=$(echo "$out" | grep -c '^video ')
    distinct=$(echo "$out" | awk '/^video /{print $3}' | sort -u | wc -l | tr -d ' ')
    last=$(echo "$out" | awk '/^video /{p=$2} END{print p+0}')
    pcm=$(echo "$out" | awk '/^audio /{print $2}')
    sb=$(echo "$out" | grep -c '^sandboxed true')
    # 60 s at 25 frames/s; Opus at 48 kHz.
    if echo "$out" | grep -q '^ended' && [ "$frames" -ge 1490 ] && [ "$last" -ge 59000000000 ] && [ "${pcm:-0}" -ge 2800000 ] && [ "$sb" -ge 1 ]; then
        echo "player $v: $frames frames ($distinct distinct, last at $((last / 1000000)) ms), $pcm audio frames, sandboxed, $((t1 - t0)) s"
    else
        echo "player $v: FAILED ($frames frames, last at $last ns, audio ${pcm:-none})"; cat "$D/player-$v.out" | head -20; tail -20 "$D/player-$v.err"; rc=1
    fi
done
echo "licence POSTs relayed: see the host's relay log"
pkill -f fetcher-standin 2>/dev/null
left=$(pgrep -f hospes-cdm | wc -l | tr -d ' ')
echo "hospes-cdm processes left: $left"
exit $rc
