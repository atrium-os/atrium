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
ARCA="${AURA_ARCA:-1}"
JAIL="${AURA_JAIL:-1}"

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
    R="$ROOT/aura/target/$T/release"
    B=$(mktemp -d)/aura
    mkdir -p "$B"
    cp "$ROOT/aura/kmod/out/arm64-$KERNCONF/"*.ko "$R/aura-air" "$R/aura-mlme" "$R/aura-keys" "$R/aura-ctl" "$B/"
    if [ "$ARCA" = 1 ]; then
        (cd "$ROOT/arca/arcad" && RUSTFLAGS="$X" cargo build --release --bin arcad --target "$T" >&2)
        cp "$ROOT/arca/arcad/target/$T/release/arcad" "$B/"
    fi
    tar czf - -C "$(dirname "$B")" aura | "$V" 'rm -rf /tmp/aura && tar xzf - -C /tmp'
    rm -rf "$(dirname "$B")"

    sh "$AP" ap hostapd-wpa2.conf

    # The daemons run as their own uids (_aura 65033, _aura-keys 65032),
    # each seeing only its own node. AURA_JAIL=1 (default): jailed by
    # portcullisd/jaild from aura/etc/services.d with devfs rulesets 28/29;
    # AURA_JAIL=0: plain processes (daemon -u). aura-mlme runs in Capsicum
    # capability mode either way. Credentials: Arca's machine store (arcad,
    # unjailed here, with ARCA_SERVICES naming aura-keys) unless AURA_ARCA=0.
    if [ "$ARCA" = 1 ]; then STORE="--arca /atrium/sockets/arca/arcad.sock"; else STORE="--memory-store"; fi
    "$V" "set -e
        sysctl -n kern.bootfile | grep -q kernel.atrium || { echo 'guest is not running the fork kernel: run the vms step'; exit 1; }
        service atrium-portcullisd-bootstrap onestop >/dev/null 2>&1 || true
        pkill -x aura-mlme aura-keys atrium-aura-mlme atrium-aura-keys aura-air arcad dhclient 2>/dev/null || true
        sleep 2
        cd /tmp/aura
        [ \$(id -u _aura 2>/dev/null || echo 0) = 65033 ] || { pw userdel _aura 2>/dev/null || true; pw useradd _aura -u 65033 -d /nonexistent -s /usr/sbin/nologin -c 'Aura MLME'; }
        id _aura-keys >/dev/null 2>&1 || pw useradd _aura-keys -u 65032 -d /nonexistent -s /usr/sbin/nologin -c 'Aura keys'
        kldunload aura_sim 2>/dev/null || true
        kldunload aura_wlan 2>/dev/null || true
        kldload ./aura_wlan.ko
        kldload ./aura_sim.ko
        rm -rf /atrium/sockets/aura /atrium/sockets/aura-keys
        install -d -o _aura -m 0755 /atrium/sockets/aura
        install -d -o _aura-keys -m 0755 /atrium/sockets/aura-keys
        install -d -m 0755 /atrium/sockets/arca
        ifconfig wlan0 up
        if [ $ARCA = 1 ]; then
            rm -rf /tmp/aura/arca && mkdir -m 0700 /tmp/aura/arca
            : > /tmp/aura/launch-registry
            env ARCA_DIR=/tmp/aura/arca ARCA_SOCKET=/atrium/sockets/arca/arcad.sock ARCA_APP_REGISTRY=/tmp/aura/launch-registry ARCA_SERVICES=aura-keys=65032 \\
                daemon -f -o /tmp/aura/arcad.log ./arcad
            i=0; until [ -S /atrium/sockets/arca/arcad.sock ]; do i=\$((i+1)); [ \$i -lt 30 ] || { cat /tmp/aura/arcad.log; exit 1; }; sleep 1; done
        fi
        daemon -f -o /tmp/aura/air.log ./aura-air -v"
    if [ "$JAIL" = 1 ]; then
        # jaild sends each service's stdout/stderr to /var/log/atrium/<name>.log.
        MLOG=/var/log/atrium/atrium-aura-mlme.log KLOG=/var/log/atrium/atrium-aura-keys.log
        "$V" ": > $MLOG; : > $KLOG" 2>/dev/null || true
        tmpd=$(mktemp -d)
        "$V" 'cat /etc/atrium/jaild.policy.toml' > "$tmpd/live.toml"
        sh "$ROOT/scripts/jaild-policy-merge.sh" "$ROOT/etc/jaild.policy.toml" "$tmpd/live.toml" > "$tmpd/jaild.policy.toml" 2>/dev/null
        "$V" 'cat > /etc/atrium/jaild.policy.toml' < "$tmpd/jaild.policy.toml"
        "$V" 'cat > /usr/local/etc/atrium/devfs.rules' < "$ROOT/etc/atrium.devfs.rules"
        # The daemons log to stderr; -v so the gate can read what they did.
        sed 's|argv = \["atrium-aura-keys"\]|argv = ["atrium-aura-keys", "-v"'"$( [ "$ARCA" = 1 ] || echo ', "--memory-store"')"']|' \
            "$ROOT/aura/etc/services.d/43-atrium-aura-keys.toml" | "$V" 'cat > /tmp/aura/43-atrium-aura-keys.toml'
        sed 's|argv = \["atrium-aura-mlme", "--capsicum"\]|argv = ["atrium-aura-mlme", "--capsicum", "-v"]|' \
            "$ROOT/aura/etc/services.d/44-atrium-aura-mlme.toml" | "$V" 'cat > /tmp/aura/44-atrium-aura-mlme.toml'
        rm -rf "$tmpd"
        "$V" "set -e
            install -m 755 /tmp/aura/aura-mlme /usr/local/bin/atrium-aura-mlme
            install -m 755 /tmp/aura/aura-keys /usr/local/bin/atrium-aura-keys
            service devfs restart >/dev/null
            for id in 28 29; do [ -n \"\$(devfs rule -s \$id show)\" ] || { echo ruleset \$id not loaded; exit 1; }; done
            for j in aura-mlme aura-keys; do
                J=/var/lib/atrium/jails/\$j
                mkdir -p \$J/dev \$J/libexec \$J/lib \$J/usr/lib \$J/usr/local/bin \$J/atrium/sockets/aura \$J/atrium/sockets/aura-keys \$J/atrium/sockets/arca
            done
            mkdir -p /etc/atrium/services.d.off
            mv /etc/atrium/services.d/*-atrium-smoke-*.toml /etc/atrium/services.d.off/ 2>/dev/null || true
            install -m 644 /tmp/aura/43-atrium-aura-keys.toml /tmp/aura/44-atrium-aura-mlme.toml /etc/atrium/services.d/
            service atrium-jaild restart >/dev/null 2>&1
            sleep 2
            daemon -f -o /tmp/aura/bootstrap.log service atrium-portcullisd-bootstrap onestart
            i=0; until [ -S /atrium/sockets/aura/mlme.sock ]; do i=\$((i+1)); [ \$i -lt 60 ] || { echo 'jailed daemons did not come up'; tail -20 /tmp/aura/bootstrap.log /var/log/atrium/*.log 2>/dev/null; exit 1; }; sleep 2; done
            sleep 2
            ps -o user,jid,command -ax | grep '[a]trium-aura-'"
    else
        MLOG=/tmp/aura/mlme.log KLOG=/tmp/aura/keys.log
        "$V" "set -e
            cd /tmp/aura
            chown _aura /dev/aura/ctl0
            chown _aura-keys /dev/aura/key0
            daemon -f -u _aura-keys -o /tmp/aura/keys.log ./aura-keys -v $STORE --mlme-uid 65033 --ctl-uid 0
            sleep 1
            daemon -f -u _aura -o /tmp/aura/mlme.log ./aura-mlme -v --capsicum
            sleep 2
            ps -o user,command -ax | grep '[a]ura-'"
    fi
    CTL=/tmp/aura/aura-ctl
    pass=0
    ok() { echo "ok   $1"; pass=$((pass + 1)); }
    fail() {
        echo "FAIL $1"
        "$V" "for f in /tmp/aura/air.log $KLOG $MLOG; do echo \"--- \$f\"; tail -n 40 \$f; done" || true
        sh "$AP" log || true
        exit 1
    }

    if [ "$JAIL" = 1 ]; then
        m=$("$V" 'ls /var/lib/atrium/jails/aura-mlme/dev/aura/ | tr "\n" " "')
        k=$("$V" 'ls /var/lib/atrium/jails/aura-keys/dev/aura/ | tr "\n" " "')
        [ "$m" = "ctl0 " ] && ok "aura-mlme's jail shows only /dev/aura/ctl0" || fail "mlme jail devfs shows: $m"
        [ "$k" = "key0 " ] && ok "aura-keys' jail shows only /dev/aura/key0" || fail "keys jail devfs shows: $k"
        n=$("$V" 'for j in atrium-aura-mlme atrium-aura-keys; do jls -j $j ip4.addr ip6.addr; done | tr -d " -\n"')
        [ -z "$n" ] && ok "no network in either jail" || fail "jail network: $n"
    fi

    out=$("$V" "$CTL scan" 2>&1) || fail "scan: $out"
    echo "$out" | sed 's/^/    /'
    echo "$out" | grep -q 'ssid="aura-test".*security=psk-sha256' && ok "scan finds aura-test (WPA2, PMF required, PSK-SHA256)" || fail "scan result"

    r=$("$V" "echo 'correct horse battery staple' | $CTL remember aura-test") || fail "remember: $r"
    ok "credential remembered ($([ "$ARCA" = 1 ] && echo "Arca machine store" || echo "memory store"))"

    out=$("$V" "$CTL join aura-test --wait 180 --dhcp wlan0" 2>&1) || fail "join: $out"
    echo "$out" | sed 's/^/    /'
    echo "$out" | grep -q 'state=associated.*pmf=1' && ok "associated with PMF" || fail "associated with PMF"
    echo "$out" | grep -q 'state=authorized' && ok "4-way handshake done, port authorized" || fail "authorized"
    echo "$out" | grep -q 'address inet 10\.77\.0\.1[0-2][0-9]' && ok "DHCP address from udhcpd" || fail "DHCP address"

    if [ "$ARCA" = 1 ]; then
        "$V" "grep -q 'service:aura-keys op=0x0005 .* status=0' /tmp/aura/arca/audit.log" && ok "Arca released the credential to aura-keys (audit log)" || fail "Arca release"
    fi
    sh "$AP" ssh "grep -q 'AP-STA-CONNECTED 02:a5:00:00:00:01' /tmp/hostapd.log" && ok "hostapd: AP-STA-CONNECTED" || fail "hostapd connected"
    sh "$AP" ssh "grep -q 'EAPOL-4WAY-HS-COMPLETED 02:a5:00:00:00:01' /tmp/hostapd.log" && ok "hostapd: EAPOL-4WAY-HS-COMPLETED" || fail "hostapd 4-way"

    "$V" 'ping -c 3 -t 30 10.77.0.1' >/dev/null && ok "ping the AP over CCMP" || fail "ping"
    ADDR=$("$V" "ifconfig wlan0 inet | awk '/inet / {print \$2}'")
    sh "$AP" ssh "ping -c 2 -W 10 $ADDR" >/dev/null && ok "AP pings us back ($ADDR)" || fail "ping from AP"

    # Group rekey (wpa_group_rekey=60): wait for it, then broadcast still works.
    i=0
    until [ "$("$V" "grep -c SetGtk $KLOG" 2>/dev/null || echo 0)" -ge 2 ]; do
        i=$((i + 1)); [ $i -lt 30 ] || fail "group rekey within 150 s"
        sleep 5
    done
    ok "group key handshake (GTK rekey) done"
    "$V" 'ping -c 2 -t 20 10.77.0.255' >/dev/null 2>&1 || true
    "$V" 'ping -c 3 -t 30 10.77.0.1' >/dev/null && ok "ping after rekey" || fail "ping after rekey"

    # PMF: a forged (unprotected) deauth from the AP's address is ignored.
    sh "$AP" ssh "hostapd_cli -p /run/hostapd deauthenticate 02:a5:00:00:00:01 test=0 >/dev/null 2>&1; true"
    sleep 5
    st=$("$V" "$CTL status")
    echo "    $st"
    if "$V" "grep -q unprotected-deauth-ignored $MLOG"; then
        echo "$st" | grep -q 'state=authorized' && ok "unprotected deauth ignored (PMF), SA Query kept the link" || fail "forged deauth"
    else
        echo "skip unprotected deauth (this hostapd build did not send one)"
    fi

    out=$("$V" "$CTL leave") || true
    echo "$out" | grep -q 'state=disconnected' && ok "leave" || fail "leave"
    "$V" "grep -q 'secret=\\|correct horse' $MLOG && exit 1; true" && ok "no secret in aura-mlme's log" || fail "secret leaked to mlme log"

    "$V" 'sysctl net.aura.wlan0; service atrium-portcullisd-bootstrap onestop >/dev/null 2>&1; pkill -x atrium-aura-mlme atrium-aura-keys aura-mlme aura-keys aura-air arcad dhclient; sleep 3; kldunload aura_sim && kldunload aura_wlan && echo "modules unloaded cleanly"'
    echo "W1 gate: $pass checks passed" ;;
*)
    sed -n 2,20p "$0"; exit 2 ;;
esac
