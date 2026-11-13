#!/bin/sh
# Lyra on a REAL USB DAC passed through to the Mac's VM (usb-host-vm.sh).
#
#   sudo USB="08bb:27c4" sh scripts/usb-host-vm.sh   # VM with the DAC
#   sh scripts/usb-dac-check.sh                       # as yourself
#
# For each sound card snd_uaudio attached (or DAC_UNITS="1 2", pcm units):
# make it the default unit, start lyrad (_lyra) and choragusd (_choragus) as
# lyra-vm-check.sh does, and have lyra-play play a tone through Lyra for
# TONE_SECS (5) at LYRA_PLAY_VOLUME (0.25). A person listens; the script
# checks what it can see:
#   - the card is uaudio's, and lyrad opened it (sndstat shows lyrad's channel)
#   - the samples lyrad sent held the tone for ~TONE_SECS (LYRA_DUMP)
#   - the channel's underrun counter before and after (reported).
# USB isochronous transfers go guest xHCI -> qemu -> libusb -> macOS, so an
# xrun or two at start is the transport, not lyrad; the count is reported.
#
# LYRA_PREBUILT=<dir> skips the cross-build. Release builds by default.
set -u
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
VSSH="$ROOT/scripts/vssh"
SCP_OPTS="-i $HOME/.ssh/fresco_bsd_ed25519 -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o LogLevel=ERROR -P 2222"
BIN=/usr/local/bin
T=/var/run/lyra-dac
DUMP=/var/tmp/lyra-dac.raw
TONE_SECS="${TONE_SECS:-5}"
VOL="${LYRA_PLAY_VOLUME:-0.25}"
FAILS=0
say() { printf '\n=== %s\n' "$*"; }
die() { printf '\nFAILED: %s\n' "$*" >&2; exit 1; }
ok() { printf '  ok   %s\n' "$*"; }
bad() { printf '  FAIL %s\n' "$*"; FAILS=$((FAILS + 1)); }
v() { "$VSSH" "$@"; }

v true >/dev/null 2>&1 || die "cannot reach the VM (sudo USB=vid:pid sh scripts/usb-host-vm.sh)"

if [ -n "${LYRA_PREBUILT:-}" ]; then
    say "prebuilt binaries from $LYRA_PREBUILT"; SRC="$LYRA_PREBUILT"
else
    say "cross-build (host, release)"
    # shellcheck disable=SC1091
    . "$ROOT/scripts/cloud-vm.sh" env
    for c in lyra choragus; do
        (cd "$ROOT/$c" && RUSTFLAGS="${CROSS_RUSTFLAGS:-}" cargo +nightly build --quiet --target "$TARGET" \
            -Z build-std=std,panic_abort --bins --release) || die "$c cross-build"
    done
    SRC=""
fi

say "stage"
v "pkill -f 'daemon.*lyra-dac' ; pkill lyrad ; pkill choragusd ; pkill lyra-play ; true" >/dev/null 2>&1
for b in lyra/lyrad lyra/lyra-play choragus/choragusd; do
    name="${b#*/}"
    if [ -n "$SRC" ]; then src="$SRC/$name"; else src="$ROOT/${b%/*}/target/$TARGET/release/$name"; fi
    h=$(shasum -a 256 "$src" | cut -d' ' -f1)
    for try in 1 2 3; do
        v "rm -f $BIN/$name" >/dev/null 2>&1
        # shellcheck disable=SC2086
        scp $SCP_OPTS "$src" "root@localhost:$BIN/$name" >/dev/null 2>&1 || die "scp $name"
        [ "$(v "sha256 -q $BIN/$name")" = "$h" ] && break
        [ $try = 3 ] && die "$name hash mismatch after 3 copies"
    done
    v "chmod 755 $BIN/$name"; echo "  $name"
done

say "USB and sound(4)"
v "kldload -n snd_uaudio; sysctl -q hw.snd.verbose=2 >/dev/null; sleep 2; usbconfig list; echo; cat /dev/sndstat"
if [ -n "${DAC_UNITS:-}" ]; then UNITS="$DAC_UNITS"; else
    UNITS=$(v 'for p in $(sysctl -N dev.pcm 2>/dev/null | sed -n "s/^dev\.pcm\.\([0-9]*\)\.%parent$/\1/p" | sort -un); do
                 case $(sysctl -n dev.pcm.$p.%parent) in uaudio*) echo $p ;; esac; done' | tr '\n' ' ')
fi
[ -n "$UNITS" ] || die "no uaudio sound card in the guest (is the DAC passed through? see usbconfig above)"
echo "  uaudio pcm units: $UNITS"

