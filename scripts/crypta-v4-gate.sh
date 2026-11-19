#!/bin/sh
# Crypta V4 gate (docs/spec/vpn.md §11): AnyConnect to ocserv.
#
#   ARCH=amd64 sh scripts/atrium-net-gate.sh vm     the FreeBSD VM, LAN NIC on vtnet1
#   sh scripts/crypta-wg-linux-vm.sh up              the Ubuntu VM
#   sh scripts/crypta-ocserv-vm.sh up                ocserv there, forwarded to 127.0.0.1:8443-8446
#   sh scripts/crypta-v4-gate.sh run                 build, install, check
#
# The V1 stand (its setup is shared). lan.py ends the guest's TCP to
# 198.51.100.52:443-446 and carries it to the host's forwards, and counts
# every packet the guest puts on the LAN. ocserv's four gateways: password
# (with a group to choose), password + TOTP, client certificate, and one
# pushing what Atrium must refuse.
#
# Passwords go the way Settings sends them: typed into Arca's field
# (`arca-probe hold ... vpn`, as arca-ui's uid), a handle to cryptad, the
# value from Arca to the engine's own helper (TAKE_VPN), never cryptad.
set -eu
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
D=${CRYPTA_LINUX_DIR:-$HOME/crypta-linux-vm}
P=$D/pki
EXTRA_BUILD=1
CRYPTA_GATE_LIB=1 . "$ROOT/scripts/crypta-v1-gate.sh"
GW=198.51.100.52
K="-i $KEY -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o LogLevel=ERROR"
NAVPUB=$(printf 'c%.0s' $(seq 64))
# A web courier's uid: arcad's courier range (64000-64999), clear of the
# launcher's (it counts up from 64000).
COURIER_UID=64990
PUBK=$(printf 'a%.0s' $(seq 64))
asks() { "$V" "grep -c 'the gateway asks' $G/cryptad.log" || true; }
hexof() { printf %s "$1" | od -An -tx1 | tr -d ' \n'; }
unhex() { python3 -c 'import sys; print(bytes.fromhex(sys.argv[1]).decode())' "$1"; }
# One key of profile $1's `tunnel` line.
tl() { "$V" "$CTL list" | awk -v n="$1" -v k="$2" '/^tunnel /{name="";for(i=2;i<=NF;i++){split($i,a,"=");if(a[1]=="name")name=a[2]}; if(name!=n)next; for(i=2;i<=NF;i++){split($i,a,"=");if(a[1]==k)print a[2]}}'; }
# What the person typed, held by Arca for Crypta's engines only.
hold() { "$V" "su -m cg-ui -c '$G/arca-probe $G/arcad.sock hold alice $PUBK org.atrium.curia $1 vpn'" | sed -n 's/^status=0 body=//p'; }
wait_for() { # $1 profile, $2 key, $3 value, $4 what
    i=0; until [ "$(tl "$1" "$2")" = "$3" ]; do
        i=$((i + 1)); [ $i -lt 60 ] || fail "$4: $("$V" "$CTL list" | grep "name=$1 "); $(e=$(tl "$1" error); [ -n "$e" ] && unhex "$e")"; sleep 1; done
}
# Answer profile $1's prompt: user $2, secret $3 (a password through Arca,
# a code as text), group $4; KEEP=1 saves the password.
answer() {
    pr=$("$V" "$CTL prompt name=$1") || fail "prompt $1: $pr"
    args=""
    for f in $(echo "$pr" | awk '/^field /{for(i=2;i<=NF;i++){split($i,a,"=");v[a[1]]=a[2]}; print v["i"] ":" v["kind"]}'); do
        i=${f%%:*}; k=${f#*:}
        case $k in
        0) args="$args t$i=$(hexof "$2")" ;;
        1) h=$(hold "$3"); [ -n "$h" ] || fail "Arca held nothing"; args="$args h$i=$h" ;;
        2) args="$args t$i=$(hexof "$3")" ;;
        3) args="$args t$i=$(hexof "$4")" ;;
        esac
    done
    "$V" "$CTL answer name=$1 $args ${KEEP:+keep=1}" >/dev/null || fail "answer $1"
}
# Sign profile $1 in: answer each prompt (the gateway may ask in several
# steps) until it is in. $2 user, $3 secret, $4 group.
signin() {
    for round in 1 2 3 4; do
        i=0; until st=$(tl "$1" signin); [ "$st" = prompt ] || [ "$st" = in ]; do
            i=$((i + 1)); [ $i -lt 60 ] || fail "$1: neither asked nor in: $("$V" "$CTL list" | grep "name=$1 "); $(e=$(tl "$1" error); [ -n "$e" ] && unhex "$e")"; sleep 1; done
        [ "$st" = in ] && return 0
        a=$(asks)
        answer "$@"
        i=0; until [ "$(asks)" -gt "$a" ] || [ "$(tl "$1" signin)" = in ] || [ -n "$(tl "$1" error)" ]; do
            i=$((i + 1)); [ $i -lt 60 ] || fail "$1: no answer from the gateway"; sleep 1; done
        [ -n "$(tl "$1" error)" ] && return 0
    done
    fail "$1: still asked after 4 rounds"
}
# How many items Arca keeps for Crypta's engines.
arca_items() { echo $((0x$("$V" "su -m _crypta-key -c '$G/arca-probe $G/arcad.sock list'" | sed -n 's/^status=0 body=//p' | cut -c1-2))); }
prompt_text() { # $1 profile, $2 k: the text, decoded
    "$V" "$CTL prompt name=$1" | awk -v k="$2" '/^text /{split($2,a,"=");split($3,b,"=");if(a[2]==k)printf "%s", b[2]}' | { read -r h || true; [ -n "${h:-}" ] && unhex "$h"; true; }
}
# A split tunnel: what the guest sent the LAN for the tunnel's networks.
acleaks() { lanctl seen | python3 -c "import json,sys; d=json.load(sys.stdin); print(' '.join(f'{k}={v}' for k,v in sorted(d.items()) if k.split(' ')[1].startswith(('10.98.', '203.0.113.'))))"; }

