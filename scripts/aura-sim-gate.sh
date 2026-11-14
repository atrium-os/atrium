#!/bin/sh
# Aura's simulation gates (docs/spec/wifi.md §6.2, §7): the FreeBSD VM
# against real hostapd.
#
#   Linux VM (scripts/aura-ap-vm.sh): hostapd on mac80211_hwsim radios, the
#     medium air-hwsim.py, udhcpd on br0 (10.77.0.0/24).
#   FreeBSD VM: aura_wlan.ko + aura_sim.ko; aura-air (the radio's firmware,
#     test only) on UDP 7701; arcad; aura-keys and aura-mlme jailed;
#     atrium-net managing wlan0 (Wi-Fi profiles, DHCP through its sandboxed
#     atrium-lease), driven by atrium-netctl.
#
#   sh scripts/aura-sim-gate.sh vms   boot both VMs (FreeBSD with the UDP
#                                     forward; fork kernel via nextboot)
#   sh scripts/aura-sim-gate.sh w1    WPA2-PSK + PMF: join, DHCP, pings,
#                                     GTK rekey, forged deauth, jails
#   sh scripts/aura-sim-gate.sh w2    WPA3-SAE (H2E and hunting-and-pecking,
#                                     anti-clogging tokens), transition mode
#                                     and Transition Disable, per-network
#                                     MAC, roaming between two BSSs
#   sh scripts/aura-sim-gate.sh n3    atrium-net N3: Ethernet (vtnet1 on
#                                     atrium-net/sim/lan.py) and Wi-Fi together:
#                                     cable pulls and flaps, dead upstream,
#                                     captive portal (docs/spec/atrium-net.md)
#
# AURA_JAIL=0 runs the daemons unjailed; AURA_ARCA=0 uses aura-keys'
# memory store. The FreeBSD VM must have been started with
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
PASS='correct horse battery staple'

# The address a join used (from its "authenticating ... mac=" line).
mac_of() {
    printf '%s\n' "$1" | sed -n 's/.*state=authenticating.* mac=\([0-9a-f:]*\).*/\1/p' | head -n 1
}

# join SSID ARGS...: prints the join's lines; fails the gate unless authorized.
join() {
    ssid=$1; shift
    out=$("$V" "$CTL join $ssid --wait 240 $*" 2>&1) || fail "join $*: $out"
    echo "$out" | sed 's/^/    /'
    echo "$out" | grep -q 'state=authorized' || fail "join $*: not authorized"
}

leave() {
    out=$("$V" "$CTL leave") || true
    echo "$out" | grep -q 'state=disconnected' || fail "leave"
}

# atrium-net on wlan0 (state in /tmp/aura/net-state, so profiles persist
# across its restarts within one run).
start_net() {
    "$V" "set -e
        pkill -x atrium-net 2>/dev/null || true; sleep 1
        id _atrium-lease >/dev/null 2>&1 || pw useradd _atrium-lease -u 65034 -d /nonexistent -s /usr/sbin/nologin -c 'atrium-net lease parser'
        mkdir -p /var/empty /tmp/aura/net-state
        daemon -f -o /tmp/aura/net.log /tmp/aura/atrium-net -v ${NET_ARGS:---wifi wlan0 --no-probe} --capsicum --no-unbound --lease /tmp/aura/atrium-lease --sock /tmp/aura/net.sock --state-dir /tmp/aura/net-state
        sleep 2"
}

