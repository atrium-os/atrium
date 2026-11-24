#!/bin/sh
# Curia's Bluetooth page (docs/spec/curia.md §14) against a real vinculumd,
# on the Linux host: Rootcanal (virtual controller) -> vinculum-hci ->
# vinculumd with its bluetooth-control socket, and a Bumble peer. Curia's
# page model drives it (curia/examples/bt_drive.rs) as a person pressing
# the page's buttons would: find, pair answering the prompt, disconnect,
# connect with the stored bond, forget after asking. Three peers, one per
# prompt: a number to compare, a code Settings shows (the peer types it), a
# code the peer shows (typed into Settings); and a Classic-only keyboard
# found by inquiry (named in its answer, or asked). Each run also turns
# Bluetooth off (the link drops) and on. Ends "curia-bt: N failed".
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
run() { # name peer-iocap [user to drive as [peer mode]]
    name=$1 iocap=$2 as=${3:-} mode=${4:-} pname=CuriaPeer
    [ -n "$mode" ] && pname=CuriaKeys
    rm -f "$work/passkey"; : >"$work/peer.log"
    "$venv/bin/python" "$H/peer_advertiser.py" "tcp-client:127.0.0.1:$port" "$pname" 90 "$iocap" "$work/passkey" $mode >"$work/peer.log" 2>&1 &
    peer_pid=$!
    i=0; until grep -q advertising "$work/peer.log" 2>/dev/null; do i=$((i+1)); [ $i -gt 200 ] && break; sleep 0.1; done
    if [ "$iocap" = display ]; then
        # The code the peer shows, as the person reads it off the device.
        ( until grep -q "peer displays" "$work/peer.log"; do sleep 0.1; done
          sed -n 's/^peer displays //p' "$work/peer.log" | head -1 > "$work/passkey" ) &
        w_pid=$!
    fi
    rm -rf "$work/keys"; mkdir -p "$work/keys"
    "$vbin/vinculumd" --transport "unix:$work/hci.sock" --control "$work/ctl.sock" --control-apps "$work/registry" "$work/apps" --keys "$work/keys" --machine-key "$work/machine.key" >"$work/svc.log" 2>&1 &
    d_pid=$!
    i=0; until [ -S "$work/ctl.sock" ]; do i=$((i+1)); [ $i -gt 100 ] && break; sleep 0.1; done
    chmod 666 "$work/ctl.sock" 2>/dev/null
    if [ -n "$as" ]; then
        cp "$drive" "$work/bt_drive"; chmod 755 "$work/bt_drive"
        cmd="runuser -u $as -- $work/bt_drive"
    else
        cmd=$drive
    fi
    if timeout 180 $cmd "$work/ctl.sock" "$pname" "$work/passkey" >"$work/drive.log" 2>&1 && grep -q "bt-drive: ok" "$work/drive.log"; then
        echo "  ok $name"; sed -n 's/^step: /     /p' "$work/drive.log"
    else
        echo "  FAIL $name"; for f in drive svc peer; do echo "--- $f"; tail -15 "$work/$f.log"; done
        fails=$((fails+1))
    fi
    kill $d_pid $peer_pid $w_pid 2>/dev/null; wait $d_pid $peer_pid 2>/dev/null; d_pid= peer_pid= w_pid=
    rm -f "$work/ctl.sock"
}
# Settings as an Atrium app: the launch registry names the uid, and its
# installed manifest holds bluetooth-control (vinculumd --control-apps).
chmod 755 "$work"; : > "$work/passkey"; chmod 666 "$work/passkey"
mkdir -p "$work/apps/org.atrium.curia" "$work/apps/org.example.game"
printf '[app]\nid = "org.atrium.curia"\n\n[capabilities]\ngraphics = "fresco"\nbluetooth-control = true\n' > "$work/apps/org.atrium.curia/atrium.toml"
printf '[app]\nid = "org.example.game"\n\n[capabilities]\ngraphics = "fresco"\n' > "$work/apps/org.example.game/atrium.toml"
printf '%s alice org.atrium.curia\n' "$(id -u nobody)" > "$work/registry"
run "numbers to compare, as an app the registry names (uid $(id -u nobody))" display-yes-no nobody
# The same uid as an app without the capability: refused.
printf '%s alice org.example.game\n' "$(id -u nobody)" > "$work/registry"
"$vbin/vinculumd" --transport "unix:$work/hci.sock" --control "$work/ctl.sock" --control-apps "$work/registry" "$work/apps" --keys "$work/keys" --machine-key "$work/machine.key" >"$work/svc.log" 2>&1 &
d_pid=$!
i=0; until [ -S "$work/ctl.sock" ]; do i=$((i+1)); [ $i -gt 100 ] && break; sleep 0.1; done
chmod 666 "$work/ctl.sock"
timeout 20 runuser -u nobody -- "$work/bt_drive" "$work/ctl.sock" CuriaPeer >"$work/drive.log" 2>&1
if grep -q "control client uid $(id -u nobody) refused" "$work/svc.log" && ! grep -q "bt-drive: ok" "$work/drive.log"; then
    echo "  ok an app without bluetooth-control is refused"
else
    echo "  FAIL an app without bluetooth-control: $(tail -3 "$work/svc.log")"; fails=$((fails+1))
fi
kill $d_pid 2>/dev/null; wait $d_pid 2>/dev/null; d_pid=; rm -f "$work/ctl.sock"
run "numbers to compare" display-yes-no
run "a code Settings shows, typed on the device" keyboard
run "a code the device shows, typed into Settings" display
# A Classic keyboard: found by inquiry (no LE advertising), paired over
# BR/EDR.
run "a Classic keyboard found by inquiry, a code it types" keyboard "" classic
# One that leaves its name out of the inquiry answer: asked for it.
run "a Classic keyboard that is asked its name" display-yes-no "" classic-unnamed
echo "curia-bt: $fails failed"
[ "$fails" -eq 0 ]
