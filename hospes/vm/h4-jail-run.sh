#!/bin/sh
# H4 the way the system runs it (copied in by `JAILED=1 scripts/hospes-h4-vm.sh`, run as
# root from its directory): Shaka's Widevine stream played by the player as a jailed app.
#
#   - Widevine is installed by hospes-install as the system's CDM for com.widevine.alpha
#     (/var/db/atrium/cdm/com.widevine.alpha/current/).
#   - The player runs in an app jail of its own (a test bundle per clip, holding
#     `net-fetch` and `protected-media`). It asks portcullisd for a CDM (OpenCdm): hospes-cdm
#     then runs in a one-shot jail as _hospes-cdm with the store for the app and origin.
#   - The licence goes through the system's fetcher (OpenFetcher): nuntiusd in its own jail
#     as _nuntius, to the proxy's name, which maps to the host's relay.
#
# Checks for each clip: it plays to the end (as h4-run.sh), the store is the app's and
# origin's and is _hospes-cdm's, and no CDM or fetcher jail is left. The player's own
# stderr is in $D/jail-<clip>.err.
set -u
D=$(cd "$(dirname "$0")" && pwd)
PROXY=cwip-shaka-proxy.appspot.com
PUB=${PUB:-1.2.3.100}
KS=com.widevine.alpha
ORIGIN=https://shaka-player-demo.appspot.com
ORIGIN_DIR='https%3A%2F%2Fshaka-player-demo.appspot.com'
PSOCK=/atrium/sockets/portcullis/portcullis.sock
PLOG=/var/log/atrium/portcullisd-h4jail.log
export PATH=/usr/local/bin:$PATH
rc=0
fail() { echo "FAILED: $*"; exit 1; }

kldstat | grep -q hospes_trap || kldload "$D/hospes_trap.ko" || fail "hospes_trap.ko (the fork kernel must be booted)"

# 1. The system's CDM, from Google's signed package.
CDM=/var/db/atrium/cdm/$KS
mkdir -p /var/db/atrium/cdm && chmod 755 /var/db/atrium/cdm
"$D/bin/hospes-install" widevine-deb "$D/chrome/InRelease" "$D/chrome/Packages" "$D/chrome/chrome.deb" "$CDM" > "$D/install.out" 2>&1 \
    || { cat "$D/install.out"; fail "install"; }
echo "install: $(cat "$D/install.out")"

# 2. The Portcullis stack: jaild with resource limits, portcullisd on its socket.
P=/etc/atrium/jaild.policy.toml
if grep -q '^allow_rctl' $P; then sed -i '' 's/^allow_rctl.*/allow_rctl = true/' $P
elif grep -q '^\[resource_control\]' $P; then sed -i '' '/^\[resource_control\]/a\
allow_rctl = true
' $P
else printf '\n[resource_control]\nallow_rctl = true\n' >> $P; fi
service atrium_jaild restart >/dev/null 2>&1; sleep 2
pkill -x portcullisd; pkill -f fetcher-standin; sleep 1
mkdir -p /atrium/sockets/portcullis; rm -f $PSOCK
daemon -f -o $PLOG /usr/local/bin/portcullisd --socket $PSOCK; sleep 3
pgrep -f atrium-jaild >/dev/null && test -S $PSOCK || fail "jaild / portcullisd"

# 3. Bundles: the CDM host, the fetcher (trusting the fixture CA, the proxy's name at the
#    relay), and one player app per clip.
test -f /root/e2e-key.pem || openssl ecparam -name prime256v1 -genkey -noout -out /root/e2e-key.pem
openssl ec -in /root/e2e-key.pem -pubout -out /root/e2e-pub.pem 2>/dev/null
mkdir -p /etc/atrium/publishers && cp /root/e2e-pub.pem /etc/atrium/publishers/e2e.pem
grep -q " $PROXY" /etc/hosts || printf '%s %s\n' "$PUB" "$PROXY" >> /etc/hosts
cat /etc/ssl/cert.pem "$D/ca.pem" > "$D/trust.pem"
(cd "$D" && sh hospes-cdm-bundle.sh "$D/bin/hospes-cdm" /root/e2e-key.pem /usr/local/bin/opifex) > "$D/bundles.log" 2>&1 \
    && (cd "$D" && sh nuntius-bundle.sh "$D/bin/nuntiusd" /root/e2e-key.pem /usr/local/bin/opifex "$D/trust.pem") >> "$D/bundles.log" 2>&1 \
    || { tail -5 "$D/bundles.log"; fail "system bundles"; }

# The player's libraries (its decoders) at their own paths: opifex resolves only the
# entry's, and the entry here is the launcher.
libs() { ldd -f '%p\n' "$@" 2>/dev/null | grep '^/' | sort -u; }