# The sign-in window's part as Arca sees it: the broker's ticket for the
# gateway's answer to the SAML form, then the courier's Set-Cookie of the
# token the gateway set (what navigator-cookies' Broker and arca-courier do
# for the window; their path is navigator-cookies' tests/broker.rs).
courier_token() { # $1 the token
    t=$("$V" "su -m cg-nav -c '$G/arca-probe $G/arcad.sock web-ticket sso.crypta.test:447 vpn-sso.crypta.test 3 /+CSCOE+/saml/sp/acs 3 idp.crypta.test'" | sed -n 's/^status=0 body=//p')
    [ ${#t} = 32 ] || fail "no ticket for the window's form: $t"
    "$V" "su -m cg-cour -c '$G/arca-probe $G/arcad.sock web-set-cookie $t 3 /+CSCOE+/saml/sp/acs acSamlv2Token $1 \"Secure;HttpOnly\"'"
}

sso_checks() {
    "$V" "$CTL create-ac name=sso host=sso.crypta.test:447 ca=$CA scoped=0" >/dev/null || fail "create sso"
    a5=$(asks)
    "$V" "$CTL up name=sso" >/dev/null
    wait_for sso signin sso "no single sign-on asked"
    pr=$("$V" "$CTL prompt name=sso")
    login=$(unhex "$(echo "$pr" | awk '/^sso /{for(i=2;i<=NF;i++){split($i,a,"=");if(a[1]=="login")print a[2]}}')")
    case "$login" in https://sso.crypta.test:447/+CSCOE+/saml/sp/login*) ok "the gateway asks for single sign-on at $login" ;; *) fail "sso prompt: $pr" ;; esac
    [ "$(asks)" = "$a5" ] && ok "no form asked: the sign-in window's" || fail "a form was asked"
    # The window's sign-in, from the host: the gateway's sign-on address,
    # Keycloak's login form, its signed SAML answer back to the gateway.
    tok=$(sh "$ROOT/scripts/crypta-sso-stand.sh" token alice alice-sso-pw) || fail "the sign-in at Keycloak: $tok"
    ok "signed in at Keycloak; the gateway set its token (${#tok} characters)"
    # A token set anywhere but the profile's own sign-in jar is not Crypta's:
    # a Personal jar's cookie of the same name stays a cookie.
    n0=$(curl -sk https://127.0.0.1:8447/stats | sed -n 's/.*"token_ok": \([0-9]*\).*/\1/p')
    r=$(courier_token "$tok")
    case "$r" in status=0*) ok "the courier hands the token to Arca" ;; *) fail "courier: $r" ;; esac
    wait_for sso handshake 1 "not signed on with the token"
    ok "signed on: the engine took the token from Arca (tunnel $(tl sso addr))"
    r=$("$V" 'fetch -T 10 -qo - http://10.98.0.1/' 2>&1) && [ "$r" = crypta-ac-ok ] && ok "single sign-on: through the tunnel" || fail "sso HTTP: $r"
    "$V" "grep -rq $tok $G/state $G/cryptad.log" && fail "the token is in cryptad's state or log" || ok "the token is not in cryptad's state or log"
    left=$("$V" "su -m _crypta-key -c '$G/arca-probe $G/arcad.sock list'" | sed -n 's/^status=0 body=//p')
    case "$(python3 -c 'import sys; print(bytes.fromhex(sys.argv[1]).decode("latin-1"))' "$left")" in *sso/sso@*) fail "the token is still in Arca" ;; *) ok "used once: Arca no longer has it" ;; esac
    st=$(curl -sk https://127.0.0.1:8447/stats)
    n1=$(echo "$st" | sed -n 's/.*"token_ok": \([0-9]*\).*/\1/p')
    [ "$n1" = $((n0 + 1)) ] && ok "the gateway took it once ($st)" || fail "stand-in: $st"
    # A replayed token: refused by the gateway, and nothing signed in.
    "$V" "$CTL down name=sso" >/dev/null
    "$V" "$CTL up name=sso" >/dev/null
    wait_for sso signin sso "no sign-on asked again"
    r0=$(curl -sk https://127.0.0.1:8447/stats | sed -n 's/.*"token_refused": \([0-9]*\).*/\1/p')
    r=$(courier_token "$tok")
    i=0; until [ "$(curl -sk https://127.0.0.1:8447/stats | sed -n 's/.*"token_refused": \([0-9]*\).*/\1/p')" = $((r0 + 1)) ]; do i=$((i + 1)); [ $i -lt 30 ] || fail "the replayed token never reached the gateway: $r"; sleep 1; done
    wait_for sso signin sso "not asked to sign on again after the refusal"
    [ "$(tl sso handshake)" != 1 ] && ok "a used token again: refused by the gateway, and single sign-on asked again" || fail "replayed token got in"
    "$V" "$CTL down name=sso" >/dev/null
}

# The sign-in window as it runs (vpn.md §9.3): navigatord as the Navigator's
# broker in the profile's jar, every page through a courier that
# arca-courierd starts in a jail of its own, the forms sent as the window's
# form panel would (the probe stands in for the UI and the page child),
# the password typed into Arca's field (held by arca-ui: here arca-probe as
# its uid), Keycloak's session kept for the next sign-in, and gone with the
# profile. The stand's names point at lan.py's private relay (172.16.47.1),
# which couriers may reach under their test grant.
SSO_LAN=172.16.47.1
NV=/tmp/crypta-nav
nav_window() { # $1 profile, $2 login, $3 final; prints the probe's lines
    # No fetcher: an empty fixtures folder, so nothing comes the plain way.
    "$V" "pkill -f 'navigatord --unconfined' ; sleep 1; rm -rf $NV; mkdir -p $NV/none; chown -R cg-nav $NV
        su -m cg-nav -c 'cd $NV && env NAVIGATORD_FIXTURES=$NV/none NAVIGATORD_SOCKET=$NV/nav.sock NAVIGATORD_PAGE_SOCKET=$NV/page.sock NAVIGATORD_PRERENDER=$G/prerender \
            NAVIGATORD_ARCA=$G/arcad.sock NAVIGATORD_COURIERD=/atrium/sockets/arca-courier/courierd.sock NAVIGATORD_VPN_SIGN_IN=\"$1 $2 $3\" \
            daemon -f -o $NV/navigatord.log $G/navigatord --unconfined-for-testing' </dev/null
        i=0; while [ \$i -lt 30 ] && ! test -S $NV/nav.sock; do sleep 1; i=\$((i+1)); done
        test -S $NV/nav.sock || { echo \"stuck navigatord: \$(cat $NV/navigatord.log)\"; exit 1; }" || return 0
    h=$("$V" "su -m cg-ui -c '$G/arca-probe $G/arcad.sock hold alice $NAVPUB org.atrium.navigator alice-sso-pw'" | sed -n 's/^status=0 body=//p')
    "$V" "su -m cg-nav -c '$G/sign-in-probe $NV/nav.sock $NV/page.sock \"$2\" \"$3\" alice $h'" 2>&1 || true
}

window_checks() {
    "$V" "set -e
        install -m 755 $G/arca-courier /usr/local/bin/atrium-arca-courier
        install -m 755 $G/arca-courierd /usr/local/bin/arca-courierd
        sed -i '' -e '/sso.crypta.test/d' -e '/idp.crypta.test/d' /etc/hosts
        echo '$SSO_LAN sso.crypta.test idp.crypta.test' >> /etc/hosts
        pkill -x arca-courierd || true; sleep 1
        chmod 755 $G
        env ARCA_COURIERD_CA=$G/ca.pem ARCA_COURIERD_GRANT_PRIVATE_NETWORK=1 ARCA_WEB_BROKER='org.atrium.navigator $NAVPUB' \
            ARCA_APP_REGISTRY=$G/apps.registry ARCA_COURIER_REGISTRY=/var/run/atrium/courier-registry \
            daemon -f -o $G/courierd.log /usr/local/bin/arca-courierd
        i=0; while [ \$i -lt 30 ] && ! test -S /atrium/sockets/arca-courier/courierd.sock; do sleep 1; i=\$((i+1)); done
        test -S /atrium/sockets/arca-courier/courierd.sock"
    "$V" "$CTL create-ac name=win host=sso.crypta.test:447 ca=$CA scoped=0" >/dev/null || fail "create win"
    "$V" "$CTL up name=win" >/dev/null
    wait_for win signin sso "no single sign-on asked"
    pr=$("$V" "$CTL prompt name=win")
    login=$(unhex "$(echo "$pr" | awk '/^sso /{for(i=2;i<=NF;i++){split($i,a,"=");if(a[1]=="login")print a[2]}}')")
    fin=$(unhex "$(echo "$pr" | awk '/^sso /{for(i=2;i<=NF;i++){split($i,a,"=");if(a[1]=="final")print a[2]}}')")
    r=$(nav_window win "$login" "$fin")
    echo "$r" | sed 's/^/     /'
    echo "$r" | grep -q 'form POST .*/login-actions/authenticate .*password=<arca>' && ok "Keycloak's login form went with the password from Arca's field (the probe never had it)" || fail "login form: $r"
    echo "$r" | grep -q 'form POST https://sso.crypta.test:447/+CSCOE+/saml/sp/acs .*SAMLResponse=' && ok "its signed answer went on to the gateway (the noscript Continue)" || fail "SAML post: $r"
    echo "$r" | grep -q "^final " && ok "the window reached the final address" || fail "no final page: $r"
    cl=$("$V" "grep -o 'courier [0-9]* for org.atrium.navigator (alice) to [a-z.]*:[0-9]*' $G/courierd.log | awk '{print \$NF}' | sort -u | tr '\\n' ' '")
    case "$cl" in *idp.crypta.test:448*sso.crypta.test:447*) ok "every page through couriers in jails of their own ($cl)" ;; *) fail "couriers: $cl $("$V" "tail -5 $G/courierd.log")" ;; esac
    wait_for win handshake 1 "not signed on with the window's token"
    ok "signed on: the token the gateway set in the window's jar reached the engine through Arca (tunnel $(tl win addr))"
    "$V" "grep -rqs alice-sso-pw $NV $G/state $G/cryptad.log $G/courierd.log" && fail "the IdP password is in a log" || ok "the IdP password is in no log or state"
    # Keycloak's own session stayed in the profile's jar: the next sign-in
    # asks for no password.
    "$V" "$CTL down name=win" >/dev/null
    "$V" "$CTL up name=win" >/dev/null
    wait_for win signin sso "no sign-on asked again"
    r=$(nav_window win "$login" "$fin")
    echo "$r" | sed 's/^/     /'
    echo "$r" | grep -q 'password=' && fail "asked for the password again: $r" || ok "the next sign-in: Keycloak's session from the profile's jar, no password asked"
    wait_for win handshake 1 "not signed on the second time"
    "$V" "$CTL down name=win" >/dev/null
    # Removed and made again under the same name: the jar is gone.
    "$V" "$CTL delete name=win" >/dev/null || fail "delete win"
    "$V" "$CTL create-ac name=win host=sso.crypta.test:447 ca=$CA scoped=0" >/dev/null || fail "create win again"
    "$V" "$CTL up name=win" >/dev/null
    wait_for win signin sso "no sign-on asked after removal"
    r=$(nav_window win "$login" "$fin")
    echo "$r" | grep -q 'password=<arca>' && ok "after removing the profile, Keycloak asks for the password again: its session went with the jar" || fail "after removal: $r"
    wait_for win handshake 1 "not signed on after removal"
    "$V" "$CTL down name=win" >/dev/null
    "$V" "pkill -f 'navigatord --unconfined'; pkill -x arca-courierd" || true
}