setup() {
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
    # atrium-net is the network manager here (Wi-Fi policy, DHCP through its
    # sandboxed atrium-lease); atrium-netctl is the command line.
    (cd "$ROOT/atrium-net" && RUSTFLAGS="$X" cargo build --release -p atrium-net -p atrium-lease -p atrium-netconf --target "$T" >&2)
    N="$ROOT/atrium-net/target/$T/release"
    cp "$N/atrium-net" "$N/atrium-lease" "$N/atrium-probe" "$N/atrium-netctl" "$N/atrium-netconf" "$B/"
    tar czf - -C "$(dirname "$B")" aura | "$V" 'rm -rf /tmp/aura && tar xzf - -C /tmp'
    rm -rf "$(dirname "$B")"

    sh "$AP" ap "$@"

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
    start_net
    CTL="/tmp/aura/atrium-netctl --sock /tmp/aura/net.sock"
    pass=0
    ok() { echo "ok   $1"; pass=$((pass + 1)); }
    fail() {
        echo "FAIL $1"
        "$V" "for f in /tmp/aura/air.log $KLOG $MLOG /tmp/aura/net.log; do echo \"--- \$f\"; tail -n 40 \$f; done" || true
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

}

w1() {
    out=$("$V" "$CTL scan" 2>&1) || fail "scan: $out"
    echo "$out" | sed 's/^/    /'
    echo "$out" | grep -q 'ssid="aura-test".*security=psk-sha256' && ok "scan finds aura-test (WPA2, PMF required, PSK-SHA256)" || fail "scan result"

    r=$("$V" "echo 'correct horse battery staple' | $CTL remember aura-test --manual") || fail "remember: $r"
    ok "credential remembered ($([ "$ARCA" = 1 ] && echo "Arca machine store" || echo "memory store"))"

    out=$("$V" "$CTL join aura-test --wait 180 --dhcp wlan0" 2>&1) || fail "join: $out"
    echo "$out" | sed 's/^/    /'
    MAC=$(mac_of "$out")
    [ -n "$MAC" ] && [ "$MAC" != 02:a5:00:00:00:01 ] && ok "per-network address $MAC, not the radio's" || fail "per-network address: '$MAC'"
    "$V" "ifconfig wlan0 ether | grep -q 'ether $MAC'" && ok "wlan0 took the address" || fail "wlan0 ether"
    
    echo "$out" | grep -q 'state=associated.*pmf=1' && ok "associated with PMF" || fail "associated with PMF"
    echo "$out" | grep -q 'state=authorized' && ok "4-way handshake done, port authorized" || fail "authorized"
    echo "$out" | grep -q 'address inet 10\.77\.0\.1[0-2][0-9]' && ok "DHCP address from udhcpd" || fail "DHCP address"

    if [ "$ARCA" = 1 ]; then
        "$V" "grep -q 'service:aura-keys op=0x0005 .* status=0' /tmp/aura/arca/audit.log" && ok "Arca released the credential to aura-keys (audit log)" || fail "Arca release"
    fi
    sh "$AP" ssh "grep -q 'AP-STA-CONNECTED $MAC' /tmp/hostapd.log" && ok "hostapd: AP-STA-CONNECTED" || fail "hostapd connected"
    sh "$AP" ssh "grep -q 'EAPOL-4WAY-HS-COMPLETED $MAC' /tmp/hostapd.log" && ok "hostapd: EAPOL-4WAY-HS-COMPLETED" || fail "hostapd 4-way"

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
    sh "$AP" ssh "hostapd_cli -p /run/hostapd deauthenticate $MAC test=0 >/dev/null 2>&1; true"
    sleep 5
    st=$("$V" "$CTL wifi-status")
    echo "    $st"
    if "$V" "grep -q unprotected-deauth-ignored $MLOG"; then
        echo "$st" | grep -q 'state=authorized' && ok "unprotected deauth ignored (PMF), SA Query kept the link" || fail "forged deauth"
    else
        echo "skip unprotected deauth (this hostapd build did not send one)"
    fi

    out=$("$V" "$CTL leave") || true
    echo "$out" | grep -q 'state=disconnected' && ok "leave" || fail "leave"
    "$V" "grep -q 'secret=\\|correct horse' $MLOG && exit 1; true" && ok "no secret in aura-mlme's log" || fail "secret leaked to mlme log"

}

w2() {
    r=$("$V" "echo '$PASS' | $CTL remember aura-test --manual") || fail "remember: $r"
    ok "credential remembered"

    # 1. WPA3 only, H2E only, anti-clogging token on every commit.
    sh "$AP" ap hostapd-wpa3.conf
    out=$("$V" "$CTL scan" 2>&1) || fail "scan: $out"
    echo "$out" | grep 'aura-test' | sed 's/^/    /'
    echo "$out" | grep -q 'ssid="aura-test".*security=sae$' && ok "scan: aura-test is WPA3-SAE" || fail "scan security"
    join aura-test --dhcp wlan0
    MAC=$(mac_of "$out")
    st=$("$V" "$CTL wifi-status"); echo "    $st"
    echo "$st" | grep -q 'using=sae-h2e' && ok "SAE with hash-to-element" || fail "using H2E"
    "$V" "grep -q 'anti-clogging token requested' $KLOG" && ok "anti-clogging token: asked, commit sent again" || fail "token path"
    sh "$AP" ssh "grep -q 'AP-STA-CONNECTED $MAC' /tmp/hostapd.log && grep -q 'EAPOL-4WAY-HS-COMPLETED $MAC' /tmp/hostapd.log" && ok "hostapd: SAE station connected, 4-way done" || fail "hostapd SAE connect"
    echo "$out" | grep -q 'address inet 10\.77\.0\.1[0-2][0-9]' && ok "DHCP address" || fail "DHCP"
    "$V" 'ping -c 3 -t 30 10.77.0.1' >/dev/null && ok "ping over WPA3" || fail "ping (wpa3)"
    # Regulatory: our country DE; the AP's Country element (US) narrows it.
    r=$("$V" "$CTL country DE") || fail "country: $r"
    echo "    $r"
    st=$("$V" "$CTL wifi-status"); echo "    $st"
    echo "$st" | grep -q 'country=DE narrowed=1' && ok "country DE set; the AP's US Country element narrowed the rules" || fail "regulatory narrowing"
    "$V" 'ping -c 2 -t 20 10.77.0.1' >/dev/null && ok "still connected on channel 6 under DE and US" || fail "ping after country"
    leave

    # 2. Hunting-and-pecking (no H2E offered).
    sh "$AP" ap hostapd-wpa3-hnp.conf
    "$V" "killall -q dhclient; true"
    join aura-test --dhcp wlan0
    st=$("$V" "$CTL wifi-status"); echo "    $st"
    echo "$st" | grep -q 'using=sae ' && ok "SAE with hunting-and-pecking" || fail "using H&P"
    "$V" "grep -q 'hunting-and-pecking' $KLOG" || fail "H&P in keys log"
    "$V" 'ping -c 3 -t 30 10.77.0.1' >/dev/null && ok "ping (hunting-and-pecking)" || fail "ping (hnp)"
    leave

    # 3. Transition mode: SAE chosen over PSK; Transition Disable honoured.
    sh "$AP" ap hostapd-transition.conf
    out=$("$V" "$CTL scan" 2>&1) || fail "scan: $out"
    echo "$out" | grep -q 'ssid="aura-test".*security=sae-transition' && ok "scan: transition mode seen" || fail "scan transition"
    "$V" "killall -q dhclient; true"
    join aura-test
    echo "$out" | grep -q 'transition_disabled=1' && ok "WPA3 chosen; the AP disabled transition (KDE)" || fail "transition disable"
    leave

    # 4. The same network offered as WPA2 only afterwards: refused.
    sh "$AP" ap hostapd-wpa2.conf
    out=$("$V" "$CTL join aura-test --wait 120" 2>&1) && fail "joined WPA2 after transition disable: $out"
    echo "$out" | tail -n 2 | sed 's/^/    /'
    echo "$out" | grep -q 'why=security-mismatch' && ok "WPA2-only BSS refused after Transition Disable" || fail "downgrade refused"

    # 5. Roaming between two BSSs of one WPA3 network (channels 1 and 6).
    sh "$AP" ap hostapd-roam-a.conf hostapd-roam-b.conf
    sh "$AP" signal 2412 -40 2437 -75
    BA=$(sh "$AP" ssh "cat /sys/class/net/wlan0/address")
    BB=$(sh "$AP" ssh "cat /sys/class/net/wlan1/address")
    "$V" "killall -q dhclient; true"
    join aura-test --dhcp wlan0
    echo "$out" | grep -q "state=authorized bssid=$BA" && ok "joined the stronger BSS ($BA, channel 1)" || fail "first BSS"
    "$V" "rm -f /tmp/aura/watch.log; daemon -f -o /tmp/aura/watch.log $CTL watch 150"
    sleep 2
    sh "$AP" signal 2412 -85 2437 -40
    i=0
    until "$V" "grep -q 'state=authorized bssid=$BB' /tmp/aura/watch.log"; do
        i=$((i + 1)); [ $i -lt 30 ] || { "$V" "cat /tmp/aura/watch.log"; fail "roam within 150 s"; }
        sleep 5
    done
    "$V" "cat /tmp/aura/watch.log" | sed 's/^/    /'
    "$V" "grep -q 'what=roam ' /tmp/aura/watch.log" && ok "roamed to $BB (channel 6) when $BA faded" || fail "roam event"
    sh "$AP" ssh "grep -q 'wlan1: AP-STA-CONNECTED $MAC' /tmp/hostapd.log" && ok "hostapd: same address on the second BSS" || fail "hostapd roam"
    "$V" 'ping -c 3 -t 30 10.77.0.1' >/dev/null && ok "ping after the roam (same address, same LAN)" || fail "ping after roam"
    A1=$("$V" "$CTL status" | sed -n 's/^iface name=wlan0 .*addr=\([0-9.]*\).*/\1/p')
    leave

    # atrium-net (N1): what it keeps and does on its own.
    # Transition Disable from scenario 3 survives a restart of atrium-net.
    start_net
    l=$("$V" "$CTL list"); echo "$l" | sed 's/^/    /'
    echo "$l" | grep -q 'wpa3_only=1' && ok "WPA3-only flag (Transition Disable) persisted across an atrium-net restart" || fail "wpa3_only persisted"
    # Auto-join: remembered for auto-join, nothing else asked; the best BSS of
    # the two is joined and DHCP runs by itself.
    sh "$AP" signal 2412 -40 2437 -75
    r=$("$V" "echo '$PASS' | $CTL remember aura-test") || fail "remember (auto): $r"
    i=0
    until "$V" "$CTL status" | grep -q '^iface name=wlan0 .*state=bound'; do
        i=$((i + 1)); [ $i -lt 40 ] || { "$V" "$CTL status; tail -n 30 /tmp/aura/net.log"; fail "auto-join within 200 s"; }
        sleep 5
    done
    st=$("$V" "$CTL status"); echo "$st" | sed 's/^/    /'
    A2=$(echo "$st" | sed -n 's/^iface name=wlan0 .*addr=\([0-9.]*\).*/\1/p')
    ok "auto-joined aura-test and got $A2 by DHCP without a join command"
    [ -n "$A1" ] && [ "$A1" = "$A2" ] && ok "same address as before ($A1): the lease was confirmed, not renegotiated" || echo "    note: address $A1 then $A2"
    "$V" 'ping -c 3 -t 30 10.77.0.1' >/dev/null && ok "ping after auto-join" || fail "ping after auto-join"
    # Driving into a tunnel: both BSSs out of range for 20 s. aura-mlme
    # notices the missing beacons, atrium-net keeps the address through the
    # drop, reconnects fast and confirms the same lease.
    "$V" "rm -f /tmp/aura/watch.log; daemon -f -o /tmp/aura/watch.log $CTL watch 400"
    sleep 2
    # The AP VM restores the signal 20 s after the cut on its own clock: the
    # checks below go through slow ssh and must not stretch the drop.
    sh "$AP" signal 2412 -40 2437 -75
    sh "$AP" blackout 20
    t0=$(date +%s)
    i=0
    until "$V" "grep -q 'beacon-loss' /tmp/aura/watch.log"; do
        i=$((i + 1)); [ $i -lt 10 ] || { "$V" "cat /tmp/aura/watch.log"; fail "beacon loss within 30 s"; }
        sleep 3
    done
    ok "tunnel: link loss noticed from missing beacons"
    "$V" "ifconfig wlan0 inet | grep -q 'inet $A2 '" && ok "tunnel: address kept while the link is gone" || fail "address dropped at once"
    "$V" "$CTL status" | grep -q '^iface name=wlan0 .*link=lost' && ok "tunnel: status says link=lost" || fail "status link=lost"
    i=0
    until "$V" "$CTL status" | grep '^iface name=wlan0 .*state=bound' | grep -qv 'link=lost'; do
        i=$((i + 1)); [ $i -lt 40 ] || { "$V" "$CTL status; cat /tmp/aura/watch.log; tail -n 30 /tmp/aura/net.log"; fail "reconnect after the tunnel"; }
        sleep 3
    done
    ok "tunnel: reconnected, seen $(( $(date +%s) - t0 )) s after a 20 s drop began"
    "$V" "grep -q 'lease-lost' /tmp/aura/watch.log" && fail "lease dropped during a 20 s drop" || ok "tunnel: no lease-lost event during the drop"
    "$V" "ifconfig wlan0 inet | grep -q 'inet $A2 '" && ok "tunnel: same address ($A2) after the drop" || fail "address changed after the tunnel"
    "$V" 'ping -c 3 -t 30 10.77.0.1' >/dev/null && ok "ping after the tunnel" || fail "ping after tunnel"
    # A long tunnel: past the 60 s grace the address and routes go.
    sh "$AP" signal 2412 -100 2437 -100
    i=0
    until "$V" "grep -q 'lease-lost.*why=link-down' /tmp/aura/watch.log"; do
        i=$((i + 1)); [ $i -lt 30 ] || { "$V" "cat /tmp/aura/watch.log"; fail "address dropped within 90 s of a long drop"; }
        sleep 3
    done
    ok "long drop: address dropped after the grace period ($((i * 3)) s)"
    a=$("$V" "ifconfig wlan0 inet | grep -c 'inet ' || true")
    [ "$a" = 0 ] && ok "long drop: wlan0 has no IPv4 address" || fail "address kept after the grace"
    sh "$AP" signal 2412 -40 2437 -75
    i=0
    until "$V" "$CTL status" | grep -q '^iface name=wlan0 .*state=bound'; do
        i=$((i + 1)); [ $i -lt 40 ] || { "$V" "$CTL status; tail -n 30 /tmp/aura/net.log"; fail "rejoin after a long drop"; }
        sleep 3
    done
    "$V" "ifconfig wlan0 inet | grep -q 'inet $A2 '" && ok "long drop: rejoined, same address back ($A2)" || fail "address after the long drop"
    "$V" "pkill -f 'atrium-netctl.*watch' || true"
    # Leaving drops the address (no routes through a dead link) and holds off auto-join.
    leave
    sleep 3
    a=$("$V" "ifconfig wlan0 inet | grep -c 'inet ' || true")
    [ "$a" = 0 ] && ok "left: wlan0 has no IPv4 address" || fail "address kept after leave"
    sleep 10
    "$V" "$CTL status" | grep -q '^wifi state=idle' && ok "no auto-join after the user left" || fail "auto-joined after leave"
    "$V" "grep -q 'secret=\\|correct horse' $MLOG /tmp/aura/net.log && exit 1; true" && ok "no secret in aura-mlme's or atrium-net's log" || fail "secret leaked to a log"
}

# The QEMU monitor of the FreeBSD VM: `lan on|off` is the Ethernet cable.
cable() {
    python3 -I -c "import socket,sys,time;s=socket.socket(socket.AF_UNIX);s.connect(sys.argv[1]);s.sendall(b'set_link lan '+sys.argv[2].encode()+b'\n');time.sleep(0.5)" "$HOME/atrium-vm/mon.sock" "$1"
}

lanctl() {
    python3 -I -c "import socket,sys;s=socket.socket(socket.AF_INET,socket.SOCK_DGRAM);s.settimeout(5);s.sendto(sys.argv[1].encode(),('127.0.0.1',7803));print(s.recv(4096).decode())" "$1"
}

n3_cleanup() {
    cable on 2>/dev/null || true
    "$V" 'pkill -x atrium-net; pkill -f "nc -lkU /tmp/netd"; sleep 1
        for a in $(ifconfig vtnet1 inet | awk "/inet /{print \$2}"); do ifconfig vtnet1 inet $a -alias; done
        ifconfig vtnet1 inet6 -accept_rtadv
        for a in $(ifconfig vtnet1 inet6 | awk "/inet6 fd77:/{print \$2}"); do ifconfig vtnet1 inet6 $a -alias; done
        ndp -i vtnet1 -- -no_radr >/dev/null 2>&1; ndp -i wlan0 -- -no_radr >/dev/null 2>&1
        g=$(cat /tmp/aura/default.saved 2>/dev/null); [ -n "$g" ] && { while route -q delete default 2>/dev/null; do :; done; route -q add default $g; }
        true' || true
    sh "$AP" ssh "echo ok > /tmp/inet-mode" >/dev/null 2>&1 || true
}

# n3: Ethernet and Wi-Fi together (docs/spec/atrium-net.md §4.1, §4.2, §10).
# "The internet" sits beyond each network's router: lan.py answers for
# 198.51.100.80 behind 10.88.0.1, inet.py in the AP VM for 192.0.2.80 (on
# its lo) behind 10.77.0.1, and each network's DNS answers probe.atrium.test
# with its own one; so a probe that leaves by the wrong interface fails.
n3() {
    gw() { "$V" 'route -n get default 2>/dev/null | awk "/gateway/{print \$2}"'; }
    wait_gw() {
        i=0; while [ $i -lt "$2" ]; do [ "$(gw)" = "$1" ] && return 0; sleep 2; i=$((i + 2)); done; return 1
    }
    st() { "$V" "$CTL status"; }
    # wait_st PATTERN SECS: until a status line matches (extended regex).
    wait_st() {
        i=0; while [ $i -lt "$2" ]; do st | grep -Eq "$1" && return 0; sleep 3; i=$((i + 3)); done; return 1
    }
    # Seconds (atrium-net's clock) of the last log line matching PATTERN.
    t_of() { "$V" "grep -E '$1' /tmp/aura/net.log | tail -n 1" | sed -n 's/^atrium-net: \[\([0-9.]*\)\].*/\1/p'; }
    hold_of() { "$V" "grep -E 'egress $1 ' /tmp/aura/net.log | tail -n 1" | sed -n 's/.*hold-down \([0-9]*\) s.*/\1/p'; }
    gap() { awk -v a="$1" -v b="$2" 'BEGIN{printf "%.1f", b - a}'; }

    pkill -f atrium-net/sim/lan.py 2>/dev/null || true
    python3 "$ROOT/atrium-net/sim/lan.py" 600 > "$HOME/atrium-vm/lan.log" 2>&1 &
    LANPID=$!
    trap 'n3_cleanup; kill $LANPID 2>/dev/null || true' EXIT
    sh "$AP" ssh "ip addr add 192.0.2.80/32 dev lo 2>/dev/null; echo ok > /tmp/inet-mode; pkill -f '[i]net.py'; (nohup python3 /tmp/inet.py >/tmp/inet.log 2>&1 &); sleep 2; true"
    "$V" "set -e
        ifconfig vtnet1 >/dev/null 2>&1 || { echo 'no vtnet1: boot with the vms step'; exit 1; }
        id _atrium-probe >/dev/null 2>&1 || pw useradd _atrium-probe -u 65035 -d /nonexistent -s /usr/sbin/nologin -c 'atrium-net connectivity probe'
        pkill -f 'nc -lkU /tmp/netd' 2>/dev/null || true
        rm -rf /tmp/netd && mkdir -p /tmp/netd
        daemon -f -o /tmp/netd/events nc -lkU /tmp/netd/ctl.sock
        g=\$(route -n get default 2>/dev/null | awk '/gateway/{print \$2}'); case \$g in 10.0.2.*) echo \$g > /tmp/aura/default.saved ;; esac
        for a in \$(ifconfig vtnet1 inet | awk '/inet /{print \$2}'); do ifconfig vtnet1 inet \$a -alias; done
        ifconfig vtnet1 inet6 -ifdisabled up"
    cable on
    NET_ARGS="--iface vtnet1 --wifi wlan0 --probe http://probe.atrium.test/generate_204 --probe-every 20 --probe-bin /tmp/aura/atrium-probe --netd-dir /tmp/netd" start_net
    r=$("$V" "echo '$PASS' | $CTL remember aura-test --manual") || fail "remember: $r"
    out=$("$V" "$CTL join aura-test --wait 240 --dhcp wlan0" 2>&1) || fail "join: $out"
    echo "$out" | grep -q 'address inet 10\.77\.0\.' || fail "no Wi-Fi address: $out"
    wait_st 'name=vtnet1 .*state=bound' 120 || fail "vtnet1 not bound"
    ok "both up: vtnet1 bound on the LAN, wlan0 joined aura-test"

    wait_gw 10.88.0.1 30 && ok "Ethernet carries the default route (10.88.0.1) with Wi-Fi up too" || fail "default route: $(gw)"
    wait_st 'name=vtnet1 .*internet=yes' 120 && wait_st 'name=wlan0 .*internet=yes' 120 \
        && ok "probe through each interface: internet on both" || fail "probes: $(st)"
    sh "$AP" ssh "grep -q 'http 10.77.0.1[0-2][0-9] /generate_204' /tmp/inet.log" \
        && ok "Wi-Fi's probe reached 192.0.2.80 through 10.77.0.1 while Ethernet had the default route (pinned host route)" || fail "AP inet.log: $(sh "$AP" ssh 'tail -5 /tmp/inet.log')"
    [ "$(lanctl stats | python3 -c 'import json,sys; print(json.load(sys.stdin).get("web_get", 0))')" -ge 1 ] && ok "Ethernet's probe reached 198.51.100.80 through 10.88.0.1" || fail "lan.py got no probe"
    "$V" 'route -n get 192.0.2.80 | grep -q "gateway: 10.88.0.1"' && ok "probe's host route removed afterwards" || fail "pinned route left: $("$V" 'route -n get 192.0.2.80')"
    "$V" 'grep -q "egress-changed iface=vtnet1" /tmp/netd/events' && ok "atrium-netd told: egress-changed iface=vtnet1" || fail "netd events: $("$V" 'cat /tmp/netd/events')"
    i=0; while [ $i -lt 30 ] && ! "$V" 'ndp -rn' | grep -q '^fe80::1%vtnet1 '; do sleep 3; i=$((i + 3)); done
    "$V" 'ndp -rn' | grep -q '^fe80::1%vtnet1 ' && ok "IPv6 default router from Ethernet's RA (fe80::1%vtnet1)" || fail "ndp -rn: $("$V" 'ndp -rn')"

    # Pull the cable: Wi-Fi at once.
    cable off
    wait_gw 10.77.0.1 30 && ok "cable pulled: default route moved to Wi-Fi (10.77.0.1)" || fail "after cable pull: $(gw)"
    nd=$("$V" 'netstat -rn4 | grep -c "^default"')
    [ "$nd" = 1 ] && ok "one default route, not a second path beside the old one" || fail "default routes: $("$V" 'netstat -rn4 | grep "^default"')"
    t_down=$(t_of 'vtnet1: link down'); t_eg=$(t_of 'egress wlan0')
    echo "    link down at ${t_down}s, egress wlan0 at ${t_eg}s"
    if "$V" 'nc -z -w 20 192.0.2.80 80' >/dev/null 2>&1; then
        ok "the internet through Wi-Fi (TCP to 192.0.2.80:80)"
    else
        sh "$AP" ssh "(nohup timeout 40 tcpdump -c 12 -ni br0 host 192.0.2.80 or arp > /tmp/aptd.txt 2>&1 &); sleep 2" >/dev/null 2>&1
        d=$("$V" '(tcpdump -c 8 -ni wlan0 not port 7701 > /tmp/td.txt 2>&1 &); sleep 1; nc -z -w 8 192.0.2.80 80; nc -z -w 8 10.77.0.1 80; sleep 1; cat /tmp/td.txt; netstat -rn4; sysctl net.aura.wlan0; tail -n 20 /tmp/aura/net.log' 2>&1)
        a=$(sh "$AP" ssh 'sleep 5; cat /tmp/aptd.txt; ip route; ip -4 addr; tail -5 /tmp/inet.log' 2>&1)
        fail "no internet over Wi-Fi:
$d
--- AP:
$a"
    fi
    "$V" 'grep -q "egress-changed iface=wlan0" /tmp/netd/events' && ok "atrium-netd told: egress-changed iface=wlan0" || fail "netd events: $("$V" 'cat /tmp/netd/events')"
    "$V" 'ndp -rn' | grep -q '%vtnet1' && fail "IPv6 router through the pulled cable: $("$V" 'ndp -rn')" || ok "no IPv6 default router through the pulled cable"

    # Cable back: the route returns only after the hold-down.
    cable on
    wait_gw 10.88.0.1 90 || fail "route did not return to Ethernet: $(gw)"
    t_up=$(t_of 'vtnet1: link up'); t_eg=$(t_of 'egress vtnet1'); h=$(hold_of vtnet1)
    g=$(gap "$t_up" "$t_eg")
    awk -v g="$g" -v h="$h" 'BEGIN{exit !(h >= 5 && g >= h)}' && ok "cable back: route returned to Ethernet ${g} s after the link (hold-down $h s)" || fail "cable back: gap $g s, hold $h"

    # A flapping cable: each flap within 2 min doubles the wait.
    for n in 1 2; do cable off; wait_gw 10.77.0.1 30 || fail "flap $n: $(gw)"; cable on; sleep 2; done
    wait_gw 10.88.0.1 180 || fail "route never came back after flapping: $(gw)"
    t_up=$(t_of 'vtnet1: link up'); t_eg=$(t_of 'egress vtnet1'); h=$(hold_of vtnet1)
    g=$(gap "$t_up" "$t_eg")
    awk -v g="$g" -v h="$h" 'BEGIN{exit !(h >= 10 && g >= h)}' && ok "flapping cable: hold-down doubled to $h s (returned after $g s)" || fail "flapping: gap $g s, hold $h"

    # Ethernet's upstream dies (link fine): Wi-Fi still has internet, so the route moves.
    lanctl web-dead >/dev/null
    wait_st 'name=vtnet1 .*internet=no' 90 || fail "dead upstream not seen: $(st)"
    wait_gw 10.77.0.1 30 && ok "Ethernet's internet dead (link up): route moved to Wi-Fi, which still has internet" || fail "dead upstream: $(gw)"
    lanctl web-ok >/dev/null
    wait_st 'name=vtnet1 .*internet=yes' 90 && ok "Ethernet probed again through a pinned route while Wi-Fi had the default: internet back" || fail "vtnet1 internet: $(st)"
    wait_gw 10.88.0.1 120 || fail "route did not return after the upstream came back: $(gw)"
    t_ok=$(t_of 'vtnet1: internet yes'); t_eg=$(t_of 'egress vtnet1'); h=$(hold_of vtnet1)
    g=$(gap "$t_ok" "$t_eg")
    awk -v g="$g" -v h="$h" 'BEGIN{exit !(g >= h)}' && ok "route back on Ethernet ${g} s after its internet returned (hold-down $h s)" || fail "upstream back: gap $g, hold $h"

    # A captive portal on the Wi-Fi network.
    sh "$AP" ssh "echo captive > /tmp/inet-mode"
    portal=$(printf 'http://10.77.0.1/portal' | od -An -tx1 | tr -d ' \n')
    wait_st "name=wlan0 .*internet=captive.*portal=$portal" 90 && ok "captive portal on Wi-Fi detected, sign-in page reported (http://10.77.0.1/portal)" || fail "captive: $(st)"
    [ "$(gw)" = 10.88.0.1 ] && ok "route stays on Ethernet" || fail "captive moved the route: $(gw)"
    sh "$AP" ssh "echo ok > /tmp/inet-mode"

    # The probe's sandbox.
    p=$("$V" 'pgrep -x atrium-probe')
    [ "$("$V" "ps -o user= -p $p")" = _atrium-probe ] && ok "atrium-probe runs as _atrium-probe" || fail "probe uid"
    "$V" "procstat -f $p | awk '\$3==\"root\"{print \$NF}'" | grep -q '^/var/empty$' && ok "atrium-probe is chrooted to /var/empty" || fail "probe root: $("$V" "procstat -f $p")"
    "$V" "procstat -l $p | awk '/openfiles/{print \$4}'" | grep -qx 8 && ok "atrium-probe limited to 8 descriptors, no children" || fail "probe limits: $("$V" "procstat -l $p")"
    "$V" "procstat -s \$(pgrep -x atrium-net) | awk 'NR==2{print \$10}'" | grep -q C && ok "atrium-net still in capability mode" || fail "atrium-net capmode"

    # One network left, its upstream dead: reported, the route kept.
    leave
    lanctl web-dead >/dev/null
    wait_st 'name=vtnet1 .*internet=no' 90 || fail "dead upstream not seen: $(st)"
    [ "$(gw)" = 10.88.0.1 ] && ok "only Ethernet left, no internet: reported (internet=no), route kept" || fail "lone interface lost its route: $(gw)"
    lanctl web-ok >/dev/null

    weak_move
}

