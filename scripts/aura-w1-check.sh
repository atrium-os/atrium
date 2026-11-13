#!/bin/sh
# W1 gate for Aura (docs/spec/wifi.md §7): the FreeBSD VM joins a WPA2
# network run by real hostapd and gets an address over DHCP.
#
#   Linux VM (scripts/aura-ap-vm.sh): hostapd on mac80211_hwsim, PMF
#     required, WPA-PSK + WPA-PSK-SHA256, group rekey every 60 s; the
#     medium air-hwsim.py; udhcpd on 10.77.0.0/24.
#   FreeBSD VM: aura_wlan.ko + aura_sim.ko; aura-air (the radio's firmware,
#     test only) on UDP 7701; aura-keys, aura-mlme; aura-ctl as atrium-net.
#
#   sh scripts/aura-w1-check.sh vms     boot both VMs (FreeBSD with the UDP
#                                       forward; fork kernel via nextboot)
#   sh scripts/aura-w1-check.sh test    build, install, run the gate
#
# The FreeBSD VM must have been started with
#   NETDEV_EXTRA="hostfwd=udp:127.0.0.1:7701-:7701"
# (the vms step does it): the medium in the Linux VM sends to the host's
# 127.0.0.1:7701 through its own slirp, and that forward carries it on.
set -eu
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
KERNCONF="${KERNCONF:-ATRIUM-NODEBUG}"
T=aarch64-unknown-freebsd
S="$HOME/atrium-vm/sysroot"
V="$ROOT/scripts/vssh"
AP="$ROOT/scripts/aura-ap-vm.sh"
ARCA="${AURA_ARCA:-0}"

case "${1:-}" in
vms)
    sh "$AP" up
    if ! VSSH_CONNECT_TIMEOUT=5 "$V" true 2>/dev/null; then
        NETDEV_EXTRA="hostfwd=udp:127.0.0.1:7701-:7701" sh "$ROOT/scripts/cloud-vm.sh" up
    fi
    ARCH=aarch64 KERNCONF="$KERNCONF" sh "$ROOT/scripts/aura-vm-check.sh" kernel ;;
