#!/bin/sh
# Lyra in the VM (docs/spec/lyra-v1.md §8): lyrad as _lyra on a real sound(4)
# device, choragusd as _choragus, apps at registry uids, every result read back
# from LYRA_DUMP (the exact samples lyrad sent the device).
#
#   QEMU_EXTRA="$(sh scripts/lyra-vm-check.sh qemu-args)" sh scripts/cloud-vm.sh up
#   sh scripts/cloud-vm.sh lyra          # cross-builds, stages, runs this
#
# Checks (numbers are lyra-v1.md §8's):
#   4  identity: an app with no grant, an uid not in the registry, root and an
#      app talking to lyrad directly: all refused.
#   5  two apps at 500 and 700 Hz; the volume UI sets one to 0.25 (-12 dB):
#      only its tone drops 12 dB. A communication stream ducks media 18 dB.
#   2  a hostile stream (NaN, infinity, wild ring header) beside a 1 kHz tone:
#      lyrad lives and the tone is untouched.
#   7  kill lyrad while an app plays: the app is told, the supervisor restarts
#      lyrad and choragusd, and a new stream plays.
# Not here: 3 (no device in app jails) is portcullis-jail's devfs test plus
# the shipped manifests; 6 (clock accuracy, ±2 ms over 10 min) needs real
# timing, which QEMU's emulated CPU doesn't give: run it on the Mac's VM.
#
# Needs CROSS_RUSTFLAGS and TARGET (cloud-vm.sh's cross_env sets them).
set -u
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
VSSH="$ROOT/scripts/vssh"
SCP_OPTS="-i $HOME/.ssh/fresco_bsd_ed25519 -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o LogLevel=ERROR -P 2222"
OUT="${LYRA_VM_OUT:-$HOME/atrium-vm/lyra}"
BIN=/usr/local/bin
T=/var/run/lyra-test
DUMP=/var/tmp/lyra-mix.raw
FAILS=0

if [ "${1:-}" = qemu-args ]; then
    # QEMU's HDA codec with no host audio: the guest's snd_hda drives it at
    # the device rate, and LYRA_DUMP is the ground truth.
    echo "-audiodev none,id=snd0 -device intel-hda -device hda-output,audiodev=snd0"
    exit 0
fi

say() { printf '\n=== %s\n' "$*"; }
die() { printf '\nFAILED: %s\n' "$*" >&2; exit 1; }
ok() { printf '  ok   %s\n' "$*"; }
bad() { printf '  FAIL %s\n' "$*"; FAILS=$((FAILS + 1)); }
v() { "$VSSH" "$@"; }

say "cross-build (host)"
: "${TARGET:?run through scripts/cloud-vm.sh lyra}"
for c in lyra choragus; do
    (cd "$ROOT/$c" && RUSTFLAGS="${CROSS_RUSTFLAGS:-}" cargo +nightly build --quiet --target "$TARGET" \
        -Z build-std=std,panic_abort --bins) || die "$c cross-build"
done
echo "  built"

say "stage"
v true >/dev/null 2>&1 || die "cannot reach the VM (sh scripts/cloud-vm.sh up)"
v "pkill -f 'daemon.*lyra-test' ; pkill lyrad ; pkill choragusd ; pkill lyra-play ; true" >/dev/null 2>&1
for b in lyra/lyrad lyra/lyra-play lyra/lyra-vmtest choragus/choragusd; do
    src="$ROOT/${b%/*}/target/$TARGET/debug/${b#*/}"
    name="${b#*/}"
    h=$(sha256sum "$src" | cut -d' ' -f1)
    for try in 1 2 3; do
        v "rm -f $BIN/$name" >/dev/null 2>&1
        # shellcheck disable=SC2086
        scp $SCP_OPTS "$src" "root@localhost:$BIN/$name" >/dev/null 2>&1 || die "scp $name"
        [ "$(v "sha256 -q $BIN/$name")" = "$h" ] && break
        [ $try = 3 ] && die "$name hash mismatch after 3 copies"
    done
    v "chmod 755 $BIN/$name"
    echo "  $name"
