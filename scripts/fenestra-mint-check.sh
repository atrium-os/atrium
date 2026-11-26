#!/bin/sh
# Fenestra R2 gate (docs/spec/fenestra.md §11.2): the mint, on one machine
# (Linux or macOS, no VM). fenestrad takes mints on its socket from
# fenestra-shell, as sshd would run it, with a stand-in for frescod's link;
# fenestra-view mints through a transport command (here `env`, in place of
# `ssh -T host`) and connects to the session it was given.
#
#   1. A viewer mints and connects.
#   2. A second mint while it holds the desktop is refused...
#   3. ...unless it takes over: the first session ends, the new one serves.
#   4. A caller who is not the seat's user is refused, and so is an
#      unreadable seat file.
#   5. Requests that are not one mint line are refused.
#   6. A session with no viewer ends after its grace.
#
#   sh scripts/fenestra-mint-check.sh
set -u
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
. "$ROOT/scripts/lib/gate.sh"
gate_begin fenestra-mint 10

for c in fenestrad fenestra-session fenestra-view; do
    (cd "$ROOT/$c" && cargo build -q --release 2>/dev/null) || gate_abort "$c did not build"
done
B="$ROOT/fenestrad/target/release"
SESSION="$ROOT/fenestra-session/target/release/fenestra-session"
VIEW="$ROOT/fenestra-view/target/release/fenestra-view"
W=$(mktemp -d "${TMPDIR:-/tmp}/fenestra-mint.XXXXXX")
chmod 700 "$W"
pids=""
trap 'kill $pids 2>/dev/null' EXIT

# frescod's side of the link: takes the records and answers nothing.
python3 -I -c '
import socket, sys, os
s = socket.socket(socket.AF_UNIX); s.bind(sys.argv[1]); s.listen(1)
c, _ = s.accept()
while c.recv(65536): pass
' "$W/frescod.sock" &
pids="$pids $!"
i=0; while [ $i -lt 50 ] && [ ! -S "$W/frescod.sock" ]; do sleep 0.1; i=$((i+1)); done

id -un >"$W/seat"
"$B/fenestrad" --frescod "$W/frescod.sock" --session "$SESSION" --mint-sock "$W/mint.sock" \
    --seat "$W/seat" --bind 127.0.0.1:0 --grace 4 >"$W/fenestrad.log" 2>&1 &
pids="$pids $!"
i=0; while [ $i -lt 50 ] && [ ! -S "$W/mint.sock" ]; do sleep 0.1; i=$((i+1)); done
[ -S "$W/mint.sock" ] || { cat "$W/fenestrad.log"; gate_abort "fenestrad did not open its mint socket"; }

T="env PATH=$B:$PATH FENESTRA_MINT_SOCK=$W/mint.sock"
KEY=$(printf '%064x' 1)
shell() { env PATH="$B:$PATH" FENESTRA_MINT_SOCK="$W/mint.sock" fenestra-shell "$@" 2>&1; }

# 1, 2, 3
"$VIEW" --headless --ssh "$T" --out "$W/v1" --run-secs 8 >"$W/v1.log" 2>&1 &
v1=$!; pids="$pids $v1"
sleep 3
out=$(shell mint "$KEY")
case "$out" in "FENESTRA_REFUSED a viewer already has"*) ok "a second mint is refused while a viewer holds the desktop";; *) bad "a second mint: $out";; esac
"$VIEW" --headless --ssh "$T" --take-over --out "$W/v2" --run-secs 4 >"$W/v2.log" 2>&1
check "a take-over viewer connects" 1 "$(awk '$1 == "connections" { print $2 }' "$W/v2/viewer.txt" 2>/dev/null)"
grep -q "taken over" "$W/fenestrad.log" && ok "the take-over ended the first session" || bad "no take-over in fenestrad's log"
wait $v1
grep -q "connected to 127.0.0.1" "$W/v1.log" && ok "the first viewer minted over the transport and connected" || { tail -5 "$W/v1.log"; bad "the first viewer did not connect"; }

# 6: the take-over session's viewer left at 4 s; its grace is 4 s.
sleep 6
n=$(grep -c "no viewer for 4s" "$W/fenestrad.log")
[ "$n" -ge 1 ] && ok "a session with no viewer ends after its grace" || bad "no session ended on its grace"
out=$(shell mint "$KEY")
case "$out" in FENESTRA_MINT\ addr\ 127.0.0.1:*) ok "after it ended, a new mint is given";; *) bad "a mint after the grace: $out";; esac

# 4
echo "nobody-$$" >"$W/seat"
out=$(shell mint "$KEY")
case "$out" in "FENESTRA_REFUSED not the user of the seat session"*) ok "a caller who is not the seat's user is refused";; *) bad "another seat user: $out";; esac
rm -f "$W/seat"
out=$(shell mint "$KEY")
case "$out" in "FENESTRA_REFUSED not the user"*) ok "no seat file: refused";; *) bad "no seat file: $out";; esac
id -un >"$W/seat"

# 5
ask() { python3 -I -c '
import socket, sys
s = socket.socket(socket.AF_UNIX); s.connect(sys.argv[1]); s.sendall(sys.argv[2].encode()); s.shutdown(socket.SHUT_WR)
print(s.recv(4096).decode().strip())' "$W/mint.sock" "$1"; }
r1=$(ask "MINT zz
"); r2=$(ask "MINT $(printf 'a%.0s' $(seq 1 600))
"); r3=$(ask "HELLO
")
case "$r1|$r2|$r3" in "FENESTRA_REFUSED a viewer key that is not hex|FENESTRA_REFUSED a request that is not one short line|FENESTRA_REFUSED not a mint request") ok "malformed requests are refused";; *) bad "malformed: $r1 | $r2 | $r3";; esac
kill -0 "$(echo $pids | awk '{print $2}')" 2>/dev/null && ok "fenestrad is still up after all of them" || bad "fenestrad died"

echo "  evidence in $W"
gate_end
