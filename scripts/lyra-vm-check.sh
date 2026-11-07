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
#   balance: the output's balance 0.5 takes the left 6 dB down, the right not.
#   surround: headphone virtual surround on (lyra-spatial.md §3.1, rendered
#      through FABIAN's measured HRTFs, S3): a 500 Hz tone in a 7.1 stream's
#      left surround reaches the left ear 4.5 dB above the right; a Pro stream
#      beside it passes untouched.
#   7  kill lyrad while an app plays: the app is told, the supervisor restarts
#      lyrad and choragusd, and a new stream plays.
# Not here: 3 (no device in app jails) is portcullis-jail's devfs test plus
# the shipped manifests.
#
#   sh scripts/lyra-vm-check.sh clock     # only check 6, CLOCK_SECS (600) long
#   6  clock accuracy: two apps (48 and 44.1 kHz) play clicks at known frames;
#      each click's heard time, predicted by the clock page 30-300 ms ahead,
#      is compared with when the device consumed that frame (the dump gives
#      the frame, LYRA_POSLOG the device position over CLOCK_MONOTONIC).
#      Pass: every click within ±2 ms, rate_ppm stable. Needs real timing:
#      run it on the Mac's VM (HVF), not under TCG.
#   CLOCK_DEVICE=1 sh scripts/lyra-vm-check.sh clock
#      The same against a test external device (lyra-external-device.md §7,
#      E3): `lyra-vmtest device` takes lyrad's mix at 48 kHz on CLOCK_MONOTONIC
#      with 150 ms of delay, and prints when each click is heard. Its clock
#      is the system clock, so this judges lyrad's arithmetic and the
#      external path anywhere, the cloud VM included.
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
POSLOG=/var/tmp/lyra-pos.log
MODE="${1:-all}"
CLOCK_SECS="${CLOCK_SECS:-600}"
CLOCK_DEVICE="${CLOCK_DEVICE:-0}"
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

# LYRA_PREBUILT=<dir>: take lyrad, lyra-play, lyra-vmtest and choragusd from
# <dir> instead of cross-building (a host with no Rust, e.g. the R9700 box).
if [ -n "${LYRA_PREBUILT:-}" ]; then
    say "prebuilt binaries from $LYRA_PREBUILT"
else
say "cross-build (host)"
: "${TARGET:?run through scripts/cloud-vm.sh lyra}"
for c in lyra choragus; do
    (cd "$ROOT/$c" && RUSTFLAGS="${CROSS_RUSTFLAGS:-}" cargo +nightly build --quiet --target "$TARGET" \
        -Z build-std=std,panic_abort --bins) || die "$c cross-build"
done
echo "  built"
fi