done

say "commission (users, device, registry, grants)"
v "set -e
    kldload -n snd_hda
    for i in 1 2 3 4 5 6 7 8 9 10; do [ -e /dev/dsp0 ] && break; sleep 1; done
    [ -e /dev/dsp0 ] || { echo 'no /dev/dsp0 (boot with QEMU_EXTRA from qemu-args)'; exit 1; }
    sysctl -q dev.pcm.0.bitperfect=1 >/dev/null 2>&1 || true
    id _lyra >/dev/null 2>&1 || pw useradd _lyra -u 65016 -d /nonexistent -s /usr/sbin/nologin
    id _choragus >/dev/null 2>&1 || pw useradd _choragus -u 65017 -d /nonexistent -s /usr/sbin/nologin
    for u in appa:50001 appb:50002 appc:50003 appui:50004 stranger:50009; do
        id \${u%:*} >/dev/null 2>&1 || pw useradd \${u%:*} -u \${u#*:} -d /nonexistent -s /usr/sbin/nologin
    done
    install -d -o _lyra -g _choragus -m 0750 /atrium/sockets/lyrad
    rm -rf $T /var/db/choragus; install -d -o _choragus -g _choragus -m 0755 $T
    install -d -o _choragus -g _choragus -m 0700 /var/db/choragus
    printf '50001 alice org.test.a\n50002 alice org.test.b\n50003 alice org.test.c\n50004 alice org.test.ui\n' > $T/registry
    printf 'org.test.a audio\norg.test.b audio\norg.test.ui audio-control\n' > $T/grants
    chmod 644 $T/registry $T/grants
    rm -f $DUMP; install -o _lyra -m 0644 /dev/null $DUMP
    chmod 666 /dev/dsp0" || die "commission"
echo "  snd_hda, users, registry, grants"

say "start lyrad (_lyra) and choragusd (_choragus) under daemon(8) -r"
v "daemon -r -R 1 -u _lyra -o $T/lyrad.log -t lyra-test-lyrad env LYRA_DUMP=$DUMP $BIN/lyrad --serve </dev/null >/dev/null 2>&1
   daemon -r -R 1 -u _choragus -o $T/choragusd.log -t lyra-test-choragusd \
       $BIN/choragusd --daemon $T/audio.sock /atrium/sockets/lyrad/lyrad.sock \
       --grants $T/grants --app-registry $T/registry --control $T/control.sock --state /var/db/choragus </dev/null >/dev/null 2>&1
   for i in \$(seq 1 30); do [ -S $T/audio.sock ] && [ -S $T/control.sock ] && break; sleep 1; done
   [ -S $T/audio.sock ]" || die "services did not come up: $(v "cat $T/lyrad.log $T/choragusd.log")"
# Let the device open and the dump start.
sleep 3
echo "  up"

mark() { v "stat -f %z $DUMP"; }

say "4: identity"
r=$(v "su -m appc -c '$BIN/lyra-play $T/audio.sock media 1' 2>&1; echo rc=\$?")
case "$r" in *rc=1*) ok "an app without the audio grant is refused" ;; *) bad "ungranted app: $r" ;; esac
r=$(v "su -m stranger -c '$BIN/lyra-play $T/audio.sock media 1' 2>&1; echo rc=\$?")
case "$r" in *rc=1*) ok "an uid not in the registry is refused" ;; *) bad "unregistered uid: $r" ;; esac
r=$(v "$BIN/lyra-vmtest lyrad-raw /atrium/sockets/lyrad/lyrad.sock")
case "$r" in refused*) ok "root talking to lyrad directly is refused" ;; *) bad "root to lyrad: $r" ;; esac
r=$(v "su -m appa -c '$BIN/lyra-vmtest lyrad-raw /atrium/sockets/lyrad/lyrad.sock'")
case "$r" in refused*) ok "an app can't reach lyrad's socket" ;; *) bad "app to lyrad: $r" ;; esac
r=$(v "su -m appa -c '$BIN/lyra-vmtest ctl $T/control.sock list' 2>&1; echo rc=\$?")
case "$r" in *rc=1*) ok "an app without audio-control can't use the volume UI socket" ;; *) bad "ungranted control: $r" ;; esac

