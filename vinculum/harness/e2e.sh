#!/bin/sh
# Vinculum e2e (docs/spec/bluetooth.md §15): Rootcanal controller ->
# vinculum-hci (allowlist relay) -> vinculumd (host core), against a Bumble
# peer "VinculumPeer". Scenarios: B1 connect + ATT MTU exchange (ACL credits,
# L2CAP reassembly); B2 LE Secure Connections pairing in each association
# model, then reconnecting and encrypting with the stored key; Classic SSP;
# the control socket; B3 a Classic HID keyboard and B4 a HID-over-GATT mouse
# handed to a real tactusd.
#
#   sh vinculum/harness/e2e.sh
#   VINCULUM_E2E=hid sh vinculum/harness/e2e.sh   # only the Classic HID scenario (hogp: the LE one)
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
cargo build -q --manifest-path "$ws/Cargo.toml" -p vinculum-hci -p vinculumd -p vinculum-ctl 2>/dev/null
bin=$(cargo metadata -q --manifest-path "$ws/Cargo.toml" --format-version 1 | python3 -c 'import json,sys; print(json.load(sys.stdin)["target_directory"])')/debug

work=$(mktemp -d)
rc_pid= peer_pid= hci_pid= d_pid=
cleanup() { kill ${t_pid:-} ${w_pid:-} $d_pid $hci_pid $peer_pid $rc_pid 2>/dev/null || true; [ -n "${VINCULUM_KEEP:-}" ] && echo "logs kept in $work" || rm -rf "$work"; }
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
    if [ -n "${CLASSIC:-}" ]; then
        until grep -q "classic address" "$work/peer.log"; do sleep 0.1; done
        target="--classic $(sed -n 's/^classic address //p' "$work/peer.log")"
    else
        target="--connect VinculumPeer"
    fi
    d() { "$bin/vinculumd" --transport "unix:$work/hci.sock" $target --timeout 30 --passkey-file "$work/passkey" "$@"; }
    if [ -n "${EXPECT_FAIL:-}" ]; then
        # The run must fail, saying EXPECT_FAIL.
        if ! d "$@" >"$work/d.log" 2>&1 && grep -q "$EXPECT_FAIL" "$work/d.log"; then
            echo "ok   $name"
        else
            echo "FAIL $name (expected: $EXPECT_FAIL)"; cat "$work/d.log"; fails=$((fails + 1))
        fi
    elif d "$@" >"$work/d.log" 2>&1 && { [ -z "$AGAIN" ] || d $AGAIN >>"$work/d.log" 2>&1; }; then
        echo "ok   $name"
    else
        echo "FAIL $name"; echo "--- vinculumd"; cat "$work/d.log"; echo "--- peer"; cat "$work/peer.log"
        fails=$((fails + 1))
    fi
    kill $peer_pid 2>/dev/null || true
    wait $peer_pid 2>/dev/null || true
    peer_pid=
}

only=${VINCULUM_E2E:-all}
if [ "$only" = all ]; then
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
# No downgrade: the same device, having lost its keys, comes back able to do
# only Just Works. Re-pairing must not replace the authenticated bond.
EXPECT_FAIL="pairing refused: the new key is unauthenticated"
scenario "B2 no key downgrade (authenticated bond kept)" none --pair --keys "$work/keys" --machine-key "$work/machine.key"
EXPECT_FAIL=

# BR/EDR: Secure Simple Pairing (Secure Connections) by address, then a
# second process authenticates and encrypts with the stored link key.
CLASSIC=1
AGAIN="--pair --keys $work/keys4 --machine-key $work/machine.key"
scenario "B2 Classic SSP numeric comparison, bond survives a restart" display-yes-no --pair --keys "$work/keys4" --machine-key "$work/machine.key"
AGAIN=
grep -q "reconnect: encrypted with the stored key" "$work/d.log" || { echo "FAIL Classic second run did not use the stored bond"; cat "$work/d.log"; fails=$((fails + 1)); }
scenario "B2 Classic SSP just works" none --pair
CLASSIC=

# The service: vinculumd with the bluetooth-control socket, driven by
# vinculum-ctl as the pairing agent (spec §13).
"$venv/bin/python" "$here/peer_advertiser.py" "tcp-client:127.0.0.1:$port" VinculumPeer 60 display-yes-no >"$work/peer.log" 2>&1 &
peer_pid=$!
until grep -q advertising "$work/peer.log" 2>/dev/null; do sleep 0.1; done
"$bin/vinculumd" --transport "unix:$work/hci.sock" --control "$work/ctl.sock" --keys "$work/keys3" --machine-key "$work/machine.key" >"$work/svc.log" 2>&1 &
d_pid=$!
until [ -S "$work/ctl.sock" ]; do sleep 0.1; done
ctl() { "$bin/vinculum-ctl" -s "$work/ctl.sock" "$@"; }
control_ok=1
i=0
until ctl status 2>/dev/null | grep -q "ready: true"; do
    i=$((i + 1)); [ $i -gt 50 ] && break
    sleep 0.1