# Weak signal: move to another remembered network (spec §4.1), only when
# ours is below -80 dBm for 30 s and the other above -70 on two scans.
weak_move() {
    st() { "$V" "$CTL status"; }
    wait_st() {
        i=0; while [ $i -lt "$2" ]; do st | grep -Eq "$1" && return 0; sleep 3; i=$((i + 3)); done; return 1
    }
    sh "$AP" ap hostapd-wpa2.conf hostapd-other.conf
    other="\"aura-other\""
    sh "$AP" signal 2437 -75 2412 -50
    out=$("$V" "$CTL join aura-test --wait 240 --dhcp wlan0" 2>&1) || fail "join: $out"
    # Remembered (auto-join on) once connected, so auto-join does not pick it.
    r=$("$V" "echo '$PASS' | $CTL remember aura-other") || fail "remember aura-other: $r"
    t0=$(date +%s)
    while [ $(( $(date +%s) - t0 )) -lt 90 ]; do
        st | grep -qF "ssid=$other" && fail "moved at -75 dBm: $(st)"
        sleep 10
    done
    ok "at -75 dBm (weak, not bad) Wi-Fi stays on aura-test with aura-other at -50 for 90 s"
    sh "$AP" signal 2437 -85 2412 -50
    t1=$(date +%s)
    wait_st "^wifi .*state=up.*ssid=\"aura-other\"" 240 || fail "no move to aura-other: $(st; "$V" "grep -Ev 'probing|probe ' /tmp/aura/net.log | tail -n 40")"
    "$V" "grep -q 'weak signal, moving to aura-other' /tmp/aura/net.log" || fail "aura-other joined, but not by the weak-signal move: $("$V" 'tail -n 30 /tmp/aura/net.log')"
    wait_st 'name=wlan0 .*state=bound' 120 && ok "at -85 dBm Wi-Fi moved to aura-other and took a lease there" || fail "no lease after the move: $(st)"
    echo "    moved $(( $(date +%s) - t1 )) s after the signal dropped (at least 30 s weak + two scans, plus ssh time under TCG)"
    sh "$AP" ssh "grep -q 'wlan1: AP-STA-CONNECTED' /tmp/hostapd.log" && ok "hostapd: associated with aura-other's BSS" || fail "hostapd aura-other"
    sh "$AP" signal 2437 -50 2412 -50
    leave
    "$V" "$CTL forget aura-other" >/dev/null
}

