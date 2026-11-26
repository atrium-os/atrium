#!/bin/sh
# Fenestra R2 VM gate, first part (docs/spec/fenestra.md §3.2, §19 R2): a
# whole-desktop session from a real frescod in the cloud VM to fenestra-view
# --headless in the container, over QUIC on a forwarded UDP port.
#
# In the guest: frescod in split mode (each client in its own renderer
# process, here unconfined) with the fenestra link on, an app (Officina's
# atrium-sheets on a small sheet), fenestrad (no network) and its session. The
# renderers write the scene streams; the broker relays them; fenestra-session
# sends them on without parsing them. The viewer draws each window and
# composes the desktop from the host's layout.
#
# Checks: the viewer gets windows and a layout; every window's interior on
# the viewer's desktop is pixel-equal to frescod's own screen, after a click
# and typing sent from the viewer changed it, and again after more typing
# sent once the viewer had reconnected; an idle screen
# sends no scene bytes; a reconnect resumes (the session's log: nothing
# resent whole); no stream errors; the session outlives its viewer.
#
#   sh scripts/cloud-vm.sh up      (with NETDEV_EXTRA=hostfwd=udp:127.0.0.1:24300-:24300)
#   sh scripts/fenestra-desktop-vm.sh
set -u
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
. "$ROOT/scripts/lib/gate.sh"
gate_begin fenestra-desktop 13
. "$ROOT/scripts/lib/fenestra-vm.sh"
fen_setup

echo "=== session"
W=$(mktemp -d "${TMPDIR:-/tmp}/fenestra-desktop.XXXXXX")
RUN="${RUN_SECS:-150}"
# The viewer's input, seconds from its start (it connects within ~10 s): a
# click on a cell and "123" Enter; it reconnects at 70 s; then right arrow
# and "45" Enter; nothing from 120 s.
cat >"$W/input.txt" <<'EOF'
45 MOVE 300 200
45.2 BTN 1 1
45.3 BTN 1 0
48 KEY 30 1
48.1 KEY 30 0
49 KEY 31 1
49.1 KEY 31 0
50 KEY 32 1
50.1 KEY 32 0
51 KEY 40 1
51.1 KEY 40 0
85 KEY 79 1
85.1 KEY 79 0
86 KEY 33 1
86.1 KEY 33 0
87 KEY 34 1
87.1 KEY 34 0
88 KEY 40 1
88.1 KEY 40 0
EOF
fen_fenestrad
"$VIEW" --headless --input "$W/input.txt" --ssh "$FEN_SSH" --out "$W/view" --run-secs "$RUN" --connect "127.0.0.1:$PORT" \
    --reconnect-after 70 --idle-after 120 >"$W/view.log" 2>&1 &
VP=$!

sleep 15
g "cp $G/screen.png $G/before.png"
sleep 50
g "cp $G/screen.png $G/middle.png"
sleep 45
g "cp $G/screen.png $G/after.png"
wait $VP
get "$G/after.png" "$W/host.png"
get "$G/before.png" "$W/host-before.png"
get "$G/middle.png" "$W/host-middle.png"
g "cat $G/fenestrad.log" >"$W/fenestrad.log" 2>&1
g "tail -50 $G/frescod.log" >"$W/frescod.log" 2>&1
cat "$W/view.log"
V="$W/view/viewer.txt"
cat "$V"

nw=$(grep -c '^window ' "$V")
check "the viewer drew windows" yes "$([ "$nw" -gt 0 ] && echo yes || echo "no ($nw)")"
check "the viewer has the host's layout" yes "$(grep -q '^desktop ' "$V" && echo yes || echo no)"
check "no stream errors" 0 "$(awk '$1 == "errors" { print $2 }' "$V")"
check "nothing sent while idle" 0 "$(awk '$1 == "idle_bytes" { print $2 }' "$V")"
cmp=$(compare "$W/host.png")
check "every window equal to frescod's own screen" equal "$cmp"
# The positive control: the screen before the last typing is not what the
# viewer ended with, so the comparison sees a typed cell.
check "the typing reached the viewer (its desktop differs from the screen before it)" yes "$(compare "$W/host-middle.png" | grep -q differ && echo yes || echo no)"
check "the viewer's input changed the host's screen" yes "$(cmp -s "$W/host-middle.png" "$W/host-before.png" && echo no || echo yes)"
check "input sent after the reconnect changed it again" yes "$(cmp -s "$W/host.png" "$W/host-middle.png" && echo no || echo yes)"
ins=$(grep 'viewer [0-9] gone' "$W/fenestrad.log" | sed 's/.*inputs \([0-9]*\) dropped \([0-9]*\).*/\1:\2/' | tr '\n' ' ')
check "the session passed on every input event, on both connections, none dropped" "11:0 8:0 " "$ins"
grep 'fenestra-session' "$W/fenestrad.log" | tail -5
check "the viewer reconnected" yes "$(grep -c 'connected to' "$W/view.log" | awk '{ print ($1 >= 2) ? "yes" : "no (" $1 ")" }')"
r2=$(grep 'viewer 2 gone' "$W/fenestrad.log" | sed 's/.*resumed \([0-9]*\) resent \([0-9]*\).*/\1 \2/')
case "$r2" in
    "") bad "the second connection resumed: no record of it in the session's log" ;;
    "0 "*) bad "the second connection resumed nothing ($r2)" ;;
    *" 0") ok "the second connection resumed every held window (resumed/resent: $r2)" ;;
    *) bad "held windows were sent whole again (resumed/resent: $r2)" ;;
esac
grep -q "minted session 1 for root" "$W/fenestrad.log" && ok "the viewer minted over ssh, as the seat's user" || bad "no ssh mint in fenestrad's log"
check "the session is still up after its viewer left" yes "$(g "pgrep -x fenestra-session >/dev/null" && echo yes || echo "no: $(tail -2 "$W/fenestrad.log")")"
g "pkill -x fenestrad; pkill -x fenestra-session; pkill -x atrium-sheets; pkill -x frescod; true"
echo "  evidence in $W"
gate_end
