#!/bin/sh
# Crypta V3 gate (docs/spec/vpn.md §7.2-§7.4, §14 V3): CLASS_VPN through
# atrium-crypta-ctl, jailed apps with and without each capability, and
# frames of Settings' VPN page and the bar's VPN item.
#
#   ARCH=amd64 TCG_CPU=Haswell-noTSX QEMU_BIN=... sh scripts/atrium-net-gate.sh vm
#   sh scripts/crypta-wg-linux-vm.sh up              the Ubuntu VM (wg0, the peer)
#   sh scripts/crypta-v3-gate.sh run                 build, install, check
#   sh scripts/crypta-v3-gate.sh ui                  after run (KEEP=1): frames
#
# The stand is the V1 gate's (it shares its setup): lan.py, the Ubuntu VM as
# the WireGuard peer with HTTP and DNS on 10.99.0.1. atrium-crypta-ctl is
# started by its rc.d script, serving /atrium/sockets/vpn-* for the gate's
# cryptad. A tunnel is made with a key Arca mints (CREATE_WIREGUARD); the
# Ubuntu side then learns our public key, as a server's admin would.
#
# Jailed apps, launched by portcullisd from signed bundles, are the vpnctl
# probe (no arguments: it tries each vpn-* socket it can see):
# org.test.vpnuse (vpn-read + vpn-use), org.test.vpnconf (vpn-configure),
# org.test.vpnnone (nothing).
set -eu
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
CRYPTA_GATE_LIB=1 . "$ROOT/scripts/crypta-v1-gate.sh"
VC="/usr/local/bin/atrium-vpnctl --dir /atrium/sockets"
K="-i $KEY -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o LogLevel=ERROR"
B=/var/tmp/crypta-v3
PSOCK=/atrium/sockets/portcullis/portcullis.sock
OUT=${OUT:-$ROOT/scratch/crypta-v3}
DUMMY=$(python3 -c 'import os,base64; print(base64.b64encode(os.urandom(32)).decode())')

cross() { # dir, cargo args...
    d=$1; shift
    (cd "$ROOT/$d" && RUSTFLAGS="$CROSS_RUSTFLAGS" cargo build -q --release --target $T "$@")
}
state() { "$V" "$VC --grant read status" | awk -v n="$1" '$2 == "name="n {sub("state=", "", $4); print $4}'; }
capmode() { "$V" "procstat -s $1 | awk 'NR==2{print \$10}'" | grep -q C; }
probe() { # user app -> the probe's line, or the launch's error
    "$V" "su -m $1 -c 'portcullis launch --no-prompt $2' 2>&1" | grep -E 'vpn-probe:|portcullis launch' | tail -1
}