# arcad (alice's store unlocked, the Navigator's broker and a web courier
# known) and cryptad, as Settings and the sign-in window find them.
stand() {
    "$V" "set -e
        install -m 755 $G/atrium-crypta-ac /usr/local/bin/atrium-crypta-ac
        for u in cg-vest cg-ui cg-nav; do id \$u >/dev/null 2>&1 || pw useradd \$u -d /nonexistent -s /usr/sbin/nologin -c 'crypta gate'; done
        id cg-cour >/dev/null 2>&1 || pw useradd cg-cour -u $COURIER_UID -d /nonexistent -s /usr/sbin/nologin -c 'crypta gate courier'
        pkill -f atrium-cryptad; pkill -f atrium-arcad-gate; sleep 1
        # The Navigator's broker and one web courier, as arcad knows them
        # (the launch and courier registries): a VPN sign-in window's jar.
        echo \"\$(id -u cg-nav) alice org.atrium.navigator $NAVPUB persona=vpn\" > $G/apps.registry
        mkdir -p /var/run/atrium; echo '$COURIER_UID alice web https sso.crypta.test 447 vpn-sso.crypta.test' > /var/run/atrium/courier-registry
        # Where couriers' jails find arcad.
        mkdir -p /atrium/sockets/arca /var/run/atrium; rm -f $G/arcad.sock; ln -s /atrium/sockets/arca/arcad.sock $G/arcad.sock
        env ARCA_DIR=$G/arca ARCA_SOCKET=/atrium/sockets/arca/arcad.sock ARCA_APP_REGISTRY=$G/apps.registry ARCA_SERVICES=cryptad=65052 ARCA_SIGILLUM_SOCKET=none \
            ARCA_WEB_BROKER='org.atrium.navigator $NAVPUB' ARCA_COURIER_REGISTRY=/var/run/atrium/courier-registry \
            ARCA_VESTIBULUM_UID=\$(id -u cg-vest) ARCA_UI_UID=\$(id -u cg-ui) daemon -f -o $G/arcad.log /usr/local/bin/atrium-arcad-gate
        sleep 1
        su -m cg-vest -c '$G/arca-probe $G/arcad.sock unlock alice correct-horse' >/dev/null
        daemon -f -o $G/cryptad.log /usr/local/bin/atrium-cryptad -v --sock $G/crypta.sock --state $G/state --net-sock $G/net.sock --arca $G/arcad.sock
        grep -q gw.crypta.test /etc/hosts || echo '$GW gw.crypta.test' >> /etc/hosts
        grep -q sso.crypta.test /etc/hosts || echo '$GW sso.crypta.test' >> /etc/hosts
        sleep 1"
}

