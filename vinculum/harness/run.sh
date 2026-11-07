#!/bin/sh
# Vinculum B0 harness (docs/spec/bluetooth.md §14): a Rootcanal virtual
# controller, a Bumble peer advertising on it, and vinculum-probe as the host,
# all on this machine. No radio, no VM.
#
#   sh vinculum/harness/run.sh [--record FILE]
#
# Needs: python3 with `pip install rootcanal bumble` (a venv is made in
# $VINCULUM_VENV, default ~/.cache/vinculum-venv, on first run) and cargo.
# Rootcanal ships Linux x86_64 and macOS arm64 binaries in its wheel.
set -eu
here=$(cd "$(dirname "$0")" && pwd)
ws=$(dirname "$here")
venv=${VINCULUM_VENV:-$HOME/.cache/vinculum-venv}
port=${VINCULUM_HCI_PORT:-6402}
record=""
[ "${1:-}" = "--record" ] && record="--record $2"

if [ ! -x "$venv/bin/python" ]; then
    python3 -m venv "$venv"
    "$venv/bin/pip" install -q rootcanal bumble
fi

work=$(mktemp -d)
cleanup() { kill $rc_pid $peer_pid 2>/dev/null || true; rm -rf "$work"; }
trap cleanup EXIT INT TERM

"$venv/bin/python" -m rootcanal --hci_port "$port" --link_port $((port + 1)) \
    --link_ble_port $((port + 2)) --test_port $((port - 1)) >"$work/rootcanal.log" 2>&1 &
rc_pid=$!
i=0
until nc -z 127.0.0.1 "$port" 2>/dev/null; do
    i=$((i + 1)); [ $i -gt 50 ] && { cat "$work/rootcanal.log"; echo "rootcanal did not start"; exit 1; }
    sleep 0.1
done

"$venv/bin/python" "$here/peer_advertiser.py" "tcp-client:127.0.0.1:$port" VinculumPeer 30 \
    >"$work/peer.log" 2>&1 &
peer_pid=$!
i=0
until grep -q advertising "$work/peer.log" 2>/dev/null; do
    i=$((i + 1)); [ $i -gt 100 ] && { cat "$work/peer.log"; echo "peer did not start"; exit 1; }
    sleep 0.1
done

cargo run -q --manifest-path "$ws/Cargo.toml" -p vinculum-probe -- \
    --tcp "127.0.0.1:$port" --scan 3 --expect VinculumPeer $record