done
ctl status >"$work/ctl.log" 2>&1 || control_ok=0
ctl pair VinculumPeer --yes >>"$work/ctl.log" 2>&1 || control_ok=0
grep -q "paired .*(authenticated; keyboard allowed)" "$work/ctl.log" || control_ok=0
addr=$(sed -n 's/^found VinculumPeer at //p' "$work/ctl.log" | head -1)
ctl devices >"$work/dev.log" 2>&1 || control_ok=0
grep -q "VinculumPeer.*authenticated.*connected" "$work/dev.log" || control_ok=0
ctl disconnect "$addr" >>"$work/ctl.log" 2>&1 || control_ok=0
sleep 0.5
ctl connect "$addr" >>"$work/ctl.log" 2>&1 || control_ok=0
grep -q "connected .* (encrypted)" "$work/ctl.log" || control_ok=0
ctl devices >"$work/dev2.log" 2>&1 || control_ok=0
grep -q "connected" "$work/dev2.log" || control_ok=0
ctl remove "$addr" >>"$work/ctl.log" 2>&1 || control_ok=0
ctl devices >"$work/dev3.log" 2>&1 || control_ok=0
[ -s "$work/dev3.log" ] && control_ok=0
[ -z "$(ls -A "$work/keys3")" ] || control_ok=0
if [ $control_ok -eq 1 ]; then
    echo "ok   B2 control socket: pair, list, reconnect, remove (vinculum-ctl)"
else
    echo "FAIL B2 control socket"; for f in ctl dev dev2 dev3 svc peer; do echo "--- $f"; cat "$work/$f.log"; done
    fails=$((fails + 1))
fi
kill $d_pid $peer_pid 2>/dev/null || true
wait $d_pid $peer_pid 2>/dev/null || true
d_pid= peer_pid=
fi

# B3: a Classic HID keyboard. vinculumd (control + HID sockets) pairs it via
# vinculum-ctl, reads its SDP record, opens control and interrupt and hands
# it to a real tactusd on hid.sock; `tactusd --watch` must see the keys.
# Then the keyboard drops the link and pages us itself (page scan), opens
# the channels from its side, and types again.
tactus_ws=$(dirname "$ws")/tactus
cargo build -q --manifest-path "$tactus_ws/Cargo.toml" 2>/dev/null
tbin=$(cargo metadata -q --manifest-path "$tactus_ws/Cargo.toml" --format-version 1 | python3 -c 'import json,sys; print(json.load(sys.stdin)["target_directory"])')/debug
t_pid= w_pid=
if [ "$only" = all ] || [ "$only" = hid ]; then
"$bin/vinculumd" --transport "unix:$work/hci.sock" --control "$work/ctl5.sock" --hid "$work/hid.sock" \
    --keys "$work/keys5" --machine-key "$work/machine.key" >"$work/svc5.log" 2>&1 &
d_pid=$!
until [ -S "$work/hid.sock" ]; do sleep 0.1; done
TACTUS_PEERS=any TACTUS_BLUETOOTH_SOCKET="$work/hid.sock" "$tbin/tactusd" --socket "$work/tactus.sock" >"$work/tactus.log" 2>&1 &
t_pid=$!
until [ -S "$work/tactus.sock" ]; do sleep 0.1; done
"$tbin/tactusd" --watch "$work/tactus.sock" >"$work/watch.log" 2>&1 &
w_pid=$!
ctl() { "$bin/vinculum-ctl" -s "$work/ctl5.sock" "$@"; }
i=0
until ctl status 2>/dev/null | grep -q "ready: true"; do
    i=$((i + 1)); [ $i -gt 50 ] && break
    sleep 0.1
done
host_addr=$(ctl status | sed -n 's/.*bytes: \[\([^]]*\)\].*/\1/p' | python3 -c 'import sys; print(":".join("%02X" % int(x) for x in sys.stdin.read().split(",")))')
"$venv/bin/python" "$here/peer_hid_keyboard.py" "tcp-client:127.0.0.1:$port" 20 "$host_addr" >"$work/kbd.log" 2>&1 &
peer_pid=$!
until grep -q "classic address" "$work/kbd.log" 2>/dev/null; do sleep 0.1; done
kaddr=$(sed -n 's/^classic address //p' "$work/kbd.log")
hid_ok=1
ctl pair "$kaddr" --classic --yes >"$work/ctl5.log" 2>&1 || hid_ok=0
grep -q "paired .*(authenticated; keyboard allowed)" "$work/ctl5.log" || hid_ok=0
# Usages 0x070004..6 are a, b, c.
i=0
until grep -q "usage: 458758, down: true" "$work/watch.log" 2>/dev/null; do
    i=$((i + 1)); [ $i -gt 200 ] && break
    sleep 0.1
