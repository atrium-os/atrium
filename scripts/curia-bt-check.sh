#!/bin/sh
# Curia's Bluetooth page (docs/spec/curia.md §14) against a real vinculumd,
# on the Linux host: Rootcanal (virtual controller) -> vinculum-hci ->
# vinculumd with its bluetooth-control socket, and a Bumble peer. Curia's
# page model drives it (curia/examples/bt_drive.rs) as a person pressing
# the page's buttons would: find, pair answering the prompt, disconnect,
# connect with the stored bond, forget after asking. Three peers, one per
# prompt: a number to compare, a code Settings shows (the peer types it), a
# code the peer shows (typed into Settings). Ends "curia-bt: N failed".
#
#   sh scripts/curia-bt-check.sh
#
# Needs python3-venv; installs rootcanal and bumble in ~/.cache/vinculum-venv
# as vinculum/harness/e2e.sh does.
set -u
ROOT=$(cd "$(dirname "$0")/.." && pwd)
H=$ROOT/vinculum/harness
venv=${VINCULUM_VENV:-$HOME/.cache/vinculum-venv}
port=${VINCULUM_HCI_PORT:-6512}
if [ ! -x "$venv/bin/python" ]; then
    python3 -m venv "$venv" && "$venv/bin/pip" install -q rootcanal bumble || { echo "curia-bt: 1 failed (no venv)"; exit 1; }
fi
cargo build -q --manifest-path "$ROOT/vinculum/Cargo.toml" -p vinculum-hci -p vinculumd || { echo "curia-bt: 1 failed (build)"; exit 1; }
cargo build -q --manifest-path "$ROOT/curia/Cargo.toml" --example bt_drive || { echo "curia-bt: 1 failed (build)"; exit 1; }
vbin=$(cargo metadata -q --manifest-path "$ROOT/vinculum/Cargo.toml" --format-version 1 | python3 -c 'import json,sys; print(json.load(sys.stdin)["target_directory"])')/debug
drive=$(cargo metadata -q --manifest-path "$ROOT/curia/Cargo.toml" --format-version 1 | python3 -c 'import json,sys; print(json.load(sys.stdin)["target_directory"])')/debug/examples/bt_drive

work=$(mktemp -d)
rc_pid= hci_pid= peer_pid= d_pid= w_pid=
cleanup() { kill $w_pid $d_pid $peer_pid $hci_pid $rc_pid 2>/dev/null; [ -n "${KEEP:-}" ] && echo "logs in $work" || rm -rf "$work"; }
trap cleanup EXIT INT TERM
"$venv/bin/python" -m rootcanal --hci_port "$port" --link_port $((port + 1)) --link_ble_port $((port + 2)) --test_port $((port - 1)) >"$work/rootcanal.log" 2>&1 &
rc_pid=$!
i=0; until nc -z 127.0.0.1 "$port" 2>/dev/null; do i=$((i+1)); [ $i -gt 100 ] && { cat "$work/rootcanal.log"; echo "curia-bt: 1 failed (rootcanal)"; exit 1; }; sleep 0.1; done
"$vbin/vinculum-hci" --controller "tcp:127.0.0.1:$port" --host-listen "$work/hci.sock" 2>"$work/hci.log" &
hci_pid=$!
i=0; until [ -S "$work/hci.sock" ]; do i=$((i+1)); [ $i -gt 100 ] && { cat "$work/hci.log"; echo "curia-bt: 1 failed (vinculum-hci)"; exit 1; }; sleep 0.1; done

fails=0
run() { # name peer-iocap
    name=$1 iocap=$2
    rm -f "$work/passkey"; : >"$work/peer.log"
    "$venv/bin/python" "$H/peer_advertiser.py" "tcp-client:127.0.0.1:$port" CuriaPeer 90 "$iocap" "$work/passkey" >"$work/peer.log" 2>&1 &
    peer_pid=$!
    i=0; until grep -q advertising "$work/peer.log" 2>/dev/null; do i=$((i+1)); [ $i -gt 200 ] && break; sleep 0.1; done
    if [ "$iocap" = display ]; then
        # The code the peer shows, as the person reads it off the device.
        ( until grep -q "peer displays" "$work/peer.log"; do sleep 0.1; done
          sed -n 's/^peer displays //p' "$work/peer.log" | head -1 > "$work/passkey" ) &
        w_pid=$!
    fi
    rm -rf "$work/keys"; mkdir -p "$work/keys"
    "$vbin/vinculumd" --transport "unix:$work/hci.sock" --control "$work/ctl.sock" --control-uid "$(id -u)" --keys "$work/keys" --machine-key "$work/machine.key" >"$work/svc.log" 2>&1 &
    d_pid=$!
    i=0; until [ -S "$work/ctl.sock" ]; do i=$((i+1)); [ $i -gt 100 ] && break; sleep 0.1; done
    if timeout 180 "$drive" "$work/ctl.sock" CuriaPeer "$work/passkey" >"$work/drive.log" 2>&1 && grep -q "bt-drive: ok" "$work/drive.log"; then
        echo "  ok $name"; sed -n 's/^step: /     /p' "$work/drive.log"
    else
        echo "  FAIL $name"; for f in drive svc peer; do echo "--- $f"; tail -15 "$work/$f.log"; done
        fails=$((fails+1))
    fi
    kill $d_pid $peer_pid $w_pid 2>/dev/null; wait $d_pid $peer_pid 2>/dev/null; d_pid= peer_pid= w_pid=
    rm -f "$work/ctl.sock"
}
run "numbers to compare" display-yes-no
run "a code Settings shows, typed on the device" keyboard
run "a code the device shows, typed into Settings" display
echo "curia-bt: $fails failed"