test)
    TARGET=arm64 KERNCONF="$KERNCONF" sh "$ROOT/scripts/aura-kmod.sh" >&2
    X="-C link-arg=--sysroot=$S -C link-arg=-L$S/lib -C link-arg=-L$S/usr/lib -C link-arg=--dynamic-linker=/libexec/ld-elf.so.1 -C link-arg=$S/usr/lib/Scrt1.o -C link-arg=$S/usr/lib/crti.o -C link-arg=$S/usr/lib/crtbeginS.o -C link-arg=-lc -C link-arg=$S/usr/lib/crtendS.o -C link-arg=$S/usr/lib/crtn.o"
    (cd "$ROOT/aura" && RUSTFLAGS="$X" cargo build --release -p aura-air -p aura-mlme -p aura-keys -p aura-ctl --target "$T" >&2)
    B=$(mktemp -d)/aura
    mkdir -p "$B"
    R="$ROOT/aura/target/$T/release"
    cp "$ROOT/aura/kmod/out/arm64-$KERNCONF/"*.ko "$R/aura-air" "$R/aura-mlme" "$R/aura-keys" "$R/aura-ctl" "$B/"
    tar czf - -C "$(dirname "$B")" aura | "$V" 'rm -rf /tmp/aura && tar xzf - -C /tmp'
    rm -rf "$(dirname "$B")"

    sh "$AP" ap hostapd-wpa2.conf

    # Arca's machine store when AURA_ARCA=1 (arcad running in the guest,
    # ARCA_SERVICES naming aura-keys=0 for this root-run gate); otherwise
    # credentials stay in aura-keys' memory.
    if [ "$ARCA" = 1 ]; then STORE="--arca /atrium/sockets/arca/arcad.sock"; else STORE="--memory-store"; fi
    "$V" "set -e
        sysctl -n kern.bootfile | grep -q kernel.atrium || { echo 'guest is not running the fork kernel: run the vms step'; exit 1; }
        cd /tmp/aura
        pkill -x aura-mlme aura-keys aura-air dhclient 2>/dev/null || true
        sleep 1
        kldunload aura_sim 2>/dev/null || true
        kldunload aura_wlan 2>/dev/null || true
        kldload ./aura_wlan.ko
        kldload ./aura_sim.ko
        ifconfig wlan0 up
        (./aura-air -v > /tmp/aura/air.log 2>&1 &)
        (./aura-keys -v $STORE --mlme-uid 0 --dev /dev/aura/key0 > /tmp/aura/keys.log 2>&1 &)
        sleep 1
        (./aura-mlme -v > /tmp/aura/mlme.log 2>&1 &)
        sleep 2"
    pass=0
    ok() { echo "ok   $1"; pass=$((pass + 1)); }
    fail() {
        echo "FAIL $1"
        "$V" 'for f in air keys mlme; do echo "--- $f"; tail -n 40 /tmp/aura/$f.log; done' || true
        sh "$AP" log || true
        exit 1
    }

    out=$("$V" '/tmp/aura/aura-ctl scan' 2>&1) || fail "scan: $out"
    echo "$out" | sed 's/^/    /'
    echo "$out" | grep -q 'ssid="aura-test".*security=psk-sha256' && ok "scan finds aura-test (WPA2, PMF required, PSK-SHA256)" || fail "scan result"

    r=$("$V" 'echo "correct horse battery staple" | /tmp/aura/aura-ctl remember aura-test') || fail "remember: $r"
    ok "credential remembered ($([ "$ARCA" = 1 ] && echo "Arca machine store" || echo "memory store"))"

    out=$("$V" '/tmp/aura/aura-ctl join aura-test --wait 180 --dhcp wlan0' 2>&1) || fail "join: $out"
    echo "$out" | sed 's/^/    /'
    echo "$out" | grep -q 'state=associated.*pmf=1' && ok "associated with PMF" || fail "associated with PMF"
    echo "$out" | grep -q 'state=authorized' && ok "4-way handshake done, port authorized" || fail "authorized"
    echo "$out" | grep -q 'address inet 10\.77\.0\.1[0-2][0-9]' && ok "DHCP address from udhcpd" || fail "DHCP address"

    sh "$AP" ssh "grep -q 'AP-STA-CONNECTED 02:a5:00:00:00:01' /tmp/hostapd.log" && ok "hostapd: AP-STA-CONNECTED" || fail "hostapd connected"
    sh "$AP" ssh "grep -q 'EAPOL-4WAY-HS-COMPLETED 02:a5:00:00:00:01' /tmp/hostapd.log" && ok "hostapd: EAPOL-4WAY-HS-COMPLETED" || fail "hostapd 4-way"

    "$V" 'ping -c 3 -t 30 10.77.0.1' >/dev/null && ok "ping the AP over CCMP" || fail "ping"
    ADDR=$("$V" "ifconfig wlan0 inet | awk '/inet / {print \$2}'")
    sh "$AP" ssh "ping -c 2 -W 10 $ADDR" >/dev/null && ok "AP pings us back ($ADDR)" || fail "ping from AP"

    # Group rekey (wpa_group_rekey=60): wait for it, then broadcast still works.
    i=0
    until [ "$("$V" 'grep -c SetGtk /tmp/aura/keys.log' 2>/dev/null || echo 0)" -ge 2 ]; do
        i=$((i + 1)); [ $i -lt 30 ] || fail "group rekey within 150 s"
        sleep 5
    done
    ok "group key handshake (GTK rekey) done"
    "$V" 'ping -c 2 -t 20 10.77.0.255' >/dev/null 2>&1 || true
    "$V" 'ping -c 3 -t 30 10.77.0.1' >/dev/null && ok "ping after rekey" || fail "ping after rekey"

    # PMF: a forged (unprotected) deauth from the AP's address is ignored.
    sh "$AP" ssh "hostapd_cli -p /run/hostapd deauthenticate 02:a5:00:00:00:01 test=0 >/dev/null 2>&1; true"
    sleep 5
    st=$("$V" '/tmp/aura/aura-ctl status')
    echo "    $st"
    if "$V" 'grep -q unprotected-deauth-ignored /tmp/aura/mlme.log'; then
        echo "$st" | grep -q 'state=authorized' && ok "unprotected deauth ignored (PMF), SA Query kept the link" || fail "forged deauth"
    else
        echo "skip unprotected deauth (this hostapd build did not send one)"
    fi

    out=$("$V" '/tmp/aura/aura-ctl leave') || true
    echo "$out" | grep -q 'state=disconnected' && ok "leave" || fail "leave"
    "$V" 'grep -q "aura-keys\|secret\|correct horse" /tmp/aura/mlme.log && exit 1; true' && ok "no secret in aura-mlme's log" || fail "secret leaked to mlme log"

    "$V" 'sysctl net.aura.wlan0; pkill -x aura-mlme aura-keys aura-air dhclient; sleep 1; kldunload aura_sim && kldunload aura_wlan && echo "modules unloaded cleanly"'
    echo "W1 gate: $pass checks passed" ;;
*)
    sed -n 2,20p "$0"; exit 2 ;;
esac
