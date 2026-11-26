#!/bin/sh
# Fenestra VM gate for a host with monitors of its own (docs/spec/fenestra.md
# §4.1, §4.2, §11.4, decision 3). frescod is headless here, with
# FRESCOD_HEADLESS_SEAT=1: its outputs stand for the seat's monitors, and
# what they show is saved as screen-seat.png. forum-wm runs (a whole
# desktop); the viewer asks for a 1600x900 output at 1.5.
#
# Checks: while viewed, the desktop takes the viewer's output and the
# viewer's desktop equals it after typing sent from the viewer; the seat's
# monitors show frescod's viewed screen (who, and a Disconnect), nothing
# of the session; typing at the seat reaches nothing; a click on
# Disconnect at the seat ends the session; frescod asks ostiarius (a stub
# here) to lock the seat and shows the desktop on the seat again only once
# the lock file says it is locked, back at the seat's size, behind the lock.
#
#   NETDEV_EXTRA=hostfwd=udp:127.0.0.1:24300-:24300 sh scripts/cloud-vm.sh up
#   sh scripts/fenestra-seat-vm.sh
set -u
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
. "$ROOT/scripts/lib/gate.sh"
gate_begin fenestra-seat 13
FEN_WM=1
. "$ROOT/scripts/lib/fenestra-vm.sh"
G=/tmp/fendesk
FEN_FRESCOD_ENV="FRESCOD_HEADLESS_SEAT=1 FRESCOD_SEAT=$G/seat FRESCOD_LOCK_SOCK=$G/lock.sock FRESCOD_INPUT_SOCK=$G/input.sock"
fen_setup