# N4 (atrium-net.md §7.1): apps reach atrium-net only through atrium-netconf,
# one socket per capability; each socket allows its own requests only.
n4() {
    "$V" "set -e
        id _atrium-netconf >/dev/null 2>&1 || pw useradd _atrium-netconf -u 65036 -d /nonexistent -s /usr/sbin/nologin -c 'atrium-net settings front'
        pkill -x atrium-netconf 2>/dev/null || true; sleep 1
        rm -rf /tmp/aura/sockets
        daemon -f -o /tmp/aura/netconf.log /tmp/aura/atrium-netconf -v --ctl /tmp/aura/net.sock --dir /tmp/aura/sockets --uid 65036 --capsicum
        sleep 2"
    APP="/tmp/aura/atrium-netctl --dir /tmp/aura/sockets --app"
    # app CAP ARGS...: output, and the exit status in $rc.
    app() { cap=$1; shift; rc=0; out=$("$V" "$APP $cap $*" 2>&1) || rc=$?; }
    refused() { [ "$rc" = 2 ] && echo "$out" | grep -q '^refused: not-allowed'; }

    u=$("$V" 'ps -o user= -p $(pgrep -x atrium-netconf)' | tr -d ' ')
    [ "$u" = _atrium-netconf ] && ok "atrium-netconf runs as _atrium-netconf" || fail "atrium-netconf runs as '$u'"
    "$V" "procstat -s \$(pgrep -x atrium-netconf) | awk 'NR==2{print \$10}'" | grep -q C && ok "atrium-netconf in capability mode" || fail "atrium-netconf capmode"
    s=$("$V" 'ls /tmp/aura/sockets | tr "\n" " "')
    [ "$s" = "network-configure network-details network-read network-wifi wifi-nearby " ] && ok "one socket per capability: $s" || fail "sockets: $s"

    app network-read status
    [ "$rc" = 0 ] && echo "$out" | grep -q '^status link=none online=no' && ok "network-read before any network: $out" || fail "status: $out"
    app network-read join aura-test --open; refused && ok "network-read cannot join" || fail "network-read join: rc=$rc $out"
    app network-read nearby; refused && ok "network-read cannot list nearby networks" || fail "network-read nearby: rc=$rc $out"
    app network-wifi nearby; refused && ok "network-wifi without wifi-nearby gets no list (location)" || fail "network-wifi nearby: rc=$rc $out"
    app network-read interfaces; refused && ok "network-read sees no interfaces" || fail "network-read interfaces: rc=$rc $out"

    app wifi-nearby nearby
    echo "$out" | sed 's/^/    /'
    if [ "$rc" = 0 ] && echo "$out" | grep -q '^nearby ssid="aura-test" security=wpa2 bars=[0-4]'; then
        echo "$out" | grep -Eq '([0-9a-f]{2}:){5}' && fail "a BSSID in the nearby list" || ok "wifi-nearby lists aura-test (WPA2, bars), no BSSIDs"
    else
        fail "nearby: rc=$rc"
    fi

    out=$("$V" "echo '$PASS' | $APP network-wifi join aura-test" 2>&1) && [ "$out" = joined ] && ok "network-wifi joins aura-test with a passphrase" || fail "join: $out"
    i=0; until "$V" "$CTL status" | grep -q '^iface name=wlan0 .*state=bound'; do i=$((i + 1)); [ $i -lt 40 ] || fail "no lease"; sleep 3; done
    app network-read status
    echo "$out" | grep -q '^status link=wifi online=unknown metered=0 bars=[0-4]$' && ok "network-read: on Wi-Fi with bars, no network name ($out)" || fail "read status: $out"
    app network-wifi status
    echo "$out" | grep -q 'ssid="aura-test"' && ok "network-wifi: the network's name too" || fail "wifi status: $out"
    app network-details interfaces
    echo "$out" | grep -q '^iface name=wlan0 wifi=1 link=up addr=10.77.0.[0-9]*/24 router=10.77.0.1 .*egress=1' && ok "network-details: wlan0's address and router" || fail "interfaces: $out"
    app network-details routes
    [ "$out" = "default via 10.77.0.1 on wlan0" ] && ok "network-details: $out" || fail "routes: $out"
    app network-wifi list
    echo "$out" | grep -q '^net ssid="aura-test" open=0 auto=1' && ok "network-wifi lists the remembered network" || fail "list: $out"

    app network-wifi country DE; refused && ok "network-wifi cannot set the country" || fail "wifi country: rc=$rc $out"
    app network-configure country DE
    [ "$rc" = 0 ] && ok "network-configure sets the country" || fail "configure country: rc=$rc $out"
    app network-configure static
    [ "$rc" = 1 ] && echo "$out" | grep -q '^refused: unsupported' && ok "static addresses: unsupported for now" || fail "static: rc=$rc $out"

    # Subscribe: the bar hears leaving at once.
    "$V" "($APP network-read watch 40 > /tmp/aura/watch-n4.log 2>&1 &) ; sleep 4"
    app network-wifi leave
    [ "$out" = left ] || fail "leave: $out"
    sleep 6
    w=$("$V" 'cat /tmp/aura/watch-n4.log')
    echo "$w" | sed 's/^/    /'
    echo "$w" | head -n 1 | grep -q 'link=wifi' && echo "$w" | grep -q 'link=none' && ok "network-read subscriber heard Wi-Fi, then none" || fail "watch: $w"
    echo "$w" | grep -q ssid && fail "a network name reached a network-read subscriber" || ok "no network name in network-read events"

    out=$("$V" "echo 'not the passphrase' | $APP network-wifi join aura-test" 2>&1) || true
    echo "$out" | grep -q '^refused: failed' && ok "wrong passphrase: join refused ($out)" || fail "wrong passphrase: $out"
    out=$("$V" "echo '$PASS' | $APP network-wifi join aura-test" 2>&1) && [ "$out" = joined ] && ok "right passphrase again: joined" || fail "rejoin: $out"
    app network-wifi leave
    app network-wifi forget aura-test
    app network-wifi list
    [ -z "$out" ] && ok "network-wifi forgets it" || fail "after forget: $out"
    "$V" 'pkill -x atrium-netconf' || true
}