case "${1:-}" in
run)
    for p in 8443 8444 8445 8446; do
        nc -z 127.0.0.1 $p 2>/dev/null || { echo "ocserv's forward :$p is not up: sh scripts/crypta-ocserv-vm.sh up" >&2; exit 1; }
    done
    CROSS_RUSTFLAGS=$(set -- env; ARCH=amd64 . "$ROOT/scripts/cloud-vm.sh" >/dev/null; printf %s "$CROSS_RUSTFLAGS")
    (cd "$ROOT/arca/arcad" && RUSTFLAGS="$CROSS_RUSTFLAGS" cargo build -q --release --target $T --bin arca-probe)
    (cd "$ROOT/crypta" && RUSTFLAGS="$CROSS_RUSTFLAGS" cargo build -q --release --target $T -p crypta-engine)
    (cd "$ROOT/navigatord" && RUSTFLAGS="$CROSS_RUSTFLAGS" cargo build -q --release --target $T --bin navigatord)
    (cd "$ROOT/navigator-prerender" && RUSTFLAGS="$CROSS_RUSTFLAGS" cargo build -q --release --target $T --bin prerender)
    (cd "$ROOT/navigator-page" && RUSTFLAGS="$CROSS_RUSTFLAGS" cargo build -q --release --target $T --example sign-in-probe)
    (cd "$ROOT/arca/arca-courierd" && RUSTFLAGS="$CROSS_RUSTFLAGS" cargo build -q --release --target $T --bin arca-courierd)
    (cd "$ROOT/arca/arca-courier" && RUSTFLAGS="$CROSS_RUSTFLAGS" cargo build -q --release --target $T --bin arca-courier)
    EXTRA="$ROOT/crypta/target/$T/release/atrium-crypta-ac $ROOT/arca/arcad/target/$T/release/arca-probe
        $ROOT/navigatord/target/$T/release/navigatord $ROOT/navigator-prerender/target/$T/release/prerender
        $ROOT/navigator-page/target/$T/release/examples/sign-in-probe $ROOT/arca/arca-courierd/target/$T/release/arca-courierd
        $ROOT/arca/arca-courier/target/$T/release/arca-courier $P/ca.pem"
    setup

    echo "== the AnyConnect engine, Arca's field and a person's store"
    stand
    h=$(hold probe); [ ${#h} = 32 ] && ok "Arca holds a typed value for Crypta (a handle)" || fail "hold: $h"
    CA=$(openssl x509 -in "$P/ca.pem" -outform DER | base64 -w0)

    echo "== password, with a group to choose"
    "$V" "$CTL create-ac name=corp host=gw.crypta.test ca=$CA scoped=0" >/dev/null || fail "create-ac"
    "$V" "$CTL up name=corp" >/dev/null || fail "up corp"
    wait_for corp signin prompt "no prompt"
    n=$("$V" "$CTL prompt name=corp" | grep -c '^field ') ; ok "the gateway asks ($n fields: $("$V" "$CTL prompt name=corp" | awk '/^field /{print $3}' | tr '\n' ' '))"
    "$V" "$CTL prompt name=corp" | grep -q '^choice ' && ok "a group to choose" || fail "no group choice"
    lanctl seen-reset >/dev/null
    answer corp alice alice-pw Staff
    wait_for corp signin prompt "no password asked"
    i=0; until "$V" "$CTL prompt name=corp" | grep -q 'kind=1'; do i=$((i + 1)); [ $i -lt 30 ] || fail "no password field: $("$V" "$CTL prompt name=corp")"; sleep 1; done
    r=$("$V" "$CTL answer name=corp t0=$(hexof alice-pw)" 2>&1) && fail "a password as text was taken" || ok "then the password; as plain text it is refused ($r)"
    KEEP=1 signin corp alice alice-pw Staff
    wait_for corp signin in "not signed in"
    wait_for corp handshake 1 "no tunnel"
    ok "signed in; tunnel address $(tl corp addr) on $(tl corp iface)"
    "$V" 'jls name exec.command 2>/dev/null; jls -n name' | grep -q system-crypta- && ok "the engine runs in its own jail" || fail "no engine jail"
    "$V" "ps -axo command | grep -q '^/usr/local/bin/atrium-crypta-ac'" && ok "the engine is atrium-crypta-ac" || fail "engine binary"
    r=$("$V" 'fetch -T 10 -qo - http://10.98.0.1/' 2>&1) && [ "$r" = crypta-ac-ok ] && ok "HTTP to 10.98.0.1 through the tunnel" || fail "HTTP 10.98.0.1: $r"
    r=$("$V" 'fetch -T 10 -qo - http://203.0.113.80/' 2>&1) && [ "$r" = crypta-ac-ok ] && ok "HTTP to 203.0.113.80 (a pushed route)" || fail "HTTP 203.0.113.80: $r"
    "$V" 'local-unbound-control list_forwards' | grep -q corp.crypta.test && ok "the pushed domain forwards to the pushed DNS" || fail "no corp.crypta.test forward"
    l=$(acleaks); [ -z "$l" ] && ok "nothing for the tunnel's networks on the LAN, only the gateway's TCP ($(stat ac_out) bytes out)" || fail "leaked: $l"
    "$V" "grep -rq alice-pw $G/state $G/cryptad.log" && fail "the password is in cryptad's state or log" || ok "the password is not in cryptad's state or log"
    [ "$(tl corp user)" = "$(hexof alice)" ] && ok "the profile remembers the username" || fail "user: $(tl corp user)"
    [ "$(tl corp banner)" = 1 ] || fail "no banner flag"
    bn=$("$V" "$CTL banner name=corp" | awk '/^text /{for(i=2;i<=NF;i++){split($i,a,"=");if(a[1]=="v")printf "%s", a[2]}}')
    bn=$(unhex "$bn"); case "$bn" in *"Crypta gate gateway"*) ok "the gateway's banner, as text: $bn" ;; *) fail "banner: $bn" ;; esac

    echo "== roam: every connection to the gateway reset"
    a0=$(asks); s0=$(tl corp starts); addr=$(tl corp addr); syn=$(stat ac_syn)
    lanctl ac-drop >/dev/null
    sleep 3
    i=0; until r=$("$V" 'fetch -T 3 -qo - http://10.98.0.1/' 2>/dev/null) && [ "$r" = crypta-ac-ok ]; do i=$((i + 1)); [ $i -lt 30 ] || fail "not back after the reset: $("$V" "tail -5 $G/cryptad.log")"; sleep 1; done
    [ "$(asks)" = "$a0" ] && [ "$(tl corp starts)" = "$s0" ] && [ "$(tl corp addr)" = "$addr" ] && ok "resumed with the session: same engine, same address, nothing asked (${i}s)" || fail "roam: asked $(asks)/$a0 starts $(tl corp starts)/$s0 addr $(tl corp addr)/$addr"
    [ "$(stat ac_syn)" -gt "$syn" ] && ok "on a new connection ($(( $(stat ac_syn) - syn )) since the reset)" || fail "no new connection after the reset"

    echo "== down and up: the saved password, nothing asked"
    k0=$(arca_items)
    "$V" "$CTL down name=corp" >/dev/null || fail "down corp"
    "$V" 'pfctl -a atrium/vpn -sr' | grep -q . && fail "atrium/vpn left loaded" || ok "down: pf anchor emptied"
    [ "$(arca_items)" = $((k0 - 1)) ] && ok "down: the session cookie is gone from Arca, the password kept ($k0 -> $(arca_items) items)" || fail "Arca items after down: $k0 -> $(arca_items)"
    "$V" "$CTL up name=corp" >/dev/null || fail "up corp again"
    wait_for corp handshake 1 "not back"
    [ "$(asks)" = "$a0" ] && ok "signed in again without asking" || fail "asked again"
    "$V" "$CTL down name=corp" >/dev/null

    echo "== a wrong password asks again; the right one gets in"
    "$V" "$CTL create-ac name=ann host=$GW ca=$CA scoped=0" >/dev/null || fail "create ann"
    "$V" "$CTL up name=ann" >/dev/null
    wait_for ann signin prompt "no prompt (ann)"
    answer ann ann wrong-pw Staff
    i=0; until "$V" "$CTL prompt name=ann" 2>/dev/null | grep -q 'kind=1'; do i=$((i + 1)); [ $i -lt 30 ] || fail "no password asked (ann)"; sleep 1; done
    a1=$(asks)
    answer ann ann wrong-pw Staff
    i=0; until [ "$(asks)" -gt "$a1" ]; do i=$((i + 1)); [ $i -lt 30 ] || fail "not asked again"; sleep 1; done
    e=$(prompt_text ann error; prompt_text ann message)
    ok "asked again: $(echo "$e" | head -1)"
    signin ann ann ann-pw Staff
    wait_for ann handshake 1 "ann not in"
    ok "the right password gets in"
    "$V" "$CTL down name=ann" >/dev/null

    echo "== password and a one-time code"
    "$V" "$CTL create-ac name=otp host=$GW:444 ca=$CA scoped=0" >/dev/null || fail "create otp"
    "$V" "$CTL up name=otp" >/dev/null
    wait_for otp signin prompt "no prompt (otp)"
    for step in 1 2 3; do
        "$V" "$CTL prompt name=otp" | grep -q 'kind=2' && break
        a2=$(asks)
        answer otp otto otto-pw ""
        i=0; until [ "$(asks)" -gt "$a2" ]; do i=$((i + 1)); [ $i -lt 30 ] || fail "no code asked: $("$V" "$CTL list" | grep name=otp)"; sleep 1; done
    done
    "$V" "$CTL prompt name=otp" | grep -q 'kind=2' && ok "then a one-time code is asked" || fail "no code field: $("$V" "$CTL prompt name=otp")"
    answer otp otto "$(sh "$ROOT/scripts/crypta-ocserv-vm.sh" totp)" ""
    wait_for otp handshake 1 "otp not in"
    r=$("$V" 'fetch -T 10 -qo - http://10.98.1.1/' 2>&1) && [ "$r" = crypta-ac-ok ] && ok "password + TOTP: through the tunnel" || fail "otp HTTP: $r"
    "$V" "$CTL down name=otp" >/dev/null

    echo "== a client certificate: the key made in Arca, signed by the gate CA"
    "$V" "$CTL create-ac name=cert host=$GW:445 ca=$CA scoped=0" >/dev/null || fail "create cert"
    C=$(mktemp -d)
    "$V" "$CTL csr name=cert cn=$(hexof carol)" | sed -n 's/^csr pem=//p' | python3 -c 'import sys; sys.stdout.write(bytes.fromhex(sys.stdin.read().strip()).decode())' > "$C/req.csr"
    openssl req -in "$C/req.csr" -verify -noout 2>/dev/null && ok "a request Arca signed (openssl verifies it)" || fail "csr: $(cat "$C/req.csr")"
    openssl x509 -req -in "$C/req.csr" -CA "$P/ca.pem" -CAkey "$P/ca.key" -CAcreateserial -out "$C/cert.pem" -days 30 -extfile "$P/client.ext" 2>/dev/null
    "$V" "$CTL certificate name=cert" < "$P/carol.pem" >/dev/null 2>&1 && fail "another key's certificate taken" || ok "another key's certificate refused"
    "$V" "$CTL certificate name=cert" < "$C/cert.pem" >/dev/null || fail "certificate"
    [ "$(tl cert cert)" = 1 ] && ok "certificate kept" || fail "cert flag"
    a3=$(asks)
    "$V" "$CTL up name=cert" >/dev/null
    wait_for cert handshake 1 "cert not in"
    [ "$(asks)" = "$a3" ] && ok "certificate sign-in: nothing asked" || fail "cert asked"
    r=$("$V" 'fetch -T 10 -qo - http://10.98.2.1/' 2>&1) && [ "$r" = crypta-ac-ok ] && ok "certificate: through the tunnel" || fail "cert HTTP: $r"
    "$V" "$CTL down name=cert" >/dev/null
    rm -rf "$C"

    echo "== a pinned gateway certificate"
    PIN=$(openssl x509 -in "$P/server.pem" -outform DER | sha256sum | cut -c1-64)
    BAD=$(printf '0%.0s' $(seq 64))
    "$V" "$CTL create-ac name=pin host=$GW pin=$PIN user=$(hexof ann) group=$(hexof Staff)" >/dev/null || fail "create pin"
    "$V" "$CTL create-ac name=badpin host=$GW pin=$BAD" >/dev/null || fail "create badpin"
    "$V" "$CTL up name=pin" >/dev/null
    wait_for pin signin prompt "pin: no prompt"
    "$V" "$CTL prompt name=pin" | grep '^field ' | grep -qv 'kind=1' && fail "pin: more than the password asked: $("$V" "$CTL prompt name=pin")"
    ok "pinned: the saved username and group answered, only the password asked"
    signin pin ann ann-pw Staff
    wait_for pin handshake 1 "pin not in"
    ok "pinned gateway: in"
    # A profile is a persona's by default (vpn.md §9.6): its routes in the
    # persona's route domain, none in the machine's table.
    "$V" 'netstat -rn -F 1 -f inet' | grep -q '^10.98.0.0/24 .*tun' && ok "scoped by default: the routes are in the persona's route domain" || fail "no route in fib 1: $("$V" 'netstat -rn -F 1 -f inet')"
    "$V" 'netstat -rn -F 0 -f inet' | grep -q '^10.98.0.0/24 .*tun' && fail "the persona's routes are in the machine's table" || ok "and none in the machine's table"
    "$V" "$CTL down name=pin" >/dev/null
    "$V" "$CTL up name=badpin" >/dev/null
    i=0; until [ -n "$(tl badpin error)" ]; do i=$((i + 1)); [ $i -lt 30 ] || fail "badpin not refused"; sleep 1; done
    ok "the wrong pin: refused ($(unhex "$(tl badpin error)"))"
    [ "$(tl badpin up)" = 0 ] && ok "and the tunnel is closed" || fail "badpin still up"

    echo "== a gateway pushing what Atrium must refuse"
    "$V" "$CTL create-ac name=hostile host=$GW:446 ca=$CA scoped=0" >/dev/null || fail "create hostile"
    "$V" "$CTL up name=hostile" >/dev/null
    wait_for hostile signin prompt "hostile: no prompt"
    signin hostile alice alice-pw ""
    i=0; until [ -n "$(tl hostile error)" ]; do i=$((i + 1)); [ $i -lt 30 ] || fail "hostile push not refused: $("$V" "$CTL list" | grep name=hostile)"; sleep 1; done
    ok "refused: $(unhex "$(tl hostile error)")"
    [ "$(tl hostile up)" = 0 ] && ok "the hostile tunnel is closed" || fail "hostile still up"
    rt=$("$V" 'netstat -rn -f inet' | awk '$1 ~ /^(127\/8|127.0.0.0\/8|10.98.3)/ && $4 ~ /^tun/')
    [ -z "$rt" ] && ok "none of its routes applied" || fail "hostile routes: $rt"
    "$V" 'local-unbound-control list_forwards' | grep -q 'localhost' && fail "localhost forwarded" || ok "no forward for localhost"

    echo "== single sign-on: Keycloak (SAML) through the stand-in gateway"
    if nc -z 127.0.0.1 8447 2>/dev/null && nc -z 127.0.0.1 8448 2>/dev/null; then
        sso_checks
    else
        echo "skip the SAML stand is not up (sh scripts/crypta-sso-stand.sh up)"
    fi

    echo "== the sign-in window: navigatord, Arca's couriers, Keycloak"
    if nc -z 127.0.0.1 8447 2>/dev/null && nc -z 127.0.0.1 8448 2>/dev/null; then
        window_checks
    else
        echo "skip the SAML stand is not up (sh scripts/crypta-sso-stand.sh up)"
    fi

    echo "== delete: everything Arca kept for the profiles"
    for n in corp ann otp cert pin badpin hostile sso win; do "$V" "$CTL delete name=$n" >/dev/null || fail "delete $n"; done
    left=$(arca_items)
    [ "$left" = 0 ] && ok "Arca keeps nothing for them (passwords, sessions, the client key)" || fail "Arca still holds items: $left"
    echo "crypta V4 gate: $pass checks passed"
    ;;
