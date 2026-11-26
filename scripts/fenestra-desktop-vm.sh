#!/bin/sh
# Fenestra R2 VM gate, first part (docs/spec/fenestra.md §3.2, §19 R2): a
# session from a real frescod in the cloud VM to fenestra-view --headless in
# the container, over QUIC on a forwarded UDP port. Without a WM on the host
# the session is per-app (each window on its own on the viewer); with
# FEN_WM=1 forum-wm runs and it is the whole desktop (§3.4).
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
gate_begin fenestra-desktop $(( $([ "${FEN_JAIL:-0}" = 1 ] && echo 18 || echo 14) + $([ "${FEN_TRANSPORT:-auto}" = tcp ] && echo 1 || echo 0) + $([ -n "${FEN_OUTPUT:-}" ] && echo 1 || echo 0) ))
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
"$VIEW" --headless --input "$W/input.txt" --ssh "$FEN_SSH" --out "$W/view" --run-secs "$RUN" --connect "127.0.0.1:$PORT" --transport "${FEN_TRANSPORT:-auto}" ${FEN_OUTPUT:+--output "$FEN_OUTPUT"} \
    --reconnect-after 70 --idle-after 120 >"$W/view.log" 2>&1 &
VP=$!

sleep 15
g "cp $G/screen.png $G/before.png"
# While the viewer is attached: the jail as the kernel has it.
[ "${FEN_JAIL:-0}" = 1 ] && RJ=$(g "ps -ax -o user= -o jid= -o comm= | awk '\$3 == \"fresco-renderer\"'")
[ "${FEN_JAIL:-0}" = 1 ] && FU=$(g "ps -ax -o user= -o ppid= -o comm= | awk '\$3 == \"fenestrad\" { print \$1 }' | sort | tr '\n' ' '")
[ "${FEN_JAIL:-0}" = 1 ] && JL=$(g "jls -j system-fenestra-60500 vnet 2>/dev/null; ps -J system-fenestra-60500 -o uid= -o comm= 2>/dev/null" | tr -s ' \n' ' ')
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
if [ "${FEN_WM:-0}" = 1 ]; then
    check "the viewer shows the whole desktop (a WM runs on the host)" "mode desktop yes" "$(grep '^mode ' "$V") $(grep -q '^desktop ' "$V" && echo yes || echo no)"
else
    check "the viewer shows each app on its own (no WM on the host)" "mode apps no" "$(grep '^mode ' "$V") $(grep -q '^desktop ' "$V" && echo yes || echo no)"
fi
check "no stream errors" 0 "$(awk '$1 == "errors" { print $2 }' "$V")"
check "nothing sent while idle" 0 "$(awk '$1 == "idle_bytes" { print $2 }' "$V")"
cmp=$(compare "$W/host.png")
check "every window equal to frescod's own screen" equal "$cmp"
if [ "${FEN_WM:-0}" = 1 ]; then
    check "the whole screen equal, the strip, focus ring and background included" equal "$(compare_all "$W/host.png")"
else
    check "each window drawn at its own size, as its own" yes "$(awk '$1 == "placed" { p[$2] = $5 "x" $6 } $1 == "window" { w[$2] = $3 } END { n = 0; for (i in p) { if (w[i] != p[i]) { print "no (" i ": " w[i] " vs " p[i] ")"; exit } n++ } print (n ? "yes" : "no (none placed)") }' "$V")"
fi
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
# FEN_TRANSPORT=tcp: the whole run over the TCP fallback (the VM booted
# with hostfwd=tcp:127.0.0.1:$PORT-:$PORT as well).
# FEN_OUTPUT=WxH[@SCALE]: the viewer asks for that remote output, and the
# headless host's screen takes it (fenestra.md §4.1).
[ -n "${FEN_OUTPUT:-}" ] && check "the host's screen took the viewer's output" "${FEN_OUTPUT%@*}" "$(python3 -I -c 'import sys; from PIL import Image; w, h = Image.open(sys.argv[1]).size; print(f"{w}x{h}")' "$W/host.png")"
[ "${FEN_TRANSPORT:-auto}" = tcp ] && check "the viewer ran over the TCP fallback" tcp "$(awk '$1 == "transport" { print $2 }' "$W/view/viewer.txt")"
grep -q "minted session 1 for root" "$W/fenestrad.log" && ok "the viewer minted over ssh, as the seat's user" || bad "no ssh mint in fenestrad's log"
if [ "${FEN_JAIL:-0}" = 1 ]; then
    grep -q "session 1 in jail 24300" "$W/fenestrad.log" && ok "fenestrad started the session in a jail" || bad "no jailed session in fenestrad's log"
    # The kernel's answer: the jail exists, has its own vnet, and the
    # session runs in it as the pool's uid.
    # fenestrad as _fenestra; its forked root side holds only jaild.
    check "fenestrad runs as _fenestra, beside its root side for jaild" "_fenestra root " "$FU"
    # And every renderer in a jail of its own, as _fresco-render.
    check "the renderers run jailed as _fresco-render" yes "$(echo "$RJ" | awk 'NF { n++; if ($1 != "_fresco-render" || $2 == 0) bad = 1 } END { print (n > 0 && !bad) ? "yes" : "no: " NR " lines" }')"
    case "$JL" in *new*60500\ atrium-fenestra-se*) ok "the session runs in its own vnet jail as uid 60500";; *) bad "the session jail: $JL";; esac
    # Its grace (fen_fenestrad) ends it with no viewer; fenestrad's janitor
    # then takes the jail and its network apart.
    i=0; while [ $i -lt 40 ] && g "jls -j system-fenestra-60500 jid >/dev/null 2>&1"; do sleep 1; i=$((i+1)); done
    check "after its grace the session's jail is gone" gone "$(g "jls -j system-fenestra-60500 jid >/dev/null 2>&1" && echo present || echo gone)"
else
    check "the session is still up after its viewer left" yes "$(g "pgrep -x fenestra-session >/dev/null" && echo yes || echo "no: $(tail -2 "$W/fenestrad.log")")"
fi
g "pkill -x fenestrad; pkill -x fenestra-session; pkill -x atrium-sheets; pkill -x frescod; true"
echo "  evidence in $W"
gate_end
