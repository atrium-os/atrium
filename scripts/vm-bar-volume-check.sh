#!/bin/sh
# The Forum bar's volume drop-down (lyra-v1.md §7) against real lyrad and
# choragusd in the amd64 VM: open it from the speaker, mute, drag the level,
# and read back what choragusd stored. Headless frescod (direct mode, the
# input injector), forum-wm, forum-bar as appui (org.test.ui, granted
# audio-control by the Lyra check's grants).
#
#   QEMU_EXTRA="$(sh scripts/lyra-vm-check.sh qemu-args)" sh scripts/cloud-vm.sh up
#   LYRA_RELEASE=1 sh scripts/cloud-vm.sh lyra     # stages lyrad, choragusd, users
#   sh scripts/vm-bar-volume-check.sh               # x86_64-unknown-freebsd release
#                                                   # frescod, forum-wm, forum-bar
# Ends "bar-volume: N failed"; the last frame is OUT (default
# scratch/bar-volume.png).
set -u
ROOT=$(cd "$(dirname "$0")/.." && pwd); T=x86_64-unknown-freebsd
K="-i $HOME/.ssh/fresco_bsd_ed25519 -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o LogLevel=ERROR"
g() { ssh $K -o ConnectTimeout=30 -p 2222 root@localhost "$@"; }
put() { scp $K -P 2222 -q "$1" "root@localhost:$2"; }
D=/opt/st; L=/var/run/lyra-test
if [ "${SKIP_START:-0}" != 1 ]; then
g "pkill -x frescod; pkill -x forum-wm; pkill -x forum-bar; pkill -f 'daemon.*lyra-test'; pkill lyrad; pkill choragusd; sleep 1; rm -rf /tmp/st; mkdir -p $D /tmp/st; chmod 777 /tmp/st; true"
for b in frescod/target/$T/release/frescod forum-wm/target/$T/release/forum-wm forum-bar/target/$T/release/forum-bar; do put "$ROOT/$b" "$D/"; done
g "mkdir -p /usr/local/share/fonts/ibm-plex"
for f in "$ROOT"/fonts/ibm-plex/*.ttf "$ROOT"/fonts/phosphor/*.ttf; do
  g "test -f /usr/local/share/fonts/ibm-plex/$(basename "$f")" || put "$f" /usr/local/share/fonts/ibm-plex/
done
# A fresh choragusd state (full volume, not muted) each run.
g "kldload -n snd_hda; rm -f /var/db/choragus/*.toml; install -d -o _choragus -g _choragus -m 0755 /atrium/sockets/audio-control /atrium/sockets/media-control
   daemon -r -R 1 -u _lyra -o $L/lyrad.log -t lyra-test-lyrad /usr/local/bin/lyrad --serve </dev/null >/dev/null 2>&1
   daemon -r -R 1 -u _choragus -o $L/choragusd.log -t lyra-test-choragusd /usr/local/bin/choragusd --daemon $L/audio.sock /atrium/sockets/lyrad/lyrad.sock \
     --grants $L/grants --app-registry $L/registry --control /atrium/sockets/audio-control/audio-control.sock --device $L/device.sock --state /var/db/choragus </dev/null >/dev/null 2>&1
   for i in \$(seq 1 30); do [ -S /atrium/sockets/audio-control/audio-control.sock ] && break; sleep 1; done; ls -la /atrium/sockets/audio-control"
E2="FRESCO_SOCKET=/tmp/st/fresco.sock FRESCO_WM_SOCKET=/tmp/st/wm.sock FORUM_CTL_SOCKET=/tmp/st/ctl.sock"
g "cd /tmp/st && env FRESCOD_SPLIT=0 FRESCOD_HEADLESS_PNG=/tmp/st/screen FRESCOD_SOCK=/tmp/st/fresco.sock FRESCOD_CHROME_UIDS=50004 \
    FRESCOD_WM_SOCK=/tmp/st/wm.sock FRESCOD_INPUT_SOCK=/tmp/st/inject.sock daemon -f -o /tmp/st/frescod.log $D/frescod </dev/null"
i=0; while [ $i -lt 60 ] && ! g "test -S /tmp/st/fresco.sock"; do sleep 2; i=$((i+1)); done
g "cd /tmp/st && env $E2 daemon -f -o /tmp/st/wm.log $D/forum-wm </dev/null; sleep 4; chmod 666 /tmp/st/*.sock; ls -la /tmp/st"
g "cd /tmp/st && env $E2 daemon -f -u appui -o /tmp/st/bar.log $D/forum-bar </dev/null"
sleep ${SETTLE:-40}
fi
# STEPS: "click:x,y" or "drag:x1,y1,x2,y2". By default: the speaker, then
# Mute, then the level from full to under half.
fails=0
STEPS=${STEPS:-click:1032,19 click:812,53 drag:1025,88,905,88}
for s in $STEPS; do
  case $s in
    click:*) c=${s#click:}; g "printf 'MOVE ${c%,*} ${c#*,}\\n' | nc -N -U /tmp/st/inject.sock; sleep 2; printf 'BTN 1 1\\n' | nc -N -U /tmp/st/inject.sock; sleep 1; printf 'BTN 1 0\\n' | nc -N -U /tmp/st/inject.sock" ;;
    drag:*) c=${s#drag:}; IFS=, read x1 y1 x2 y2 <<EOF2
$c
EOF2
      g "printf 'MOVE $x1 $y1\\n' | nc -N -U /tmp/st/inject.sock; sleep 2; printf 'BTN 1 1\\n' | nc -N -U /tmp/st/inject.sock; sleep 2; printf 'MOVE $(( (x1+x2)/2 )) $y2\\n' | nc -N -U /tmp/st/inject.sock; sleep 2; printf 'MOVE $x2 $y2\\n' | nc -N -U /tmp/st/inject.sock; sleep 2; printf 'BTN 1 0\\n' | nc -N -U /tmp/st/inject.sock" ;;
  esac
  sleep ${GAP:-15}
done
g "cp /tmp/st/screen.png /tmp/st/shot.png"
OUT=${OUT:-$ROOT/scratch/bar-volume.png}; mkdir -p "$(dirname "$OUT")"
scp $K -P 2222 -q root@localhost:/tmp/st/shot.png "$OUT"
state=$(g "cat /var/db/choragus/alice.toml")
master=$(echo "$state" | sed -n 's/^master = //p'); muted=$(echo "$state" | sed -n 's/^master_muted = //p')
ok() { echo "  ok $1"; }; bad() { echo "  FAIL $1"; fails=$((fails+1)); }
[ "$muted" = true ] && ok "Mute reached choragusd" || bad "master_muted = $muted"
awk -v m="$master" 'BEGIN { exit !(m > 0.3 && m < 0.6) }' && ok "the level reached choragusd ($master)" || bad "master = $master"
echo "== frame: $OUT"
echo "bar-volume: $fails failed"