case "${1:-}" in
run)
    CROSS_RUSTFLAGS=$(set -- env; ARCH=amd64 . "$ROOT/scripts/cloud-vm.sh" >/dev/null; printf %s "$CROSS_RUSTFLAGS")
    cross crypta -p crypta-ctl
    cross portcullis -p portcullisd -p portcullis-cli -p opifex --bin portcullisd --bin portcullis --bin opifex
    EXTRA="$ROOT/crypta/target/$T/release/atrium-crypta-ctl $ROOT/crypta/target/$T/release/atrium-vpnctl \
        $ROOT/portcullis/target/$T/release/portcullisd $ROOT/portcullis/target/$T/release/portcullis $ROOT/portcullis/target/$T/release/opifex"
    setup
    [ -n "${KEEP:-}" ] && trap 'kill $LANPID 2>/dev/null || true' EXIT

    echo "== the front: its rc.d script, its own uid, capability mode"
    scp $K -P 2222 -q "$ROOT/crypta/etc/atrium-crypta-ctl" root@localhost:$G/rc.atrium-crypta-ctl
    "$V" "set -e
        id -u _crypta-ctl 2>/dev/null | grep -qx 65054 || { pw userdel _crypta-ctl 2>/dev/null || true; pw useradd _crypta-ctl -u 65054 -d /nonexistent -s /usr/sbin/nologin -c 'cryptad settings front'; }
        install -m 755 $G/atrium-crypta-ctl $G/atrium-vpnctl /usr/local/bin/
        install -m 555 $G/rc.atrium-crypta-ctl /usr/local/etc/rc.d/atrium-crypta-ctl
        sysrc atrium_crypta_ctl_ctl=$G/crypta.sock atrium_crypta_ctl_flags=-v >/dev/null
        service atrium-crypta-ctl onestop >/dev/null 2>&1 || true
        service atrium-crypta-ctl onestart >/dev/null
        i=0; while [ \$i -lt 20 ] && ! test -S /atrium/sockets/vpn-read/vpn-read.sock; do sleep 1; i=\$((i+1)); done"
    sleep 1
    p=$("$V" 'pgrep -x atrium-crypta-ctl')
    u=$("$V" "ps -o uid= -p $p" | tr -d ' ')
    [ "$u" = 65054 ] && capmode "$p" && ok "atrium-crypta-ctl runs as _crypta-ctl (65054) in capability mode" || fail "front: uid $u"
    m=$("$V" 'stat -f "%Lp" /atrium/sockets/vpn-read/vpn-read.sock /atrium/sockets/vpn-use/vpn-use.sock /atrium/sockets/vpn-configure/vpn-configure.sock' | tr '\n' ' ')
    [ "$m" = "666 666 666 " ] && ok "one socket per capability (who reaches each is which jails get it)" || fail "sockets: $m"
    for t in $("$V" "$VC --grant configure list" | awk '{sub("name=","",$2); print $2}'); do "$V" "$VC --grant configure remove $t" >/dev/null; done

    echo "== a new tunnel with a key made in Arca"
    SRV=$(ubuntu_peer "$DUMMY")
    [ -n "$SRV" ] && ok "Ubuntu wg0 up (server key $SRV)" || fail "Ubuntu peer"
    r=$("$V" "$VC --grant use create acme $SRV 198.51.100.51:51820 10.99.0.2/32 10.99.0.0/24 10.99.0.1" || true)
    case "$r" in "refused not-allowed"*) ok "vpn-use may not create ($r)" ;; *) fail "vpn-use create: $r" ;; esac
    r=$("$V" "$VC --grant configure create acme $SRV 198.51.100.51:51820 10.99.0.2/32 10.99.0.0/24 10.99.0.1")
    PUB=$(echo "$r" | sed -n 's/^created name=acme public=\([^ ]*\)$/\1/p')
    [ -n "$PUB" ] && ok "vpn-configure created acme; our public key $PUB" || fail "create: $r"
    "$V" "grep -q key= $G/state/acme.profile && ! grep -qi private $G/state/acme.profile" && ok "the profile names the key by its Arca id" || fail "profile: $("$V" "cat $G/state/acme.profile")"
    r=$("$V" "$VC --grant configure create acme $SRV 198.51.100.51:51820 10.99.0.2/32 10.99.0.0/24" || true)
    case "$r" in *"exists already"*) ok "a second acme is refused ($r)" ;; *) fail "duplicate: $r" ;; esac
    r=$("$V" "$VC --grant configure create ../x $SRV 198.51.100.51:51820 10.99.0.2/32 10.99.0.0/24" || true)
    case "$r" in "refused malformed"*) ok "a bad name is refused by the front" ;; *) fail "bad name: $r" ;; esac
    gssh <<EOF >/dev/null