v "set -e
    id _lyra >/dev/null 2>&1 || pw useradd _lyra -u 65016 -d /nonexistent -s /usr/sbin/nologin
    id _choragus >/dev/null 2>&1 || pw useradd _choragus -u 65017 -d /nonexistent -s /usr/sbin/nologin
    id appa >/dev/null 2>&1 || pw useradd appa -u 50001 -d /nonexistent -s /usr/sbin/nologin
    pw groupmod audio -m _lyra 2>/dev/null || true   # devfs remakes dspN root:audio 0660 on every re-attach
    install -d -o _lyra -g _choragus -m 0750 /atrium/sockets/lyrad" || die "users"

for u in $UNITS; do
    desc=$(v "sysctl -n dev.pcm.$u.%desc; sysctl -n dev.pcm.$u.%parent" | tr '\n' ' ')
    say "pcm$u: $desc"
    v "set -e
        pkill -f 'daemon.*lyra-dac' || true; pkill lyrad || true; pkill choragusd || true
        sleep 1
        sysctl -q hw.snd.default_unit=$u >/dev/null
        sysctl -q dev.pcm.$u.bitperfect=1 >/dev/null 2>&1 || true
        chmod 666 /dev/dsp$u
        rm -rf $T /var/db/choragus; install -d -o _choragus -g _choragus -m 0755 $T
        install -d -o _choragus -g _choragus -m 0700 /var/db/choragus
        printf '50001 alice org.test.a\n' > $T/registry; printf 'org.test.a audio\n' > $T/grants
        chmod 644 $T/registry $T/grants
        rm -f $DUMP; install -o _lyra -m 0644 /dev/null $DUMP
        daemon -r -R 1 -u _lyra -o $T/lyrad.log -t lyra-dac-lyrad env LYRA_DUMP=$DUMP $BIN/lyrad --serve </dev/null >/dev/null 2>&1
        daemon -r -R 1 -u _choragus -o $T/choragusd.log -t lyra-dac-choragusd \
            $BIN/choragusd --daemon $T/audio.sock /atrium/sockets/lyrad/lyrad.sock \
            --grants $T/grants --app-registry $T/registry --control $T/control.sock --state /var/db/choragus </dev/null >/dev/null 2>&1
        for i in \$(seq 1 30); do [ -S $T/audio.sock ] && break; sleep 1; done
        [ -S $T/audio.sock ]" || { bad "pcm$u: services did not come up: $(v "cat $T/lyrad.log $T/choragusd.log")"; continue; }
    sleep 2
    v "cat $T/lyrad.log"
    xr() { v "cat /dev/sndstat" | sed -n "/^pcm$u:/,/^pcm[0-9]*:/p" | grep -oE '(underruns|xruns) [0-9]+' | head -1; }
    pre=$(xr)
    printf '\n  >>> LISTEN on pcm%s: a 440 Hz tone for %s s at volume %s\n' "$u" "$TONE_SECS" "$VOL"
    # Bounded: a stream with no device behind it never drains.
    v "timeout $((TONE_SECS + 20)) su -m appa -c 'env LYRA_PLAY_FREQ=440 LYRA_PLAY_VOLUME=$VOL $BIN/lyra-play $T/audio.sock media $TONE_SECS'" \
        && ok "lyra-play finished" || bad "lyra-play failed"
    v "cat /dev/sndstat" | sed -n "/pcm$u:/,/^pcm/p" | head -20
    post=$(xr)
    echo "  device xruns: before '${pre:-?}', after '${post:-?}'"
    # The dump is what lyrad sent the device: S16 stereo at the device rate.
    stats=$(v "od -An -v -td2 $DUMP | awk '{for (i = 1; i <= NF; i++) {s = \$i; if (s < 0) s = -s; if (s > 200) {n++; q += \$i * \$i}}} END {if (n) printf \"%d %.1f\", n / 2, 20 * log(sqrt(q / n) / 32768) / log(10); else print \"0 -inf\"}'")
    frames=${stats% *}; dbfs=${stats#* }
    echo "  dump: $frames loud frames, RMS $dbfs dBFS"
    if [ "${frames:-0}" -gt $((TONE_SECS * 48000 * 7 / 10)) ]; then
        ok "pcm$u: lyrad sent ~${TONE_SECS}s of tone ($dbfs dBFS)"
    else
        bad "pcm$u: dump does not hold the tone ($frames loud frames, $dbfs dBFS)"
    fi
    v "tail -5 $T/lyrad.log; tail -5 $T/choragusd.log"
done

v "pkill -f 'daemon.*lyra-dac' ; pkill lyrad ; pkill choragusd ; true" >/dev/null 2>&1
say "result"
[ $FAILS = 0 ] && echo "PASS (the machine side; the ears are yours)" || { echo "$FAILS failure(s)"; exit 1; }
