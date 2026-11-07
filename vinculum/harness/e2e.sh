#!/bin/sh
# Vinculum B1 e2e (docs/spec/bluetooth.md §15): Rootcanal controller ->
# vinculum-hci (allowlist relay) -> vinculumd (host core). vinculumd scans,
# finds the Bumble peer "VinculumPeer", connects over LE, does an ATT MTU
# exchange through ACL credits and L2CAP reassembly, and disconnects.
#
#   sh vinculum/harness/e2e.sh
#
# Same requirements as run.sh.
set -eu
here=$(cd "$(dirname "$0")" && pwd)
ws=$(dirname "$here")
venv=${VINCULUM_VENV:-$HOME/.cache/vinculum-venv}
port=${VINCULUM_HCI_PORT:-6412}

if [ ! -x "$venv/bin/python" ]; then
    python3 -m venv "$venv"
    "$venv/bin/pip" install -q rootcanal bumble
fi
cargo build -q --manifest-path "$ws/Cargo.toml" -p vinculum-hci -p vinculumd 2>/dev/null
bin=$(cargo metadata -q --manifest-path "$ws/Cargo.toml" --format-version 1 | python3 -c 'import json,sys; print(json.load(sys.stdin)["target_directory"])')/debug

work=$(mktemp -d)
rc_pid= peer_pid= hci_pid=
cleanup() { kill $hci_pid $peer_pid $rc_pid 2>/dev/null || true; rm -rf "$work"; }
trap cleanup EXIT INT TERM

"$venv/bin/python" -m rootcanal --hci_port "$port" --link_port $((port + 1)) \
    --link_ble_port $((port + 2)) --test_port $((port - 1)) >"$work/rootcanal.log" 2>&1 &
rc_pid=$!
i=0
until nc -z 127.0.0.1 "$port" 2>/dev/null; do
    i=$((i + 1)); [ $i -gt 50 ] && { cat "$work/rootcanal.log"; echo "FAIL: rootcanal did not start"; exit 1; }
    sleep 0.1
done

"$venv/bin/python" "$here/peer_advertiser.py" "tcp-client:127.0.0.1:$port" VinculumPeer 60 >"$work/peer.log" 2>&1 &
peer_pid=$!
i=0
until grep -q advertising "$work/peer.log" 2>/dev/null; do
    i=$((i + 1)); [ $i -gt 100 ] && { cat "$work/peer.log"; echo "FAIL: peer did not start"; exit 1; }
    sleep 0.1
done

"$bin/vinculum-hci" --controller "tcp:127.0.0.1:$port" --host-listen "$work/hci.sock" 2>"$work/hci.log" &
hci_pid=$!
i=0
until [ -S "$work/hci.sock" ]; do
    i=$((i + 1)); [ $i -gt 50 ] && { cat "$work/hci.log"; echo "FAIL: vinculum-hci did not start"; exit 1; }
    sleep 0.1
done

if "$bin/vinculumd" --transport "unix:$work/hci.sock" --connect VinculumPeer --att-mtu --timeout 20; then
    echo "PASS: vinculum B1 e2e"
else
    echo "--- vinculum-hci"; cat "$work/hci.log"; echo "--- peer"; cat "$work/peer.log"
    echo "FAIL: vinculum B1 e2e"; exit 1
fi