wg set wg0 peer $DUMMY remove
wg set wg0 peer $PUB allowed-ips 10.99.0.2/32
EOF
    ok "the server's admin adds our key"

    echo "== what each capability sees"
    r=$("$V" "$VC --grant read status")
    [ "$r" = "tunnel name=acme kind=WireGuard state=Down scope=Machine" ] && ok "vpn-read: $r" || fail "read status: $r"
    for g in read use; do
        r=$("$V" "$VC --grant $g details acme" || true)
        case "$r" in "refused not-allowed"*) ok "vpn-$g may not see details" ;; *) fail "$g details: $r" ;; esac
    done
    r=$("$V" "$VC --grant read connect acme" || true)
    case "$r" in "refused not-allowed"*) ok "vpn-read may not connect" ;; *) fail "read connect: $r" ;; esac
    r=$("$V" "$VC --grant use set acme block=1" || true)
    case "$r" in "refused not-allowed"*) ok "vpn-use may not change settings" ;; *) fail "use set: $r" ;; esac
    r=$("$V" "$VC --grant use remove acme" || true)
    case "$r" in "refused not-allowed"*) ok "vpn-use may not remove" ;; *) fail "use remove: $r" ;; esac

    echo "== connect through vpn-use"
    "$V" "rm -f $B.watch; daemon -f -o $B.watch $VC --grant read watch 3"
    sleep 1
    r=$("$V" "$VC --grant use connect acme")
    [ "$r" = ok ] && ok "vpn-use: connect acme" || fail "connect: $r"
    i=0; until [ "$(state acme)" = Up ]; do i=$((i + 1)); [ $i -lt 30 ] || fail "no handshake: $("$V" "$VC --grant read status")"; sleep 1; done
    ok "acme Up after ${i}s (a handshake with Linux's wg0, our key in Arca)"
    r=$("$V" 'fetch -T 10 -qo - http://10.99.0.1/' 2>&1) && [ "$r" = crypta-v1-ok ] && ok "HTTP to 10.99.0.1 through the tunnel" || fail "HTTP: $r"
    w=$("$V" "cat $B.watch")
    case "$w" in *"acme=Down"*"acme=Up"*) ok "a subscriber heard Down, then Up ($(echo "$w" | tr '\n' ';'))" ;; *) fail "watch: $w" ;; esac
    r=$("$V" "$VC --grant read status")
    case "$r" in *198.51.100*|*10.99.*|*"$PUB"*) fail "vpn-read shows an address or key: $r" ;; *) ok "vpn-read shows no address, gateway or key" ;; esac
    d=$("$V" "$VC --grant configure details acme")
    case "$d" in *"state=Up public=$PUB endpoint=198.51.100.51:51820 addrs=10.99.0.2/32 routes=1"*) ok "vpn-configure: details ($d)" ;; *) fail "details: $d" ;; esac
    r=$("$V" "$VC --grant configure set acme block=1 personas=0")
    l=$("$V" "$VC --grant use list")
    [ "$r" = ok ] && [ "$l" = "profile name=acme state=Up scope=Machine block=1 full=0" ] && ok "vpn-configure: set; vpn-use's list: $l" || fail "set: $r / $l"
    r=$("$V" "$VC --grant use disconnect acme")
    i=0; until [ "$(state acme)" = Down ]; do i=$((i + 1)); [ $i -lt 10 ] || fail "not down"; sleep 1; done
    [ "$r" = ok ] && ok "vpn-use: disconnect" || fail "disconnect: $r"

    echo "== import a file through the front"
    PRIV=$(python3 -c 'import os,base64; print(base64.b64encode(os.urandom(32)).decode())')
    r=$(printf '[Interface]\nPrivateKey = %s\nAddress = 10.97.0.2/24\nPostUp = touch /tmp/pwned-v3\n\n[Peer]\nPublicKey = %s\nEndpoint = 198.51.100.52:51820\nAllowedIPs = 10.97.0.0/24\n' "$PRIV" "$SRV" | "$V" "$VC --grant configure import lab")
    case "$r" in "imported name=lab public="*" ignored=postup") ok "imported lab ($r)" ;; *) fail "import: $r" ;; esac
    "$V" "grep -rq '$PRIV' $G/state /var/log/atrium 2>/dev/null" && fail "the private key is on disk" || ok "the private key is in Arca only (not in cryptad's state or logs)"
    "$V" "test -e /tmp/pwned-v3" && fail "PostUp ran" || ok "PostUp ignored"
    r=$(printf 'not a wireguard file\n' | "$V" "$VC --grant configure import junk" || true)
    case "$r" in *"not a WireGuard configuration"*) ok "a bad file is refused in words ($r)" ;; *) fail "junk: $r" ;; esac
    "$V" "$VC --grant configure remove lab" >/dev/null
    [ -z "$(state lab)" ] && ok "removed lab" || fail "lab still listed"

    echo "== cryptad away and back"
    "$V" "pkill -f 'atrium-cryptad -v'"; sleep 3
    r=$("$V" "$VC --grant read status" 2>&1 || true)
    case "$r" in *refused*|*atrium-vpnctl:*) ok "cryptad gone: the front says so ($(echo "$r" | head -1))" ;; *) fail "with cryptad gone: $r" ;; esac
    "$V" "daemon -f -o $G/cryptad.log /usr/local/bin/atrium-cryptad -v --sock $G/crypta.sock --state $G/state --net-sock $G/net.sock --arca $G/arcad.sock"
    i=0; until [ "$(state acme)" = Down ]; do i=$((i + 1)); [ $i -lt 20 ] || fail "front not back"; sleep 1; done
    ok "cryptad back: the front restarted by daemon(8) and serves again (${i}s)"

    echo "== jailed apps: what each capability gives"
    "$V" 'sh -s' <<EOS