W=$(mktemp -d "${TMPDIR:-/tmp}/fenestra-seat.XXXXXX")
# ostiarius's lock socket, stubbed: it answers ok once and then writes the
# lock file naming the seat's user, as ostiarius would.
STUB=$(mktemp)
cat >"$STUB" <<STUBEOF
printf '{"status":"ok","active":"root"}\n' | nc -N -lU $G/lock.sock >$G/lock-req.txt
echo root >$G/session-locked
STUBEOF
put "$STUB" "$G/lockstub.sh"; rm -f "$STUB"
g "rm -f $G/lock.sock $G/session-locked $G/lock-req.txt; daemon -f sh $G/lockstub.sh"
# The viewer clicks cell C3 (at 1600x900) and types "123" Enter at 20 s.
cat >"$W/input.txt" <<'EOF'
18 MOVE 300 320
18.2 BTN 1 1
18.3 BTN 1 0
20 KEY 30 1
20.1 KEY 30 0
21 KEY 31 1
21.1 KEY 31 0
22 KEY 32 1
22.1 KEY 32 0
23 KEY 40 1
23.1 KEY 40 0
EOF
fen_fenestrad
"$VIEW" --headless --input "$W/input.txt" --ssh "$FEN_SSH" --out "$W/view" --run-secs 75 --connect "127.0.0.1:$PORT" --output 1600x900@1.5 >"$W/view.log" 2>&1 &
VP=$!
sleep 12
g "cp $G/screen.png $G/before.png"
sleep 26
g "cp $G/screen.png $G/viewed.png; cp $G/screen-seat.png $G/seat-viewed.png"
get "$G/seat-viewed.png" "$W/seat-viewed.png"
# Typing at the seat through frescod's own input path: "9a" (not Return,
# which is the viewed screen's Disconnect).
g "printf 'KEY 38 1\nKEY 38 0\nKEY 4 1\nKEY 4 0\n' | nc -N -U $G/input.sock"
sleep 4
g "cp $G/screen.png $G/after-seat-keys.png"
# Disconnect at the seat: a click in the middle of its button (frescod's
# accent colour on the seat's screen).
B=$(python3 -I - "$W/seat-viewed.png" <<'EOF'
import sys
from PIL import Image
im = Image.open(sys.argv[1]).convert("RGB")
w, h = im.size
px = im.load()
pts = [(x, y) for y in range(0, h, 2) for x in range(0, w, 2) if px[x, y] == (38, 140, 255)]
if pts:
    xs, ys = [p[0] for p in pts], [p[1] for p in pts]
    print((min(xs) + max(xs)) // 2, (min(ys) + max(ys)) // 2)
EOF
)
[ -n "$B" ] && g "printf 'MOVE $B\nBTN 1 1\nBTN 1 0\n' | nc -N -U $G/input.sock"
sleep 8
g "cp $G/screen.png $G/locked.png; cp $G/screen-seat.png $G/seat-locked.png"
wait $VP
for f in before viewed after-seat-keys locked seat-locked; do get "$G/$f.png" "$W/$f.png"; done
g "cat $G/fenestrad.log" >"$W/fenestrad.log" 2>&1
g "grep -E 'viewed|inject|headless|screen (un)?locked' $G/frescod.log" >"$W/frescod.log" 2>&1
REQ=$(g "cat $G/lock-req.txt; cat $G/session-locked 2>/dev/null")
cat "$W/view.log" | tail -5
V="$W/view/viewer.txt"
cat "$V"
cat "$W/frescod.log"

size() { python3 -I -c 'import sys; from PIL import Image; w, h = Image.open(sys.argv[1]).size; print(f"{w}x{h}")' "$1"; }
check "the viewer shows the whole desktop" "mode desktop" "$(grep '^mode ' "$V")"
check "while viewed the desktop took the viewer's output" 1600x900 "$(size "$W/viewed.png")"
check "the viewer's desktop equals the host's" equal "$(compare_all "$W/viewed.png")"
check "typing from the viewer reached the host" yes "$(cmp -s "$W/viewed.png" "$W/before.png" && echo no || echo yes)"
check "no stream errors" 0 "$(awk '$1 == "errors" { print $2 }' "$V")"
# The seat's monitors while viewed: the seat's size, frescod's plain field
# nearly everywhere, its words, its button: nothing of the session.
check "the seat shows the viewed screen, not the session" "1280x720 yes" "$(size "$W/seat-viewed.png") $(python3 -I - "$W/seat-viewed.png" <<'EOF'
import sys
from PIL import Image
im = Image.open(sys.argv[1]).convert("RGB")
d = im.getdata(); n = len(d)
field = sum(1 for p in d if p == (15, 18, 23)) / n
text = sum(1 for p in d if min(p) > 200)
button = sum(1 for p in d if p == (38, 140, 255))
print("yes" if field > 0.9 and text > 300 and button > 500 else f"no (field {field:.3f}, text {text}, button {button})")
EOF
)"
check "the seat switched to it when the session began" yes "$(grep -q "viewed: the seat's monitors show the viewed screen" "$W/frescod.log" && echo yes || echo no)"
check "typing at the seat reached nothing on the desktop" "yes yes" "$(cmp -s "$W/after-seat-keys.png" "$W/viewed.png" && echo yes || echo no) $(grep -q 'inject KEY 38 1 -> the viewed screen' "$W/frescod.log" && echo yes || echo no)"
check "Disconnect at the seat ended the session" "yes gone" "$([ -n "$B" ] && grep -q 'disconnected at the host' "$W/fenestrad.log" && echo yes || echo "no (button at '$B')") $(g "pgrep -x fenestra-session >/dev/null" && echo running || echo gone)"
check "frescod asked ostiarius to lock the seat" '{"op":"lock"} root' "$(echo $REQ)"
check "the seat showed the desktop again only once locked" yes "$(awk '/session ended; locking/ { e = NR } /the seat is locked/ { l = NR } /screen locked/ { k = NR } END { print (e && k && l && e < l) ? "yes" : "no (" e " " k " " l ")" }' "$W/frescod.log")"
check "then the desktop is back at the seat's size, on the seat" "1280x720 1280x720 same" "$(size "$W/locked.png") $(size "$W/seat-locked.png") $(cmp -s "$W/locked.png" "$W/seat-locked.png" && echo same || echo differ)"
check "behind the lock: nothing of the session on screen" yes "$(python3 -I - "$W/locked.png" <<'EOF'
import sys
from collections import Counter
from PIL import Image
# Below frescod's strip (its own chrome, drawn over the lock too).
im = Image.open(sys.argv[1]).convert("RGB")
d = list(im.crop((0, 40, im.size[0], im.size[1])).getdata())
top = Counter(d).most_common(1)[0][1] / len(d)
print("yes" if top > 0.99 else f"no ({top:.3f} one colour)")
EOF
)"
g "pkill -x fenestrad; pkill -x fenestra-session; pkill -x atrium-sheets; pkill -x forum-wm; pkill -x frescod; pkill -f 'nc -N -lU'; true"
echo "  evidence in $W"
gate_end