teardown() {
    "$V" 'sysctl net.aura.wlan0; service atrium-portcullisd-bootstrap onestop >/dev/null 2>&1; pkill -x atrium-net atrium-aura-mlme atrium-aura-keys aura-mlme aura-keys aura-air arcad dhclient; sleep 3; kldunload aura_sim && kldunload aura_wlan && echo "modules unloaded cleanly"'
}

case "${1:-}" in
vms)
    sh "$AP" up
    if ! VSSH_CONNECT_TIMEOUT=5 "$V" true 2>/dev/null; then
        QEMU_EXTRA="-netdev socket,id=lan,udp=127.0.0.1:7801,localaddr=127.0.0.1:7802 -device virtio-net-pci,netdev=lan,mac=02:4e:00:00:00:01" NETDEV_EXTRA="hostfwd=udp:127.0.0.1:7701-:7701" sh "$ROOT/scripts/cloud-vm.sh" up
    fi
    ARCH=aarch64 KERNCONF="$KERNCONF" sh "$ROOT/scripts/aura-vm-check.sh" kernel ;;
w1 | test)
    setup hostapd-wpa2.conf
    w1
    teardown
    echo "W1 gate: $pass checks passed" ;;
w2)
    setup hostapd-wpa3.conf
    w2
    teardown
    echo "W2 gate: $pass checks passed" ;;
move)
    # The weak-signal move alone (also the end of n3).
    setup hostapd-wpa2.conf
    start_net
    r=$("$V" "echo '$PASS' | $CTL remember aura-test --manual") || fail "remember aura-test: $r"
    weak_move
    teardown
    echo "weak-signal move: $pass checks passed" ;;
n4)
    setup hostapd-wpa2.conf
    n4
    teardown
    echo "atrium-net N4 gate: $pass checks passed" ;;
n3)
    setup hostapd-wpa2.conf
    n3
    n3_cleanup
    teardown
    echo "atrium-net N3 gate: $pass checks passed" ;;
*)
    sed -n 2,26p "$0"; exit 2 ;;
esac