set -e
mkdir -p $B
install -m 0755 $G/portcullisd /usr/local/bin/portcullisd
install -m 0755 $G/portcullis /usr/local/bin/portcullis
install -m 0755 $G/opifex /usr/local/bin/opifex
test -f /root/of-key.pem || openssl ecparam -name prime256v1 -genkey -noout -out /root/of-key.pem
mkdir -p /etc/atrium/publishers; openssl ec -in /root/of-key.pem -pubout -out /etc/atrium/publishers/of-gate.pem 2>/dev/null
id alice >/dev/null 2>&1 || pw useradd alice -u 1011 -m -d /home/alice -s /bin/sh
for a in vpnuse:'vpn-read = true\nvpn-use = true\n' vpnconf:'vpn-configure = true\n' vpnnone:''; do
  n=\${a%%:*}; c=\${a#*:}; id=org.test.\$n
  opifex uninstall \$id >/dev/null 2>&1 || true
  O=$B/\$id; rm -rf \$O; mkdir -p \$O/bin; cp /usr/local/bin/atrium-vpnctl \$O/bin/
  printf '[app]\nid = "%s"\nname = "VPN probe"\nversion = "1"\nentry = "bin/atrium-vpnctl"\n\n[capabilities]\nnetwork = "none"\n' \$id > \$O/atrium.toml
  printf "\$c" >> \$O/atrium.toml
  openssl dgst -sha256 -sign /root/of-key.pem -out \$O/atrium.toml.sig \$O/atrium.toml
  opifex install \$O >/dev/null
done
mkdir -p /atrium/sockets/portcullis
pkill -x portcullisd || true; sleep 1; rm -f $PSOCK
daemon -f -o $B/portcullisd.log /usr/local/bin/portcullisd --socket $PSOCK
sleep 3; test -S $PSOCK; chmod 666 $PSOCK
for n in vpnuse vpnconf vpnnone; do su -m alice -c "portcullis policy grant org.test.\$n" >/dev/null 2>&1 || true; portcullis policy grant --system org.test.\$n >/dev/null 2>&1 || true; done
EOS
    r=$(probe alice org.test.vpnuse)
    [ "$r" = "vpn-probe: read.status=1tunnels read.list=not-allowed read.create=not-allowed use.status=1tunnels use.list=1profiles use.create=not-allowed configure=absent" ] \
        && ok "vpn-read + vpn-use app: sees and connects, cannot configure ($r)" || fail "vpnuse: $r ($("$V" "tail -3 $B/portcullisd.log"))"
    r=$(probe alice org.test.vpnconf)
    case "$r" in "vpn-probe: read=absent use=absent configure.status=1tunnels configure.list=1profiles configure.create=made("*")") ok "vpn-configure app: makes a tunnel ($r)" ;; *) fail "vpnconf: $r" ;; esac
    r=$(probe alice org.test.vpnnone)
    [ "$r" = "vpn-probe: read=absent use=absent configure=absent" ] && ok "an app with no vpn capability reaches none of the sockets ($r)" || fail "vpnnone: $r"
    [ -z "$(state probe-made)" ] && ok "and the probe's tunnel is gone again" || fail "probe-made left"
    echo "crypta V3 gate: $pass checks passed"
    ;;