say "stage"
v true >/dev/null 2>&1 || die "cannot reach the VM (sh scripts/cloud-vm.sh up)"
v "pkill -f 'daemon.*lyra-test' ; pkill lyrad ; pkill choragusd ; pkill lyra-play ; true" >/dev/null 2>&1
for b in lyra/lyrad lyra/lyra-play lyra/lyra-vmtest choragus/choragusd; do
    name="${b#*/}"
    if [ -n "${LYRA_PREBUILT:-}" ]; then src="$LYRA_PREBUILT/$name"; else src="$ROOT/${b%/*}/target/$TARGET/debug/$name"; fi
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
    for u in appa:50001 appb:50002 appc:50003 appui:50004 appdev:50005 stranger:50009; do
        id \${u%:*} >/dev/null 2>&1 || pw useradd \${u%:*} -u \${u#*:} -d /nonexistent -s /usr/sbin/nologin
    done
    install -d -o _lyra -g _choragus -m 0750 /atrium/sockets/lyrad
    rm -rf $T /var/db/choragus; install -d -o _choragus -g _choragus -m 0755 $T
    install -d -o _choragus -g _choragus -m 0700 /var/db/choragus
    printf '50001 alice org.test.a\n50002 alice org.test.b\n50003 alice org.test.c\n50004 alice org.test.ui\n50005 alice org.test.dev\n' > $T/registry
    printf 'org.test.a audio\norg.test.b audio\norg.test.ui audio-control\norg.test.dev audio-device\n' > $T/grants
    chmod 644 $T/registry $T/grants
    rm -f $DUMP $POSLOG; install -o _lyra -m 0644 /dev/null $DUMP; install -o _lyra -m 0644 /dev/null $POSLOG
    chmod 666 /dev/dsp0" || die "commission"
echo "  snd_hda, users, registry, grants"

say "start lyrad (_lyra) and choragusd (_choragus) under daemon(8) -r"
v "daemon -r -R 1 -u _lyra -o $T/lyrad.log -t lyra-test-lyrad env LYRA_DUMP=$DUMP LYRA_POSLOG=$POSLOG $BIN/lyrad --serve </dev/null >/dev/null 2>&1
   daemon -r -R 1 -u _choragus -o $T/choragusd.log -t lyra-test-choragusd \
       $BIN/choragusd --daemon $T/audio.sock /atrium/sockets/lyrad/lyrad.sock \
       --grants $T/grants --app-registry $T/registry --control $T/control.sock --device $T/device.sock --state /var/db/choragus </dev/null >/dev/null 2>&1
   for i in \$(seq 1 30); do [ -S $T/audio.sock ] && [ -S $T/control.sock ] && break; sleep 1; done
   [ -S $T/audio.sock ]" || die "services did not come up: $(v "cat $T/lyrad.log $T/choragusd.log")"
# Let the device open and the dump start.
sleep 3
echo "  up"

if [ "$MODE" = clock ]; then
    say "6: clock accuracy ($CLOCK_SECS s; two apps, 48 and 44.1 kHz)"
    v "rm -f $T/dev.txt; touch $T/dev.txt"
    if [ "$CLOCK_DEVICE" = 1 ]; then
        echo "  on the test external device (150 ms delay)"
        v "su -m appdev -c '$BIN/lyra-vmtest device $T/device.sock $((CLOCK_SECS + 15)) 150' >$T/dev.txt 2>$T/dev.err </dev/null &
           sleep 3; cat $T/dev.err"
    fi
    v "su -m appa -c '$BIN/lyra-vmtest clock $T/audio.sock 48000 $CLOCK_SECS 0.25' >$T/c48.txt 2>$T/c48.err </dev/null &
       su -m appb -c '$BIN/lyra-vmtest clock $T/audio.sock 44100 $CLOCK_SECS 0.75' >$T/c44.txt 2>$T/c44.err </dev/null &
       sleep 2; while pgrep -q lyra-vmtest; do sleep 5; done"
    v "pkill -f 'daemon.*lyra-test'; pkill -u _lyra lyrad; pkill -u _choragus choragusd; true" >/dev/null 2>&1
    mkdir -p "$OUT"
    for f in "$DUMP:mix.raw" "$POSLOG:pos.log" "$T/c48.txt:c48.txt" "$T/c44.txt:c44.txt" "$T/lyrad.log:lyrad.log" "$T/dev.txt:dev.txt"; do
        # shellcheck disable=SC2086
        scp $SCP_OPTS "root@localhost:${f%%:*}" "$OUT/${f#*:}" >/dev/null 2>&1 || die "pull ${f%%:*}"
    done
    v "cat $T/c48.err $T/c44.err; [ $CLOCK_DEVICE = 1 ] && cat $T/dev.err" | sed 's/^/  app: /'
    python3 -I - "$OUT" <<'PY' || FAILS=$((FAILS + $?))
import struct, sys
out = sys.argv[1]
RATE = 48000
raw = open(out + '/mix.raw', 'rb').read()
n = len(raw) // 4
left = struct.unpack('<%dh' % (n * 2), raw[:n * 4])[0::2]
pos = [tuple(int(x) for x in l.split()) for l in open(out + '/pos.log') if len(l.split()) == 4]
now = [p[0] for p in pos]
played = [p[1] for p in pos]
queued = [p[2] - p[3] for p in pos]   # written - GETODELAY
preds = []
for name in ('c48.txt', 'c44.txt'):
    for l in open(out + '/' + name):
        w = l.split()
        if w and w[0] == 'click':
            preds.append((int(w[1]), int(w[2]), int(w[4]), int(w[5])))
# Clicks in the dump: runs above a third of full scale, placed at their
# amplitude-weighted centroid (a resampled click spreads over two frames).
clicks, i = [], 0
while i < n:
    if abs(left[i]) > 10000:
        j = i
        while j < n and j - i < 8 and abs(left[j]) > 300:
            j += 1
        w = [abs(left[k]) for k in range(i, j)]
        clicks.append(sum(k * a for k, a in zip(range(i, j), w)) / sum(w))
        i = j + 8
    else:
        i += 1
import bisect
def when(series, d):
    # The time the device position passed frame d: a least-squares line
    # through the position samples within half a second of it.
    k = bisect.bisect_left(series, d)
    lo, hi = max(0, k - RATE // 256), min(len(series), k + RATE // 256)
    xs, ys = series[lo:hi], now[lo:hi]
    if len(xs) < 10:
        return None
    mx, my = sum(xs) / len(xs), sum(ys) / len(ys)
    sxx = sum((x - mx) ** 2 for x in xs)
    if sxx == 0:
        return None
    b = sum((x - mx) * (y - my) for x, y in zip(xs, ys)) / sxx
    return my + b * (d - mx)
# The truth is when the device's queue (frames written minus GETODELAY)
# passed the click's frame: sound(4)'s own position on OSS path A. (GETOPTR
# counts frames taken into the device's buffer, not played: shown for
# reference only.)
truth = [(d, when(queued, d), when(played, d)) for d in clicks]
truth = [t for t in truth if t[1] is not None]
# Times the device ran dry: nothing queued (lyrad was late), after the
# first second. Each is a gap everyone hears and moves the clock by its
# length; a prediction made before one can't know it. lyra-vmtest predicts
# 30-300 ms ahead, so a click with a dry spell in the 400 ms before it is
# judged apart: the clock is checked on the rest, the gaps are reported.
dry = [now[i] for i in range(1, len(pos)) if pos[i][3] == 0 and now[i] - now[0] > 1e9]
dry_spells = sum(1 for i in range(len(dry)) if i == 0 or dry[i] - dry[i - 1] > 50e6)
def after_dry(t):
    k = bisect.bisect_right(dry, t)
    return k > 0 and t - dry[k - 1] < 400e6
errs, clean, alt, ppms, unmatched = [], [], [], [], 0
for rate, k, pred, ppm in preds:
    best = min(truth, key=lambda t: abs(t[1] - pred), default=None)
    if best is None or abs(best[1] - pred) > 200e6:
        unmatched += 1
        continue
    errs.append((pred - best[1]) / 1e6)
    if not after_dry(best[1]):
        clean.append((pred - best[1]) / 1e6)
    if best[2] is not None:
        alt.append((pred - best[2]) / 1e6)
    if k >= 30:
        ppms.append(ppm)
fails = 0
def line(good, text):
    global fails
    print('  %s %s' % ('ok  ' if good else 'FAIL', text))
    fails += not good
print('  dump: %d frames (%.0f s); %d clicks found, %d predictions, %d unmatched' % (n, n / RATE, len(clicks), len(preds), unmatched))
line(len(errs) >= 0.95 * len(preds) and len(preds) > 0, '6: clicks matched to predictions: %d of %d' % (len(errs), len(preds)))
print('       the device ran dry %d times (%d readings with nothing queued): lyrad late; a deadline lane should make this 0' % (dry_spells, len(dry)))
if errs:
    e = sorted(errs)
    print('       all clicks: predicted minus heard median %+.3f ms, min %+.3f, max %+.3f' % (e[len(e) // 2], e[0], e[-1]))
line(len(clean) >= 0.8 * max(len(errs), 1), '6: clicks with no dry spell in the 400 ms before: %d of %d (want 80%% to judge the clock)' % (len(clean), len(errs)))
if clean:
    e = sorted(clean)
    line(max(abs(x) for x in clean) <= 2.0, '6: predicted minus heard (device queue), those clicks: median %+.3f ms, min %+.3f, max %+.3f (want within ±2)' % (e[len(e) // 2], e[0], e[-1]))
# For diagnosis, not judged: a virtual sound card can change speed by
# percents for seconds (QEMU's HDA under host load; no crystal does). Clicks
# in 10 s windows where the device ran more than 1000 ppm off its run's
# median rate, shown apart.
win = {}
for t, q in zip(now, queued):
    win.setdefault(int((t - now[0]) // 10e9), []).append((t, q))
rates = {}
for k, pts in win.items():
    if len(pts) > 100:
        mt = sum(p[0] for p in pts) / len(pts); mq = sum(p[1] for p in pts) / len(pts)
        sxx = sum((p[0] - mt) ** 2 for p in pts)
        if sxx > 0:
            rates[k] = sum((p[0] - mt) * (p[1] - mq) for p in pts) / sxx * 1e9 / RATE * 1e6 - 1e6
if rates:
    med = sorted(rates.values())[len(rates) // 2]
    odd = {k for k, r in rates.items() if abs(r - med) > 1000}
    steady = []
    for rate, k, pred, ppm in preds:
        best = min(truth, key=lambda t: abs(t[1] - pred), default=None)
        if best is None or abs(best[1] - pred) > 200e6:
            continue
        w = int((best[1] - now[0]) // 10e9)
        if not ({w - 1, w} & odd) and not after_dry(best[1]):
            steady.append((pred - best[1]) / 1e6)
    print('       device rate: median %+.0f ppm; %d of %d 10 s windows off by more than 1000 ppm %s' % (med, len(odd), len(rates), sorted('%d s: %+.0f' % (k * 10, rates[k]) for k in odd)))
    if steady:
        e = sorted(steady)
        print('       clicks outside those windows (and the one before each): %d, median %+.3f ms, min %+.3f, max %+.3f' % (len(e), e[len(e) // 2], e[0], e[-1]))
if alt:
    a = sorted(alt)
    print('       (against GETOPTR instead: median %+.3f ms, min %+.3f, max %+.3f)' % (a[len(a) // 2], a[0], a[-1]))
# The test external device says when each click was heard: the truth
# itself, not lyrad's own log.
heard = [int(l.split()[1]) for l in open(out + '/dev.txt') if l.startswith('heard ')]
if heard:
    he = []
    for rate, k, pred, ppm in preds:
        h = min(heard, key=lambda x: abs(x - pred))
        if abs(h - pred) < 200e6:
            he.append((pred - h) / 1e6)
    e = sorted(he)
    line(len(he) >= 0.95 * len(preds) and max(abs(x) for x in he) <= 2.0,
         '6: predicted minus heard (the device\'s own times): %d of %d clicks, median %+.3f ms, min %+.3f, max %+.3f (want within ±2)' % (len(he), len(preds), e[len(e) // 2], e[0], e[-1]))
if ppms:
    line(max(ppms) - min(ppms) <= 20, '6: rate_ppm after 30 s: %d to %d (want a spread of 20 or less)' % (min(ppms), max(ppms)))
sys.exit(min(fails, 100))
PY
    echo "  data in $OUT"
    [ "$FAILS" = 0 ] && { say "lyra clock check PASSED"; exit 0; }
    say "lyra clock check: $FAILS FAILED"
    exit 1
fi

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

say "balance: the output's left/right"
M10=$(mark)
v "su -m appa -c 'env LYRA_PLAY_FREQ=500 $BIN/lyra-play $T/audio.sock media 12' >$T/a.log 2>&1 </dev/null &
   sleep 4"
M11=$(mark)
r=$(v "su -m appui -c '$BIN/lyra-vmtest ctl $T/control.sock balance 0.5'")
echo "$r" | sed 's/^/  ui: /'
v "sleep 4"
M12=$(mark)
v "while pgrep -q lyra-play; do sleep 1; done"
v "su -m appui -c '$BIN/lyra-vmtest ctl $T/control.sock balance 0'" >/dev/null

say "surround: headphone virtual surround"
v "su -m appui -c '$BIN/lyra-vmtest ctl $T/control.sock spatial 1'" | sed 's/^/  ui: /'
M13=$(mark)
v "su -m appa -c 'env LYRA_PLAY_FREQ=500 LYRA_PLAY_CHANNELS=8 LYRA_PLAY_ONLY=4 $BIN/lyra-play $T/audio.sock media 8' >$T/a.log 2>&1 </dev/null &
   su -m appa -c 'env LYRA_PLAY_FREQ=860 $BIN/lyra-play $T/audio.sock pro 8' >$T/p.log 2>&1 </dev/null &
   sleep 6"
M14=$(mark)
v "while pgrep -q lyra-play; do sleep 1; done"
v "su -m appui -c '$BIN/lyra-vmtest ctl $T/control.sock spatial 0'" >/dev/null

say "layout: the output set to 5.1"
# QEMU's codec is stereo: sound(4) either converts 6 channels for it or
# refuses them; either way the service must keep playing.
v "su -m appui -c '$BIN/lyra-vmtest ctl $T/control.sock layout 6'" | sed 's/^/  ui: /'
v "sleep 3"
r=$(v "grep -E 'layout|refused|OSS device open' $T/lyrad.log | tail -3")
echo "$r" | sed 's/^/  lyrad: /'
r=$(v "su -m appa -c '$BIN/lyra-play $T/audio.sock media 1' 2>&1; echo rc=\$?")
case "$r" in *rc=0*) ok "a stream plays after the layout change" ;; *) bad "after layout 6: $r" ;; esac
v "su -m appui -c '$BIN/lyra-vmtest ctl $T/control.sock layout 2'" >/dev/null
v "sleep 2"

say "7: restart"
v "su -m appa -c '$BIN/lyra-play $T/audio.sock media 30' >$T/r.log 2>&1 </dev/null &
   sleep 4; pkill -u _lyra lyrad; sleep 3"
r=$(v "cat $T/r.log")
case "$r" in *"stream lost"*) ok "the app is told its stream is gone" ;; *) bad "no stream-lost: $r" ;; esac
r=$(v "for i in \$(seq 1 30); do su -m appa -c '$BIN/lyra-play $T/audio.sock media 1' >/dev/null 2>&1 && { echo back after \$i; exit 0; }; sleep 1; done; echo never")
case "$r" in back*) ok "lyrad and choragusd restarted; a new stream plays ($r tries)" ;; *) bad "no restart: $(v "tail -5 $T/choragusd.log")" ;; esac

say "analyse the dump (host)"
# Stop first: lyrad writes the dump for as long as it runs.
v "pkill -f 'daemon.*lyra-test'; pkill -u _lyra lyrad; pkill -u _choragus choragusd; true" >/dev/null 2>&1
mkdir -p "$OUT"
# shellcheck disable=SC2086
scp $SCP_OPTS "root@localhost:$DUMP" "$OUT/mix.raw" >/dev/null 2>&1 || die "pull the dump"
v "cat $T/lyrad.log" > "$OUT/lyrad.log" 2>&1
v "cat $T/choragusd.log" > "$OUT/choragusd.log" 2>&1
python3 -I - "$OUT/mix.raw" "$M0" "$M1" "$M2" "$M3" "$M4" "$M5" "$M6" "$M7" "$M8" "$M9" "$M10" "$M11" "$M12" "$M13" "$M14" <<'PY' || FAILS=$((FAILS + $?))
import math, struct, sys
path = sys.argv[1]
m = [int(x) // 4 for x in sys.argv[2:]]   # byte offsets -> stereo frames
raw = open(path, 'rb').read()
n = len(raw) // 4
both = struct.unpack('<%dh' % (n * 2), raw[:n * 4])
left, right = both[0::2], both[1::2]
RATE, WIN = 48000, 4800
def amp(a, b, f, ch=None):
    """Median Goertzel amplitude (dBFS) of f over 100 ms windows in frames [a, b)."""
    ch = left if ch is None else ch
    vals = []
    k = 2 * math.cos(2 * math.pi * f / RATE)
    for s in range(a, b - WIN, WIN):
        s1 = s2 = 0.0
        for x in ch[s:s + WIN]:
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
l0, r0 = amp(m[10] + S, m[11], 500, left), amp(m[10] + S, m[11], 500, right)
l1, r1 = amp(m[11] + S, m[12], 500, left), amp(m[11] + S, m[12], 500, right)
check('balance 0.5: left fades', l1 - l0, -6.02, 1.0)
check('balance 0.5: right stays', r1 - r0, 0.0, 1.0)
# Left surround at -110 deg through the bundled FABIAN HRTFs: at 500 Hz the
# left ear's response is -1.0 dB, the right's -5.6 dB; the tone is -12 dBFS.
sl, sr = amp(m[13] + S, m[14], 500, left), amp(m[13] + S, m[14], 500, right)
check('surround: left surround, left ear', sl, -12.04 - 1.0, 1.0)
check('surround: left surround, left over right ear', sl - sr, 4.52, 1.0)
pl, pr = amp(m[13] + S, m[14], 860, left), amp(m[13] + S, m[14], 860, right)
check('surround: Pro stream untouched (left)', pl, -12.04, 0.5)
check('surround: Pro stream untouched (right)', pr, -12.04, 0.5)
sys.exit(min(fails, 100))
PY

say "stop"
v "pkill -f 'daemon.*lyra-test'; pkill -u _lyra lyrad; pkill -u _choragus choragusd; true" >/dev/null 2>&1
echo "  dump and logs in $OUT"
[ "$FAILS" = 0 ] && { say "lyra VM check PASSED"; exit 0; }
say "lyra VM check: $FAILS FAILED"
exit 1