for v in ${VIDEOS:-v-0144p-0100k-libx264.mp4 v-0144p-0100k-vp9.webm}; do
    case "$v" in *.webm) id=org.atrium.test.widevine-webm; mpd=1 ;; *) id=org.atrium.test.widevine-mp4; mpd=0 ;; esac
    B=$D/bundle-$id
    rm -rf "$B" && mkdir -p "$B/bin" "$B/media"
    cp "$D/bin/atrium-player-bundle-launch" "$D/bin/atrium-player" "$D/bin/atrium-player-decode" "$B/bin/"
    cp "$D/media/$v" "$D/media/a-eng-0096k-libopus-2c.webm" "$B/media/"
    [ $mpd = 1 ] && cp "$D/media/dash.mpd" "$B/media/"
    # Packages' libraries go to /usr/lib too: a jail has no ld-elf.so.hints naming
    # /usr/local/lib, and the rtld's built-in path is /lib:/usr/lib.
    for l in $(libs "$B/bin/atrium-player" "$B/bin/atrium-player-decode") /libexec/ld-elf.so.1; do
        case "$l" in /usr/local/lib/*) to=/usr/lib/${l#/usr/local/lib/} ;; *) to=$l ;; esac
        mkdir -p "$B$(dirname "$to")" && cp "$l" "$B$to"
    done
    {
        echo "env ATRIUM_PLAYER_DECODE=/bin/atrium-player-decode"
        echo "env ATRIUM_PLAYER_CDM=system:$KS"
        echo "env ATRIUM_PLAYER_CDM_ORIGIN=$ORIGIN"
        echo "env ATRIUM_PLAYER_LICENCE_URL=https://$PROXY/no_auth"
        [ $mpd = 1 ] && echo "env ATRIUM_PLAYER_CDM_MPD=/media/dash.mpd"
        [ "${TRACE:-0}" = 1 ] && echo "env ATRIUM_PLAYER_TRACE_CDM=1" && echo "env NUNTIUS_FETCHER_STDERR=inherit"
        echo "arg --headless"
        echo "arg --feed"
        echo "arg /media/$v"
        echo "arg /media/a-eng-0096k-libopus-2c.webm"
    } > "$B/play.conf"
    printf '[app]\nid = "%s"\nname = "Widevine check"\nversion = "1"\nentry = "bin/atrium-player-bundle-launch"\n\n[capabilities]\nnet-fetch = true\nprotected-media = true\n\n[resources]\nmemory = "2G"\ncpu = 100\n' "$id" > "$B/atrium.toml"
    openssl dgst -sha256 -sign /root/e2e-key.pem -out "$B/atrium.toml.sig" "$B/atrium.toml"
    /usr/local/bin/opifex install "$B" >> "$D/bundles.log" 2>&1 || { tail -5 "$D/bundles.log"; fail "install $id"; }
    rm -rf "/var/db/atrium/cdm-store/$id"

    # 4. Play, as the app.
    t0=$(date +%s)
    out=$(timeout "${PLAY_TIMEOUT:-900}" /usr/local/bin/portcullis launch --no-prompt "$id" 2>"$D/jail-$v.err")
    t1=$(date +%s)
    echo "$out" | grep -v '^video ' > "$D/jail-$v.out"
    frames=$(echo "$out" | grep -c '^video ')
    distinct=$(echo "$out" | awk '/^video /{print $3}' | sort -u | wc -l | tr -d ' ')
    last=$(echo "$out" | awk '/^video /{p=$2} END{print p+0}')
    pcm=$(echo "$out" | awk '/^audio /{print $2}')
    sb=$(echo "$out" | grep -c '^sandboxed true')
    if echo "$out" | grep -q '^ended' && [ "$frames" -ge 1490 ] && [ "$last" -ge 59000000000 ] && [ "${pcm:-0}" -ge 2800000 ] && [ "$sb" -ge 1 ]; then
        echo "PASS jailed player $v: $frames frames ($distinct distinct, last at $((last / 1000000)) ms), $pcm audio frames, sandboxed, $((t1 - t0)) s"
    else
        echo "FAIL jailed player $v: $frames frames, last at $last ns, audio ${pcm:-none}"
        head -20 "$D/jail-$v.out"; tail -30 "$D/jail-$v.err"; grep -a 'CDM\|Fetcher\|cdm\|nuntius' $PLOG | tail -10; rc=1
    fi
    S=/var/db/atrium/cdm-store/$id/$KS/$ORIGIN_DIR
    st=$(stat -f '%Su %Lp' "$S" 2>&1)
    if [ "$st" = "_hospes-cdm 700" ]; then echo "PASS the CDM's store is the app's and origin's, 0700, _hospes-cdm's"
    else echo "FAIL the CDM's store: $S: $st"; rc=1; fi
done

# 5. Who ran what, and nothing left.
grep -a 'CDM\|fetcher' $PLOG | tail -4 | sed 's/^/  log: /'
sleep 3
lj=$(jls -d name | grep -c -e hospes-cdm -e nuntius)
lp=$(pgrep -x 'hospes-cdm|nuntiusd' | wc -l | tr -d ' ')
if [ "$lj" = 0 ] && [ "$lp" = 0 ]; then echo "PASS no CDM or fetcher jail or process left"
else echo "FAIL left behind: jails=$lj procs=$lp"; rc=1; fi
exit $rc