ui)
    # After `KEEP=1 ... run`: cryptad, the front and acme are still there.
    # A headless frescod, forum-wm, the bar and Settings (unjailed, against
    # the same /atrium/sockets); clicks through frescod's inject socket.
    U=/var/tmp/crypta-v3-ui; FS=$U/fresco.sock
    # The LAN again (KEEP's exit stopped it; the guest's resolver forwards to it).
    pgrep -f atrium-net/sim/lan.py >/dev/null || { python3 "$ROOT/atrium-net/sim/lan.py" 3600 > "$LAN_LOG" 2>&1 & sleep 15; }
    mkdir -p "$OUT"
    "$V" "pkill -x 'curia|forum-bar|forum-wm|frescod' || true; sleep 1; rm -rf $U; mkdir -p $U /usr/local/share/fonts/ibm-plex"
    for b in curia forum-bar forum-wm frescod; do scp $K -P 2222 -q "$ROOT/$b/target/$T/release/$b" root@localhost:$U/; done
    scp $K -P 2222 -q "$ROOT/fonts/ibm-plex/IBMPlexSans.ttf" "$ROOT/fonts/phosphor/Phosphor.ttf" root@localhost:/usr/local/share/fonts/ibm-plex/
    E="FRESCO_SOCKET=$FS FRESCO_WM_SOCKET=$U/wm.sock FORUM_CTL_SOCKET=$U/ctl.sock"
    "$V" "cd $U && env FRESCOD_SPLIT=0 FRESCOD_HEADLESS_PNG=$U/screen FRESCOD_SOCK=$FS FRESCOD_WM_SOCK=$U/wm.sock FRESCOD_INPUT_SOCK=$U/inject.sock \
            FRESCOD_ACTIVATION_SOCK=$U/activation.sock FRESCOD_DEV_CAPS=all FRESCOD_INJECT_ACTIVATION=1 FRESCOD_CHROME_UIDS=1011 daemon -f -o $U/frescod.log $U/frescod </dev/null
        i=0; while [ \$i -lt 60 ] && ! test -S $FS; do sleep 2; i=\$((i+1)); done
        env $E daemon -f -o $U/wm.log $U/forum-wm </dev/null; sleep 4; chmod 666 $U/*.sock; chmod 755 $U
        env $E FORUM_BAR_NET_DIR=/atrium/sockets daemon -f -u alice -o $U/bar.log $U/forum-bar </dev/null
        env $E CURIA_SIZE=1100x700 daemon -f -u alice -o $U/curia.log $U/curia </dev/null"
    inj() { "$V" "printf '%s\n' $(printf "'%s' " "$@") | nc -N -U $U/inject.sock" >/dev/null; }
    shot() { sleep "${GAP:-15}"; "$V" "cp $U/screen.png $U/s.png 2>/dev/null || cp $U/screen $U/s.png"; scp $K -P 2222 -q root@localhost:$U/s.png "$OUT/$1.png" && echo "frame $OUT/$1.png"; }
    shot start
    [ -n "${VPN_XY:-}" ] && { inj "MOVE $VPN_XY" "BTN 1 1" "BTN 1 0"; shot vpn-page; }
    [ -n "${BAR_XY:-}" ] && { inj "MOVE $BAR_XY" "BTN 1 1" "BTN 1 0"; shot bar-menu; }
    ;;
*)
    echo "usage: $0 run|ui" >&2
    exit 2
    ;;
esac
