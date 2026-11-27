#!/bin/sh
# Fenestra VM gate for a live seat (docs/spec/fenestra.md §11.4, decision 3
# opt-out): frescod headless with FRESCOD_HEADLESS_SEAT=1 and
# FRESCOD_FENESTRA_LIVE=1, so the seat keeps showing the session while it
# is viewed. forum-wm runs (a whole desktop).
#
# Checks: while viewed the seat shows the desktop with frescod's indicator
# on its strip (the viewed colour, the viewer's address, a Disconnect);
# the seat's typing still reaches the desktop; a click on the indicator's
# Disconnect ends the session; then the indicator is gone.
#
#   NETDEV_EXTRA=hostfwd=udp:127.0.0.1:24300-:24300 sh scripts/cloud-vm.sh up
#   sh scripts/fenestra-live-vm.sh
set -u
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
. "$ROOT/scripts/lib/gate.sh"
gate_begin fenestra-live 7
FEN_WM=1
. "$ROOT/scripts/lib/fenestra-vm.sh"
G=/tmp/fendesk
FEN_FRESCOD_ENV="FRESCOD_HEADLESS_SEAT=1 FRESCOD_FENESTRA_LIVE=1 FRESCOD_SEAT=$G/seat FRESCOD_INPUT_SOCK=$G/input.sock"
fen_setup

W=$(mktemp -d "${TMPDIR:-/tmp}/fenestra-live.XXXXXX")
# The viewer clicks cell C3 at 18 s, so the seat's keys below have a cell.
cat >"$W/input.txt" <<'EOF2'
18 MOVE 300 320
18.2 BTN 1 1
18.3 BTN 1 0
EOF2
fen_fenestrad
"$VIEW" --headless --input "$W/input.txt" --ssh "$FEN_SSH" --out "$W/view" --run-secs 70 --connect "127.0.0.1:$PORT" --output 1600x900@1.5 >"$W/view.log" 2>&1 &
VP=$!
sleep 30
g "cp $G/screen-seat.png $G/seat-viewed.png; cp $G/screen.png $G/before-keys.png"
get "$G/seat-viewed.png" "$W/seat-viewed.png"
# Typing at the seat: "9" Enter reaches the session on a live seat.
g "printf 'KEY 38 1\nKEY 38 0\nKEY 40 1\nKEY 40 0\n' | nc -N -U $G/input.sock"
sleep 5
g "cp $G/screen.png $G/after-keys.png"
# The indicator's Disconnect: the blue pill in the strip's right half (the
# composed colour, sRGB of the chrome's linear one).
B=$(python3 -I - "$W/seat-viewed.png" <<'EOF2'
import sys
from PIL import Image
im = Image.open(sys.argv[1]).convert("RGB")
w, h = im.size
px = im.load()
pts = [(x, y) for y in range(0, min(h, 22)) for x in range(w // 2, w) if px[x, y] == (108, 196, 255)]
if pts:
    xs, ys = [p[0] for p in pts], [p[1] for p in pts]
    print((min(xs) + max(xs)) // 2, (min(ys) + max(ys)) // 2)
EOF2
)
[ -n "$B" ] && g "printf 'MOVE $B\nBTN 1 1\nBTN 1 0\n' | nc -N -U $G/input.sock"
sleep 8
g "cp $G/screen-seat.png $G/seat-after.png"
wait $VP
for f in before-keys after-keys seat-after; do get "$G/$f.png" "$W/$f.png"; done
g "cat $G/fenestrad.log" >"$W/fenestrad.log" 2>&1
g "grep -E 'viewed|inject' $G/frescod.log" >"$W/frescod.log" 2>&1
tail -5 "$W/view.log"
cat "$W/frescod.log"

red() { python3 -I - "$1" <<'EOF2'
import sys
from PIL import Image
im = Image.open(sys.argv[1]).convert("RGB")
w, h = im.size
d = list(im.crop((0, 0, w, min(h, 22))).getdata())
print(sum(1 for p in d if abs(p[0] - 237) <= 2 and abs(p[1] - 108) <= 2 and abs(p[2] - 124) <= 2))
EOF2
}
RV=$(red "$W/seat-viewed.png"); RA=$(red "$W/seat-after.png")
check "the seat keeps the desktop, at its size" 1280x720 "$(python3 -I -c 'import sys; from PIL import Image; w, h = Image.open(sys.argv[1]).size; print(f"{w}x{h}")' "$W/seat-viewed.png")"
check "the indicator is on the seat's strip while viewed" yes "$([ "${RV:-0}" -gt 300 ] && echo yes || echo "no ($RV px)")"
check "it names where the viewer came from" yes "$(grep -q 'viewed: live seat, indicator on' "$W/frescod.log" && grep -qF 'viewed: viewers 1, latest from 10.0.2.2' "$W/frescod.log" && echo yes || echo no)"
check "typing at a live seat reaches the session" yes "$(cmp -s "$W/after-keys.png" "$W/before-keys.png" && echo no || echo yes)"
check "Disconnect on the indicator ended the session" "yes gone" "$([ -n "$B" ] && grep -q 'Disconnect pressed on the live indicator' "$W/frescod.log" && grep -q 'disconnected at the host' "$W/fenestrad.log" && echo yes || echo "no (button at '$B')") $(g "pgrep -x fenestra-session >/dev/null" && echo running || echo gone)"
check "then the indicator is gone" "yes 0" "$(grep -q 'indicator off' "$W/frescod.log" && echo yes || echo no) $RA"
check "no stream errors" 0 "$(awk '$1 == "errors" { print $2 }' "$W/view/viewer.txt")"
g "pkill -x fenestrad; pkill -x fenestra-session; pkill -x atrium-sheets; pkill -x forum-wm; pkill -x frescod; true"
echo "  evidence in $W"
gate_end