say "5: two apps, the user's volume, ducking"
M0=$(mark)
v "su -m appa -c 'env LYRA_PLAY_FREQ=500 $BIN/lyra-play $T/audio.sock media 10' >$T/a.log 2>&1 </dev/null &
   su -m appb -c 'env LYRA_PLAY_FREQ=700 $BIN/lyra-play $T/audio.sock media 10' >$T/b.log 2>&1 </dev/null &
   sleep 4"
M1=$(mark)
r=$(v "su -m appui -c '$BIN/lyra-vmtest ctl $T/control.sock app org.test.b 0.25'")
echo "$r" | sed 's/^/  ui: /'
v "sleep 4"
M2=$(mark)
v "while pgrep -q lyra-play; do sleep 1; done"
r=$(v "su -m appui -c '$BIN/lyra-vmtest ctl $T/control.sock app org.test.b 1'")
M3=$(mark)
v "su -m appa -c 'env LYRA_PLAY_FREQ=500 $BIN/lyra-play $T/audio.sock media 20' >$T/a.log 2>&1 </dev/null &
   sleep 4"
M4=$(mark)
v "su -m appb -c 'env LYRA_PLAY_FREQ=590 $BIN/lyra-play $T/audio.sock comms 3' >$T/b.log 2>&1"
M5=$(mark)
v "sleep 2"
M6=$(mark)
v "while pgrep -q lyra-play; do sleep 1; done"

say "2: hostile stream beside a 1 kHz tone"
# The user mutes the hostile app, so what's left in the mix from it is what
# lyrad let through: nothing. Its ring header is rewritten all the while.
v "su -m appui -c '$BIN/lyra-vmtest ctl $T/control.sock mute org.test.b 1'" >/dev/null
M7=$(mark)
v "su -m appa -c 'env LYRA_PLAY_FREQ=1000 $BIN/lyra-play $T/audio.sock media 12' >$T/a.log 2>&1 </dev/null &
   sleep 4"
M8=$(mark)
v "su -m appb -c '$BIN/lyra-vmtest hostile $T/audio.sock 5' >$T/h.log 2>&1"
M9=$(mark)
v "while pgrep -q lyra-play; do sleep 1; done"
r=$(v "pgrep -u _lyra lyrad >/dev/null && echo alive")
[ "$r" = alive ] && ok "lyrad lives through the hostile stream" || bad "lyrad died: $(v "tail -5 $T/lyrad.log")"
v "su -m appui -c '$BIN/lyra-vmtest ctl $T/control.sock mute org.test.b 0'" >/dev/null

say "7: restart"
v "su -m appa -c '$BIN/lyra-play $T/audio.sock media 30' >$T/r.log 2>&1 </dev/null &
   sleep 4; pkill -u _lyra lyrad; sleep 3"
r=$(v "cat $T/r.log")
case "$r" in *"stream lost"*) ok "the app is told its stream is gone" ;; *) bad "no stream-lost: $r" ;; esac
r=$(v "for i in \$(seq 1 30); do su -m appa -c '$BIN/lyra-play $T/audio.sock media 1' >/dev/null 2>&1 && { echo back after \$i; exit 0; }; sleep 1; done; echo never")
case "$r" in back*) ok "lyrad and choragusd restarted; a new stream plays ($r tries)" ;; *) bad "no restart: $(v "tail -5 $T/choragusd.log")" ;; esac