ui)
    # Frames of Settings' sign-ins (scratch/crypta-v4-*.png): the stand,
    # Crypta's front, a password gateway and a single sign-on gateway both
    # asking, and a headless frescod, forum-wm, limend, arca-ui (Arca's
    # field in a slot) and Curia. The UI binaries come prebuilt from
    # $UIBIN (frescod forum-wm curia limend arca-ui, x86_64-unknown-freebsd).
    # Then `click X Y name` presses and frames.
    U=/var/tmp/crypta-v4-ui; FS=$U/fresco.sock
    CROSS_RUSTFLAGS=$(set -- env; ARCH=amd64 . "$ROOT/scripts/cloud-vm.sh" >/dev/null; printf %s "$CROSS_RUSTFLAGS")
    (cd "$ROOT/arca/arcad" && RUSTFLAGS="$CROSS_RUSTFLAGS" cargo build -q --release --target $T --bin arca-probe)
    (cd "$ROOT/crypta" && RUSTFLAGS="$CROSS_RUSTFLAGS" cargo build -q --release --target $T -p crypta-engine -p crypta-ctl)
    EXTRA="$ROOT/crypta/target/$T/release/atrium-crypta-ac $ROOT/arca/arcad/target/$T/release/arca-probe
        $ROOT/crypta/target/$T/release/atrium-crypta-ctl $P/ca.pem"
    setup
    trap - EXIT
    pkill -f atrium-net/sim/lan.py 2>/dev/null || true
    python3 "$ROOT/atrium-net/sim/lan.py" 3600 > "$LAN_LOG" 2>&1 &
    sleep 15
    "$V" "install -m 755 $G/atrium-crypta-ac /usr/local/bin/atrium-crypta-ac
        for u in cg-vest cg-ui cg-nav cg-set; do id \$u >/dev/null 2>&1 || pw useradd \$u -d /nonexistent -s /usr/sbin/nologin -c 'crypta gate'; done"
    stand
    scp $K -P 2222 -q "$ROOT/crypta/etc/atrium-crypta-ctl" root@localhost:$G/rc.atrium-crypta-ctl
    "$V" "set -e
        echo \"\$(id -u cg-set) alice org.atrium.curia $PUBK\" >> $G/apps.registry; chmod 644 $G/apps.registry
        id -u _crypta-ctl 2>/dev/null | grep -qx 65054 || { pw userdel _crypta-ctl 2>/dev/null || true; pw useradd _crypta-ctl -u 65054 -d /nonexistent -s /usr/sbin/nologin -c 'cryptad settings front'; }
        install -m 755 $G/atrium-crypta-ctl /usr/local/bin/
        install -m 555 $G/rc.atrium-crypta-ctl /usr/local/etc/rc.d/atrium-crypta-ctl
        sysrc atrium_crypta_ctl_ctl=$G/crypta.sock atrium_crypta_ctl_flags=-v >/dev/null
        service atrium-crypta-ctl onestop >/dev/null 2>&1 || true
        service atrium-crypta-ctl onestart >/dev/null
        i=0; while [ \$i -lt 20 ] && ! test -S /atrium/sockets/vpn-configure/vpn-configure.sock; do sleep 1; i=\$((i+1)); done"
    CA=$(openssl x509 -in "$P/ca.pem" -outform DER | base64 -w0)
    "$V" "$CTL create-ac name=corp host=gw.crypta.test ca=$CA scoped=0; $CTL up name=corp
        $CTL create-ac name=sso host=sso.crypta.test:447 ca=$CA scoped=0; $CTL up name=sso" >/dev/null || true
    wait_for corp signin prompt "no prompt"
    "$V" "pkill -x 'curia|forum-wm|frescod|limend|arca-ui' || true; sleep 1; rm -rf $U; mkdir -p $U /usr/local/share/fonts/ibm-plex"
    for b in curia forum-wm frescod limend arca-ui; do scp $K -P 2222 -q "${UIBIN:?the UI binaries}/$b" root@localhost:$U/; done
    scp $K -P 2222 -q "$ROOT/fonts/ibm-plex/IBMPlexSans.ttf" "$ROOT/fonts/phosphor/Phosphor.ttf" root@localhost:/usr/local/share/fonts/ibm-plex/
    E="FRESCO_SOCKET=$FS FRESCO_WM_SOCKET=$U/wm.sock FORUM_CTL_SOCKET=$U/ctl.sock"
    "$V" "set -e; cd $U
        mkdir -p /atrium/sockets/ostiarius; printf 'alice\n' > /atrium/sockets/ostiarius/active-session; : > /atrium/sockets/ostiarius/session-locked
        UI=\$(id -u cg-ui); SET=\$(id -u cg-set)
        env FRESCOD_SPLIT=0 FRESCOD_HEADLESS_PNG=$U/screen FRESCOD_SOCK=$FS FRESCOD_WM_SOCK=$U/wm.sock FRESCOD_INPUT_SOCK=$U/inject.sock \
            FRESCOD_ACTIVATION_SOCK=$U/activation.sock FRESCOD_CHROME_UIDS=\$SET FRESCOD_SLOT_UIDS=0 FRESCOD_ACTIVATION_UIDS=\$UI FRESCOD_INJECT_ACTIVATION=1 \
            daemon -f -o $U/frescod.log $U/frescod </dev/null
        i=0; while [ \$i -lt 60 ] && ! test -S $FS; do sleep 2; i=\$((i+1)); done
        env $E daemon -f -o $U/wm.log $U/forum-wm </dev/null; sleep 4; chmod 666 $U/*.sock; chmod 755 $U
        env LIMEN_SOCKET=$U/limend.sock LIMEN_FRESCOD_ACTIVATION=$U/activation.sock LIMEN_APP_REGISTRY=$G/apps.registry LIMEN_ARCA_UID=\$UI \
            daemon -f -o $U/limend.log $U/limend </dev/null
        i=0; while [ \$i -lt 20 ] && ! test -S $U/limend.sock; do sleep 1; i=\$((i+1)); done; chmod 666 $U/limend.sock
        env LIMEN_SOCKET=$U/limend.sock FRESCO_SOCKET=$FS ARCA_SOCKET=/atrium/sockets/arca/arcad.sock ARCA_APP_REGISTRY=$G/apps.registry \
            daemon -f -o $U/arca-ui.log -u cg-ui $U/arca-ui </dev/null
        env $E LIMEN_SOCKET=$U/limend.sock CURIA_SIZE=1100x760 daemon -f -u cg-set -o $U/curia.log $U/curia </dev/null
        sleep 3; pgrep -lx 'frescod|forum-wm|limend|arca-ui|curia' | tr '\n' ' '; echo"
    GAP=40 sh "$0" frame start
    ;;
click)
    U=/var/tmp/crypta-v4-ui
    "$V" "printf 'MOVE $2 $3\n' | nc -N -U $U/inject.sock; sleep 1; printf 'BTN 1 1\n' | nc -N -U $U/inject.sock; sleep 1; printf 'BTN 1 0\n' | nc -N -U $U/inject.sock" >/dev/null
    sh "$0" frame "${4:-click}"
    ;;
type) # lower-case letters and digits, then a frame
    U=/var/tmp/crypta-v4-ui; s=$2; out=""
    while [ -n "$s" ]; do
        c=$(printf '%s' "$s" | cut -c1); s=$(printf '%s' "$s" | cut -c2-)
        case "$c" in [a-z]) u=$(( $(printf '%d' "'$c") - 97 + 4 )) ;; [1-9]) u=$(( c + 29 )) ;; 0) u=39 ;; *) exit 2 ;; esac
        out="${out}KEY $u 1\nKEY $u 0\n"
    done
    "$V" "printf '$out' | nc -N -U $U/inject.sock" >/dev/null
    sh "$0" frame "${3:-typed}"
    ;;
frame)
    U=/var/tmp/crypta-v4-ui; O="$ROOT/scratch"; mkdir -p "$O"
    sleep "${GAP:-15}"
    "$V" "cp $U/screen.png $U/s.png 2>/dev/null || cp $U/screen $U/s.png"
    scp $K -P 2222 -q root@localhost:$U/s.png "$O/crypta-v4-$2.png" && echo "frame $O/crypta-v4-$2.png"
    ;;
*)
    echo "usage: $0 run|ui|click X Y name|frame name" >&2
    exit 2
    ;;
esac
