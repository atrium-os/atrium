#!/bin/sh
# Lock screen check (docs/spec/lock-screen.md §3), frescod's side. Runs on
# the HOST of a FreeBSD VM (scripts/cloud-vm.sh up, or the Mac's
# ~/atrium-vm). A headless frescod, direct and then split, with three
# atrium-input-probe windows:
#   - a WM (on the window-management socket, green, as root),
#   - the lock screen (blue, as _lkprobe, uid 65031, named a sheet uid),
#   - the session's app (red, as root, an owner frescod never lets through
#     a lock), on top.
# The seat files are frescod's FRESCOD_SEAT in /tmp/lk. Checked: before the
# lock the app is on screen and gets a key; with the lock file naming the
# seat's user the screen shows no red and no green, a key goes to the lock
# screen, and neither the app nor the WM hears it; after, the app is back
# on top and gets keys again.
#
#   sh scripts/vm-lock-check.sh FRESCOD FRESCO_RENDERER INPUT_PROBE
#
# (Release builds for the VM's target.) Ends "lock-check: N failed".
set -u
FD=$1 RD=$2 PROBE=$3
ROOT=$(cd "$(dirname "$0")/.." && pwd)
K="-i $HOME/.ssh/fresco_bsd_ed25519 -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o LogLevel=ERROR"
V() { "$ROOT/scripts/vssh" "$@"; }
fails=0
ok() { echo "  ok $1"; }
bad() { echo "  FAIL $1"; fails=$((fails+1)); }
T=$(mktemp -d)

