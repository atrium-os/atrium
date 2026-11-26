#!/bin/sh
# Fenestra R2 gate (docs/spec/fenestra.md §14, §19): the scene stream over
# QUIC on one machine. fenestra-host-sim plays each recorded session
# (scripts/fenestra-record-vm.sh) to fenestra-view --headless over
# loopback, through the pinned-key handshake and the session proof.
#
# For each recording, two runs:
#   1. One connection: every window the viewer draws has the host's SHA-256,
#      no stream error, and the host sends nothing while the screen is idle.
#   2. The viewer drops the connection after 8 s and reconnects: equal
#      again, every window the viewer held resumes as a difference, no error.
#
#   sh scripts/fenestra-loopback-check.sh [recordings-dir]
#
# The recordings default to ~/atrium-vm/fenestra-rec. Runs on Linux or macOS.
set -u
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
REC="${1:-$HOME/atrium-vm/fenestra-rec}"
APPS="${APPS:-pergola navigator sheets}"
. "$ROOT/scripts/lib/gate.sh"
gate_begin fenestra-loopback 18

(cd "$ROOT/fenestra-session" && cargo build -q --release --bin fenestra-host-sim 2>/dev/null) || gate_abort "fenestra-host-sim did not build"
(cd "$ROOT/fenestra-view" && cargo build -q --release 2>/dev/null) || gate_abort "fenestra-view did not build"
HOSTSIM="$ROOT/fenestra-session/target/release/fenestra-host-sim"
VIEW="$ROOT/fenestra-view/target/release/fenestra-view"
export ATRIUM_FONT_PATH="$ROOT/fonts/ibm-plex:$ROOT/fonts/phosphor"
WORK=$(mktemp -d "${TMPDIR:-/tmp}/fenestra-loopback.XXXXXX")

# session <name> <recording> <speed> [viewer args...]: one host, one viewer.
session() {
    d="$WORK/$1"; rec=$2; speed=$3; shift 3
    mkdir -p "$d/mint"
    "$HOSTSIM" --rec "$rec" --mint-dir "$d/mint" --out "$d/host" --speed "$speed" --idle 3 >"$d/host.log" 2>&1 &
    hp=$!
    "$VIEW" --headless --mint-dir "$d/mint" --out "$d/view" --run-secs 200 "$@" >"$d/view.log" 2>&1
    vr=$?
    wait "$hp"; hr=$?
    [ $vr -eq 0 ] && [ $hr -eq 0 ] && [ -s "$d/host/host.txt" ] && [ -s "$d/view/viewer.txt" ]
}
windows() { grep '^window ' "$1" | sort; }
field() { awk -v k="$2" '$1 == k { print $2 }' "$1"; }

for app in $APPS; do
    f="$REC/$app.frec"
    if [ ! -s "$f" ]; then
        for _ in 1 2 3 4 5 6; do bad "$app: no recording at $f (sh scripts/fenestra-record-vm.sh)"; done
        continue
    fi
    # 1. One connection, played fast.
    d="$WORK/$app-1"
    if session "$app-1" "$f" 4; then
        n=$(windows "$d/host/host.txt" | wc -l | tr -d ' ')
        if [ "$n" -gt 0 ] && [ "$(windows "$d/host/host.txt")" = "$(windows "$d/view/viewer.txt")" ]; then
            ok "$app: $n window(s) pixel-equal on the viewer ($(field "$d/view/viewer.txt" bytes) bytes)"
        else
            bad "$app: the viewer's windows differ from the host's"
        fi
        check "$app: no stream errors" 0 "$(field "$d/view/viewer.txt" errors)"
        check "$app: nothing sent while idle" 0 "$(field "$d/host/host.txt" idle_bytes)"
    else
        bad "$app: the session failed: $(tail -1 "$d/view.log") / $(tail -1 "$d/host.log")"
        bad "$app: no stream errors (no session)"
        bad "$app: nothing sent while idle (no session)"
    fi
    # 2. Reconnect mid-session, played in real time.
    d="$WORK/$app-2"
    if session "$app-2" "$f" 1 --reconnect-after 8; then
        if [ "$(windows "$d/host/host.txt")" = "$(windows "$d/view/viewer.txt")" ]; then
            ok "$app: pixel-equal after a reconnect"
        else
            bad "$app: the viewer's windows differ after a reconnect"
        fi
        # Every window the viewer held came back as a difference; a window
        # the app opened meanwhile is new to it, and comes whole.
        c1=$(awk '$1 == "connection" && $2 == 1 { print "resumed " $12 " resent " $14 " bytes " $8 }' "$d/host/host.txt")
        case "$c1" in
            "") bad "$app: the viewer did not reconnect" ;;
            "resumed 0 "*) bad "$app: the viewer resumed nothing ($c1)" ;;
            *" resent 0 "*) ok "$app: the second connection resumed ($c1)" ;;
            *) bad "$app: held windows were sent whole again ($c1)" ;;
        esac
        check "$app: no stream errors across the reconnect" 0 "$(field "$d/view/viewer.txt" errors)"
    else
        bad "$app: the reconnecting session failed: $(tail -1 "$d/view.log") / $(tail -1 "$d/host.log")"
        bad "$app: resume (no session)"
        bad "$app: no stream errors across the reconnect (no session)"
    fi
done
echo "  evidence in $WORK"
gate_end
