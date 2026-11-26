#!/bin/sh
# Fenestra R2 gate (docs/spec/fenestra.md §8.6): UDP blocked. The same
# session over TCP on the session's port, on one Linux machine as root (a
# network namespace of its own, with iptables dropping UDP), no VM.
# fenestra-host-sim plays the Sheets recording to fenestra-view --headless.
#
#   1. UDP dropped, --transport auto: the viewer gets no answer over UDP,
#      connects over TCP, draws every window pixel-equal with no stream
#      error, drops the connection at 8 s and resumes over TCP without
#      resending.
#   2. UDP open, auto: it stays on UDP (the fallback is not taken for
#      nothing).
#   3. --transport tcp with UDP open: pixel-equal.
#
#   sh scripts/fenestra-tcp-check.sh [recordings-dir]
set -u
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
REC="${1:-$HOME/atrium-vm/fenestra-rec}"
if [ "${FEN_NETNS:-0}" != 1 ]; then
    [ "$(id -u)" = 0 ] && command -v unshare >/dev/null && command -v iptables >/dev/null \
        || { echo "fenestra-tcp: needs Linux, root, unshare and iptables"; exit 1; }
    # Built outside the namespace, which has no network.
    (cd "$ROOT/fenestra-session" && cargo build -q --release --bin fenestra-host-sim) || exit 1
    (cd "$ROOT/fenestra-view" && cargo build -q --release) || exit 1
    exec env FEN_NETNS=1 unshare -n sh "$0" "$@"
fi
. "$ROOT/scripts/lib/gate.sh"
gate_begin fenestra-tcp 7
# The namespace's loopback, up (no ip(8) here): SIOCSIFFLAGS with UP|LOOPBACK|RUNNING.
python3 -I -c '
import fcntl, socket, struct
s = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
fcntl.ioctl(s, 0x8914, struct.pack("16sH14s", b"lo", 0x1 | 0x8 | 0x40, b""))' || gate_abort "could not bring up lo"

HOSTSIM="$ROOT/fenestra-session/target/release/fenestra-host-sim"
VIEW="$ROOT/fenestra-view/target/release/fenestra-view"
export ATRIUM_FONT_PATH="$ROOT/fonts/ibm-plex:$ROOT/fonts/phosphor"
f="$REC/sheets.frec"
[ -s "$f" ] || gate_abort "no recording at $f (sh scripts/fenestra-record-vm.sh)"
WORK=$(mktemp -d "${TMPDIR:-/tmp}/fenestra-tcp.XXXXXX")

session() {
    d="$WORK/$1"; shift
    mkdir -p "$d/mint"
    "$HOSTSIM" --rec "$f" --mint-dir "$d/mint" --out "$d/host" --speed 1 --idle 3 >"$d/host.log" 2>&1 &
    hp=$!
    "$VIEW" --headless --mint-dir "$d/mint" --out "$d/view" --run-secs 200 "$@" >"$d/view.log" 2>&1
    vr=$?
    wait "$hp"; hr=$?
    [ $vr -eq 0 ] && [ $hr -eq 0 ] && [ -s "$d/host/host.txt" ] && [ -s "$d/view/viewer.txt" ]
}
windows() { grep '^window ' "$1" | sort; }
field() { awk -v k="$2" '$1 == k { print $2 }' "$1"; }
equal() { [ -n "$(windows "$1/host/host.txt")" ] && [ "$(windows "$1/host/host.txt")" = "$(windows "$1/view/viewer.txt")" ]; }

# 1
iptables -A INPUT -p udp -j DROP || gate_abort "iptables"
if session blocked --reconnect-after 8; then
    d="$WORK/blocked"
    grep -q "no answer over UDP; trying TCP" "$d/view.log" && grep -q "over tcp" "$d/view.log" \
        && ok "UDP blocked: the viewer found no answer over UDP and connected over TCP" || bad "no TCP fallback in the viewer's log"
    equal "$d" && ok "over TCP: every window pixel-equal" || bad "over TCP: the windows differ"
    check "over TCP: no stream errors" 0 "$(field "$d/view/viewer.txt" errors)"
    check "the viewer reports the transport" tcp "$(field "$d/view/viewer.txt" transport)"
    c1=$(awk '$1 == "connection" && $2 == 1 { print "resumed " $12 " resent " $14 }' "$d/host/host.txt")
    case "$c1" in "resumed 0"*|"") bad "over TCP: no resume ($c1)";; *" resent 0") ok "over TCP: the reconnect resumed without resending ($c1)";; *) bad "over TCP: resent ($c1)";; esac
else
    for _ in 1 2 3 4 5; do bad "UDP blocked: the session failed: $(tail -1 "$WORK/blocked/view.log")"; done
fi
iptables -D INPUT -p udp -j DROP

# 2
if session open; then check "UDP open: auto stays on UDP" udp "$(field "$WORK/open/view/viewer.txt" transport)"; else bad "UDP open: the session failed"; fi
# 3
if session forced --transport tcp && equal "$WORK/forced"; then ok "--transport tcp: pixel-equal"; else bad "--transport tcp: $(tail -1 "$WORK/forced/view.log")"; fi

echo "  evidence in $WORK"
gate_end