# Share of pixels near (r,g,b) in a PNG (8-bit RGB/RGBA, not interlaced).
share() { python3 -I -c '
import sys, zlib, struct
d = open(sys.argv[1], "rb").read()
assert d[:8] == b"\x89PNG\r\n\x1a\n"
i, idat, w = 8, b"", 0
while i < len(d):
    n, t = struct.unpack(">I4s", d[i:i+8]); c = d[i+8:i+8+n]; i += 12 + n
    if t == b"IHDR": w, h, depth, ct = struct.unpack(">IIBB", c[:10])
    if t == b"IDAT": idat += c
bpp = {2: 3, 6: 4}[ct]; raw = zlib.decompress(idat); stride = w * bpp
want = [int(x) for x in sys.argv[2].split(",")]
prev = bytearray(stride); hits = total = 0; p = 0
for y in range(h):
    f = raw[p]; line = bytearray(raw[p+1:p+1+stride]); p += 1 + stride
    for x in range(stride):
        a = line[x-bpp] if x >= bpp else 0; b = prev[x]; c2 = prev[x-bpp] if x >= bpp else 0
        if f == 1: line[x] = (line[x] + a) & 255
        elif f == 2: line[x] = (line[x] + b) & 255
        elif f == 3: line[x] = (line[x] + (a + b) // 2) & 255
        elif f == 4:
            pa, pb, pc = abs(b - c2), abs(a - c2), abs(a + b - 2 * c2)
            line[x] = (line[x] + (a if pa <= pb and pa <= pc else b if pb <= pc else c2)) & 255
    prev = line
    for x in range(0, stride, bpp * 4):
        total += 1
        if all(abs(line[x+k] - want[k]) < 40 for k in range(3)): hits += 1
print("%.3f" % (hits / max(total, 1)))
' "$1" "$2"; }
num() { awk -v a="$1" -v b="$3" -v o="$2" 'BEGIN { exit !(o == ">" ? a > b : a < b) }'; }
shot() { sleep 1.5; V 'cp /tmp/lk/hl/f.png /tmp/lk/shot.png'; scp -q $K -P 2222 root@localhost:/tmp/lk/shot.png "$T/$1.png"; }
key() { V "printf 'KEY 0x04 1\nKEY 0x04 0\n' | nc -U -N /tmp/lk/inject.sock 2>/dev/null || printf 'KEY 0x04 1\nKEY 0x04 0\n' | nc -U /tmp/lk/inject.sock"; sleep 1; }
keys() { V "grep -c '^key .*usage=0x0004 pressed=1' /tmp/lk/$1.log 2>/dev/null || echo 0"; }

V 'pkill -x frescod; pkill -f atrium-input-probe; sleep 1; rm -rf /tmp/lk; mkdir -p /tmp/lk/hl; chmod 755 /tmp/lk
   id _lkprobe >/dev/null 2>&1 || pw useradd _lkprobe -u 65031 -d /nonexistent -s /usr/sbin/nologin -c "lock gate"'
for f in "$FD" "$RD" "$PROBE"; do scp -q $K -P 2222 "$f" root@localhost:/tmp/lk/; done
V 'chmod 755 /tmp/lk/*'

for mode in direct split; do
  echo "== $mode"
  SPLIT="FRESCOD_SPLIT=0"
  [ $mode = split ] && SPLIT="FRESCOD_SPLIT=1 FRESCOD_RENDERER=compute FRESCOD_RENDERER_LAUNCHER=unconfined FRESCOD_RENDERER_BIN=/tmp/lk/fresco-renderer"
  V "pkill -x frescod; pkill -f atrium-input-probe; sleep 1; rm -f /tmp/lk/*.log /tmp/lk/hl/*; cd /tmp/lk
     echo alice > active-session; : > session-locked
     daemon -f -o /tmp/lk/frescod.log env $SPLIT FRESCOD_HEADLESS_PNG=/tmp/lk/hl/f FRESCOD_APP_REGISTRY=/tmp/lk/none \
       FRESCOD_SEAT=/tmp/lk/active-session FRESCOD_SHEET_UIDS=65031 \
       FRESCOD_SOCK=/tmp/lk/fresco.sock FRESCOD_WM_SOCK=/tmp/lk/wm.sock FRESCOD_INPUT_SOCK=/tmp/lk/inject.sock ./frescod </dev/null
     for i in \$(seq 1 60); do [ -S fresco.sock ] && [ -S inject.sock ] && break; sleep 1; done
     chmod 666 fresco.sock
     daemon -f -o /tmp/lk/wm.log env PROBE_COLOR=0,1,0 ./atrium-input-probe /tmp/lk/wm.sock 120; sleep 1
     daemon -f -o /tmp/lk/lock.log -u _lkprobe env PROBE_COLOR=0,0,1 /tmp/lk/atrium-input-probe /tmp/lk/fresco.sock 120; sleep 1
     daemon -f -o /tmp/lk/app.log env PROBE_COLOR=1,0,0 ./atrium-input-probe /tmp/lk/fresco.sock 120; sleep 2"
  shot unlocked
  r=$(share "$T/unlocked.png" 255,0,0)
  num $r ">" 0.5 && ok "unlocked: the app is on screen (red $r)" || bad "unlocked: red $r"
  key
  [ "$(keys app)" -ge 1 ] && ok "unlocked: the app gets a key" || bad "unlocked: the app got no key"

  V 'echo alice > /tmp/lk/session-locked'
  shot locked
  r=$(share "$T/locked.png" 255,0,0); g=$(share "$T/locked.png" 0,255,0); b=$(share "$T/locked.png" 0,0,255)
  num $r "<" 0.001 && ok "locked: no pixel of the app (red $r)" || bad "locked: red $r on screen"
  num $g "<" 0.001 && ok "locked: no pixel of the WM (green $g)" || bad "locked: green $g on screen"
  num $b ">" 0.5 && ok "locked: the lock screen is shown (blue $b)" || bad "locked: blue $b"
  a0=$(keys app); w0=$(keys wm); l0=$(keys lock)
  key
  [ "$(keys app)" = "$a0" ] && ok "locked: the app hears no key" || bad "locked: a key reached the app"
  [ "$(keys wm)" = "$w0" ] && ok "locked: the WM hears no key" || bad "locked: a key reached the WM"
  [ "$(keys lock)" -gt "$l0" ] && ok "locked: the key goes to the lock screen" || bad "locked: the lock screen got no key"
  # A stale lock (another user's) is no lock.
  V 'echo bob > /tmp/lk/session-locked'
  shot stale
  r=$(share "$T/stale.png" 255,0,0)
  num $r ">" 0.5 && ok "another user's lock file locks nothing (red $r)" || bad "stale lock: red $r"
  V 'echo alice > /tmp/lk/session-locked'; sleep 1
  V ': > /tmp/lk/session-locked'
  shot after
  r=$(share "$T/after.png" 255,0,0)
  num $r ">" 0.5 && ok "unlocked again: the app is back on top (red $r)" || bad "after: red $r"
  a0=$(keys app); key
  [ "$(keys app)" -gt "$a0" ] && ok "unlocked again: the app gets keys" || bad "after: the app got no key"
  V 'grep "screen \(locked\|unlocked\)" /tmp/lk/frescod.log | tail -4 | sed "s/^/  frescod: /"'
  [ $fails = 0 ] || V 'tail -20 /tmp/lk/frescod.log'
done
V 'pkill -x frescod; pkill -f atrium-input-probe'
rm -rf "$T"
echo "lock-check: $fails failed"