say "analyse the dump (host)"
mkdir -p "$OUT"
# shellcheck disable=SC2086
scp $SCP_OPTS "root@localhost:$DUMP" "$OUT/mix.raw" >/dev/null 2>&1 || die "pull the dump"
v "cat $T/lyrad.log" > "$OUT/lyrad.log" 2>&1
v "cat $T/choragusd.log" > "$OUT/choragusd.log" 2>&1
python3 -I - "$OUT/mix.raw" "$M0" "$M1" "$M2" "$M3" "$M4" "$M5" "$M6" "$M7" "$M8" "$M9" <<'PY' || FAILS=$((FAILS + 1))
import math, struct, sys
path = sys.argv[1]
m = [int(x) // 4 for x in sys.argv[2:]]   # byte offsets -> stereo frames
raw = open(path, 'rb').read()
n = len(raw) // 4
left = struct.unpack('<%dh' % (n * 2), raw[:n * 4])[0::2]
RATE, WIN = 48000, 4800
def amp(a, b, f):
    """Median Goertzel amplitude (dBFS) of f over 100 ms windows in frames [a, b)."""
    vals = []
    k = 2 * math.cos(2 * math.pi * f / RATE)
    for s in range(a, b - WIN, WIN):
        s1 = s2 = 0.0
        for x in left[s:s + WIN]:
            s0 = x / 32768.0 + k * s1 - s2
            s2, s1 = s1, s0
        p = s1 * s1 + s2 * s2 - k * s1 * s2
        vals.append(10 * math.log10(max(p, 1e-20)) - 20 * math.log10(WIN / 2))
    vals.sort()
    return vals[len(vals) // 2] if vals else -200.0
fails = 0
def check(name, got, want, tol):
    global fails
    good = abs(got - want) <= tol
    print('  %s %s: %+.1f dB (want %+.1f ± %.1f)' % ('ok  ' if good else 'FAIL', name, got, want, tol))
    fails += not good
S = RATE // 2   # skip half a second after each change (ramps, start-up)
print('  dump: %d frames (%.1f s of device time)' % (n, n / RATE))
a500, a700 = amp(m[0] + S, m[1], 500), amp(m[0] + S, m[1], 700)
b500, b700 = amp(m[1] + S, m[2], 500), amp(m[1] + S, m[2], 700)
print('  before: 500 Hz %.1f dBFS, 700 Hz %.1f dBFS' % (a500, a700))
check('5: the turned-down app (700 Hz)', b700 - a700, -12.04, 1.5)
check('5: the other app (500 Hz)', b500 - a500, 0.0, 1.0)
d0, d1, d2 = amp(m[3] + S, m[4], 500), amp(m[4] + S, m[5], 500), amp(m[5] + S, m[6], 500)
check('5: media ducked by a call', d1 - d0, -18.0, 2.0)
check('5: media back after the call', d2 - d0, 0.0, 1.5)
h0, h1 = amp(m[7] + S, m[8], 1000), amp(m[8] + S // 5, m[9], 1000)
check('2: 1 kHz tone beside the hostile stream', h1 - h0, 0.0, 1.0)
for f in (3000, 6857, 9000):
    leak = amp(m[8] + S // 5, m[9], f)
    good = leak < h1 - 40
    print('  %s 2: nothing else at %d Hz: %.1f dBFS' % ('ok  ' if good else 'FAIL', f, leak))
    fails += not good
clipped = sum(1 for x in left[m[8]:m[9]] if x in (32767, -32768))
print('  %s 2: no clipped samples during the hostile stream (%d)' % ('ok  ' if clipped == 0 else 'FAIL', clipped))
fails += clipped != 0
sys.exit(1 if fails else 0)
PY

say "stop"
v "pkill -f 'daemon.*lyra-test'; pkill -u _lyra lyrad; pkill -u _choragus choragusd; true" >/dev/null 2>&1
echo "  dump and logs in $OUT"
[ "$FAILS" = 0 ] && { say "lyra VM check PASSED"; exit 0; }
say "lyra VM check: $FAILS FAILED"
exit 1
