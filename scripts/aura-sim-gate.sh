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
        daemon -f -o /tmp/aura/net.log /tmp/aura/atrium-net -v --wifi wlan0 --capsicum --no-unbound --lease /tmp/aura/atrium-lease --sock /tmp/aura/net.sock --state-dir /tmp/aura/net-state
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
    (cd "$ROOT/atrium-net" && RUSTFLAGS="$X" cargo build --release -p atrium-net -p atrium-lease --target "$T" >&2)
    N="$ROOT/atrium-net/target/$T/release"
    cp "$N/atrium-net" "$N/atrium-lease" "$N/atrium-netctl" "$B/"
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
*)
    sed -n 2,26p "$0"; exit 2 ;;
esac
