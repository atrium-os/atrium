#!/bin/sh
# Vinculum e2e (docs/spec/bluetooth.md §15): Rootcanal controller ->
# vinculum-hci (allowlist relay) -> vinculumd (host core), against a Bumble
# peer "VinculumPeer". Scenarios: B1 connect + ATT MTU exchange (ACL credits,
# L2CAP reassembly); B2 LE Secure Connections pairing in each association
# model, then reconnecting and encrypting with the stored key.
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

"$bin/vinculum-hci" --controller "tcp:127.0.0.1:$port" --host-listen "$work/hci.sock" 2>"$work/hci.log" &
hci_pid=$!
i=0
until [ -S "$work/hci.sock" ]; do
    i=$((i + 1)); [ $i -gt 50 ] && { cat "$work/hci.log"; echo "FAIL: vinculum-hci did not start"; exit 1; }
    sleep 0.1
done

# scenario NAME PEER_IOCAP VINCULUMD_ARGS... : a fresh peer, one vinculumd run
# (two runs against the same peer when AGAIN holds the second run's args).
fails=0
AGAIN=
scenario() {
    name=$1 iocap=$2; shift 2
    rm -f "$work/passkey"
    : >"$work/peer.log"
    "$venv/bin/python" "$here/peer_advertiser.py" "tcp-client:127.0.0.1:$port" VinculumPeer 60 "$iocap" "$work/passkey" >"$work/peer.log" 2>&1 &
    peer_pid=$!
    i=0
    until grep -q advertising "$work/peer.log" 2>/dev/null; do
        i=$((i + 1)); [ $i -gt 100 ] && { cat "$work/peer.log"; echo "FAIL: peer did not start"; exit 1; }
        sleep 0.1
    done
    d() { "$bin/vinculumd" --transport "unix:$work/hci.sock" --connect VinculumPeer --timeout 30 --passkey-file "$work/passkey" "$@"; }
    if d "$@" >"$work/d.log" 2>&1 && { [ -z "$AGAIN" ] || d $AGAIN >>"$work/d.log" 2>&1; }; then
        echo "ok   $name"
    else
        echo "FAIL $name"; echo "--- vinculumd"; cat "$work/d.log"; echo "--- peer"; cat "$work/peer.log"
        fails=$((fails + 1))
    fi
    kill $peer_pid 2>/dev/null || true
    wait $peer_pid 2>/dev/null || true
    peer_pid=
}

scenario "B1 connect + ATT MTU" none --att-mtu
scenario "B2 Just Works, reconnect with stored key" none --pair --reconnect
scenario "B2 numeric comparison, reconnect" display-yes-no --pair --reconnect
scenario "B2 passkey (we display, peer types)" keyboard --pair
# Persistence: pair into the sealed file store, then a new vinculumd process
# finds the bond and encrypts with it.
AGAIN="--pair --keys $work/keys --machine-key $work/machine.key"
scenario "B2 bond survives a restart (file store)" display-yes-no --pair --keys "$work/keys" --machine-key "$work/machine.key"
AGAIN=
grep -q "reconnect: encrypted with the stored key" "$work/d.log" || { echo "FAIL second run did not use the stored bond"; cat "$work/d.log"; fails=$((fails + 1)); }

if [ $fails -eq 0 ]; then
    echo "PASS: vinculum e2e"
else
    echo "--- vinculum-hci"; cat "$work/hci.log"
    echo "FAIL: vinculum e2e ($fails)"; exit 1
fi
