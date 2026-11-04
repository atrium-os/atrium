#!/bin/sh
# F3: fuzz frescod's broker through its real sockets (docs/spec/fresco-f4-split.md).
# Runs IN the FreeBSD guest, from a directory holding frescod, fresco-renderer,
# forum-wm, atrium-split-fuzz and atrium-rect-bouncer (amd64 VM: /root/s).
#
#   sh fresco-split-fuzz.sh <seconds> <seed> [persona] [chrome]
#
# persona: mixed (default) | staller | flooder. chrome: "chrome" adds the
# hostile Navigator on the chrome socket. Every renderer frescod starts is a
# hostile one (atrium-split-fuzz renderer), except the probe: an animated
# client on the real renderer, whose frames keep the capture PNG fresh while
# the frame loop runs. The verdicts: frescod alive, no panic, the frame loop
# never quiet for more than GAP_MAX seconds, and a well-behaved client served
# at the end.
SECS=${1:-60} SEED=${2:-1} PERSONA=${3:-mixed} GAP_MAX=${GAP_MAX:-15}
CHROME=-; [ "$4" = chrome ] && CHROME=/tmp/chrome.sock
pkill -x frescod forum-wm atrium-rect-bouncer 2>/dev/null; pkill -f split-fuzz; sleep 1
rm -f /tmp/fz.png /tmp/fz-*.sock /tmp/split-fuzz-benign
env RUST_LOG=info SPLIT_FUZZ_PERSONA=$PERSONA FRESCOD_HEADLESS_PNG=/tmp/fz FRESCOD_RENDERER=compute \
  FRESCOD_SPLIT=1 FRESCOD_CHROME_SOCK=/tmp/fz-chrome.sock FRESCOD_SOCK=/tmp/fz-fresco.sock FRESCOD_WM_SOCK=/tmp/fz-wm.sock \
  FRESCOD_RENDERER_LAUNCHER="$PWD/atrium-split-fuzz renderer {client} $PWD/fresco-renderer" \
  ./frescod >/tmp/fz-frescod.log 2>&1 &
P=$!
sleep 3
[ "$CHROME" != - ] && CHROME=/tmp/fz-chrome.sock
FRESCO_WM_SOCKET=/tmp/fz-wm.sock ./forum-wm >/tmp/fz-wm.log 2>&1 &
touch /tmp/split-fuzz-benign
./atrium-rect-bouncer /tmp/fz-fresco.sock >/dev/null 2>&1 &
sleep 3; rm -f /tmp/split-fuzz-benign
./atrium-split-fuzz drive /tmp/fz-fresco.sock $CHROME $SECS $SEED >/tmp/fz-drive.log 2>&1 &
D=$!
GAP=0
while kill -0 $D 2>/dev/null; do
  sleep 5
  AGE=$(( $(date +%s) - $(stat -f %m /tmp/fz.png 2>/dev/null || echo 0) )); [ $AGE -gt $GAP ] && GAP=$AGE
  echo "frame age ${AGE}s, frescod fds $(procstat -f $P 2>/dev/null | wc -l), rss $(ps -o rss= -p $P)K, renderers $(pgrep -f 'split-fuzz renderer' | wc -l)"
done
cat /tmp/fz-drive.log; pkill atrium-rect-bouncer
FAIL=0
grep -q PASS /tmp/fz-drive.log || FAIL=1
kill -0 $P 2>/dev/null || { echo "frescod DIED"; tail -20 /tmp/fz-frescod.log; FAIL=1; }
N=$(grep -ci panic /tmp/fz-frescod.log); [ "$N" -eq 0 ] || { echo "$N panics"; FAIL=1; }
[ $GAP -le $GAP_MAX ] || { echo "frame loop quiet for ${GAP}s"; FAIL=1; }
echo "longest frame gap ${GAP}s; retired for not reading: $(grep -c 'stopped reading' /tmp/fz-frescod.log)"
pkill -x frescod forum-wm; pkill -f split-fuzz
[ $FAIL -eq 0 ] && echo "fresco-split-fuzz: PASS" || { echo "fresco-split-fuzz: FAIL"; exit 1; }