done
for u in 458756 458757 458758; do
    grep -q "usage: $u, down: true" "$work/watch.log" || hid_ok=0
    grep -q "usage: $u, down: false" "$work/watch.log" || hid_ok=0
done
grep -q "Device {.*Bluetooth.*vendor: 4660, product: 22136" "$work/watch.log" || hid_ok=0
if [ $hid_ok -eq 1 ]; then
    echo "ok   B3 Classic HID keyboard to tactusd, pair + keyboard-initiated reconnect"
else
    echo "FAIL B3 Classic HID"; for f in ctl5 svc5 kbd tactus watch; do echo "--- $f"; cat "$work/$f.log"; done
    fails=$((fails + 1))
fi
kill $d_pid $peer_pid $t_pid $w_pid 2>/dev/null || true
wait $d_pid $peer_pid $t_pid $w_pid 2>/dev/null || true
d_pid= peer_pid=
fi

# B4: a HID-over-GATT mouse over LE. Paired through vinculum-ctl, vinculumd
# discovers its GATT database, tactusd gets the device and its motion and
# clicks; battery changes reach vinculumd; after the mouse drops the link
# and advertises again, vinculumd reconnects by itself and it works again.
if [ "$only" = all ] || [ "$only" = hogp ]; then
"$bin/vinculumd" --transport "unix:$work/hci.sock" --control "$work/ctl6.sock" --hid "$work/hid6.sock" \
    --keys "$work/keys6" --machine-key "$work/machine.key" >"$work/svc6.log" 2>&1 &
d_pid=$!
until [ -S "$work/hid6.sock" ]; do sleep 0.1; done
TACTUS_PEERS=any TACTUS_BLUETOOTH_SOCKET="$work/hid6.sock" "$tbin/tactusd" --socket "$work/tactus6.sock" >"$work/tactus6.log" 2>&1 &
t_pid=$!
until [ -S "$work/tactus6.sock" ]; do sleep 0.1; done
"$tbin/tactusd" --watch "$work/tactus6.sock" >"$work/watch6.log" 2>&1 &
w_pid=$!
"$venv/bin/python" "$here/peer_hogp_mouse.py" "tcp-client:127.0.0.1:$port" 30 >"$work/mouse.log" 2>&1 &
peer_pid=$!
until grep -q advertising "$work/mouse.log" 2>/dev/null; do sleep 0.1; done
ctl() { "$bin/vinculum-ctl" -s "$work/ctl6.sock" "$@"; }
i=0
until ctl status 2>/dev/null | grep -q "ready: true"; do
    i=$((i + 1)); [ $i -gt 50 ] && break
    sleep 0.1
done
hogp_ok=1
ctl pair VinculumMouse --yes >"$work/ctl6.log" 2>&1 || hogp_ok=0
# No `connect`: vinculumd scans in the background for the bonded mouse and
# reconnects when it advertises again.
i=0
until [ "$(grep -c "raw_dx: 10" "$work/watch6.log" 2>/dev/null)" -ge 6 ]; do
    i=$((i + 1)); [ $i -gt 150 ] && break
    sleep 0.1
done
[ "$(grep -c "raw_dx: 10" "$work/watch6.log")" -ge 6 ] || hogp_ok=0
grep -q "Button {.*down: true" "$work/watch6.log" || hogp_ok=0
grep -q "Device {.*Bluetooth.*vendor: 4660, product: 22136" "$work/watch6.log" || hogp_ok=0
grep -q "battery 60%" "$work/svc6.log" || hogp_ok=0
if [ $hogp_ok -eq 1 ]; then
    echo "ok   B4 HID-over-GATT mouse to tactusd, battery, automatic reconnect"
else
    echo "FAIL B4 HOGP mouse"; for f in ctl6 svc6 mouse tactus6 watch6; do echo "--- $f"; cat "$work/$f.log"; done
    fails=$((fails + 1))
fi
kill $d_pid $peer_pid $t_pid $w_pid 2>/dev/null || true
wait $d_pid $peer_pid $t_pid $w_pid 2>/dev/null || true
d_pid= peer_pid=
fi

if [ $fails -eq 0 ]; then
    echo "PASS: vinculum e2e"
else
    echo "--- vinculum-hci"; cat "$work/hci.log"
    echo "FAIL: vinculum e2e ($fails)"; exit 1
fi
