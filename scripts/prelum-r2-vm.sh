#!/bin/sh
# Prelum R2 in the FreeBSD VM (docs/spec/printing.md §11): prelumd with its
# workers jailed by jaild (`[print_workers]`), printing to CUPS's
# ippeveprinter on this host (a test oracle, never shipped), which the guest
# reaches at slirp's 10.0.2.2.
#
#   sh scripts/cloud-vm.sh up          # (setup once) the VM, ssh, the portcullis stack
#   sh scripts/prelum-r2-vm.sh
#
# Checks: a PDF job to an IPPS printer and a raster job to a plain-IPP one,
# each worker in its own jail (render: no network; ipp: a vnet whose pf
# anchor passes only the printer's address and port), every worker jail gone
# afterwards; a printer with a new key holds the job until it is pinned
# again; cancel while printing reaches the printer.
#
# Cross-builds jaild (its policy gained [print_workers]) and the prelum
# binaries on this host, then deploys them; never builds in the guest.
set -u
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
VMDIR="${VMDIR:-$HOME/atrium-vm}"
VSSH="$ROOT/scripts/vssh"
KEY="$HOME/.ssh/fresco_bsd_ed25519"
SCP="scp -i $KEY -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o LogLevel=ERROR -P 2222"
TARGET=aarch64-unknown-freebsd
HOSTIP=10.0.2.2
work=$(mktemp -d)
FAILS=0
STAGE=startup
pids=""
port_up() { python3 -I -c "import socket,sys; socket.create_connection(('127.0.0.1', int(sys.argv[1])), 1)" "$1" 2>/dev/null; }
say() { printf '\n=== %s\n' "$*"; }
die() { printf '\nFAILED [%s]: %s\n' "$STAGE" "$*" >&2; exit 1; }
check() { # check <label> <ok 0/1> <detail>
    if [ "$2" = 0 ]; then printf 'PASS %-58s %s\n' "$1" "$3"; else printf 'FAIL %-58s %s\n' "$1" "$3"; FAILS=$((FAILS+1)); fi
}
g() { "$VSSH" "$@"; }
cleanup() {
    for p in $pids; do kill "$p" 2>/dev/null; done
    g 'pkill -f "prelumd --socket /tmp/prelum.sock"' >/dev/null 2>&1
    [ -n "${KEEP:-}" ] && echo "work dir: $work" || rm -rf "$work"
}
trap cleanup EXIT INT TERM

STAGE=build
say "$STAGE (cross, on this host)"
CROSS_RUSTFLAGS=$(cd "$ROOT" && sh -c '. ./scripts/cloud-vm.sh >/dev/null 2>&1; printf %s "$CROSS_RUSTFLAGS"' sh env)
[ -n "$CROSS_RUSTFLAGS" ] || die "cloud-vm.sh env"
# ring's C and assembly, for the guest.
export CC_aarch64_unknown_freebsd=clang AR_aarch64_unknown_freebsd=llvm-ar
export CFLAGS_aarch64_unknown_freebsd="--target=aarch64-unknown-freebsd --sysroot=$VMDIR/sysroot"
(cd "$ROOT/prelum" && RUSTFLAGS="$CROSS_RUSTFLAGS" cargo build -q --release --target $TARGET -p prelumd -p prelum-ipp -p prelum-render --bins) || die "prelum cross-build"
(cd "$ROOT/portcullis" && TESSERA_CORE_LIB="$ROOT/atrium-tessera/core" RUSTFLAGS="$CROSS_RUSTFLAGS" cargo build -q --release --target $TARGET -p jaild) || die "jaild cross-build"

STAGE=deploy
say "$STAGE"
g true || die "no ssh to the VM (sh scripts/cloud-vm.sh up)"
# deploy-portcullis.sh now and then reports portcullisd "already running"
# on restart; what matters here is jaild running with the new policy.
START=1 sh "$ROOT/scripts/deploy-portcullis.sh" >"$work/deploy.log" 2>&1 \
    || grep -q "atrium-jaild  *RUNNING" "$work/deploy.log" \
    || { tail -20 "$work/deploy.log"; die "deploy-portcullis"; }
g 'grep -q "^\[print_workers\]" /etc/atrium/jaild.policy.toml' || die "jaild's policy has no [print_workers]"
B="$ROOT/prelum/target/$TARGET/release"
g 'mkdir -p /root/prelum'
for b in atrium-prelumd prelumctl atrium-prelum-ipp atrium-prelum-render; do
    $SCP "$B/$b" root@localhost:/root/prelum/$b >/dev/null || die "scp $b"
    want=$(sha256sum "$B/$b" | cut -d' ' -f1)
    got=$(g "sha256 -q /root/prelum/$b")
    [ "$want" = "$got" ] || die "$b arrived damaged (scp under TCG)"
done
g 'pkill -f "prelumd --socket /tmp/prelum.sock"; sleep 1; for b in atrium-prelumd prelumctl atrium-prelum-ipp atrium-prelum-render; do install -m 0555 /root/prelum/$b /usr/local/bin/$b; done' || die "install"
$SCP "$ROOT/prelum/prelum-ipp-codec/examples/min.pdf" root@localhost:/root/prelum/min.pdf >/dev/null || die "scp pdf"

STAGE=printers
say "$STAGE (ippeveprinter on this host)"
command -v ippeveprinter >/dev/null || die "ippeveprinter not installed (cups-ipp-utils)"
if ! avahi-daemon -c 2>/dev/null; then
    mkdir -p /run/dbus
    dbus-daemon --system --fork 2>/dev/null || { rm -f /run/dbus/pid /run/dbus/system_bus_socket; dbus-daemon --system --fork; }
    rm -f /var/run/avahi-daemon/pid; avahi-daemon -D; sleep 1
fi
preload=""
if [ ! -e /proc/net/if_inet6 ]; then
    cc -shared -fPIC -o "$work/noipv6.so" "$ROOT/scripts/prelum-oracle-noipv6.c" -ldl
    preload="$work/noipv6.so"
fi
printf '#!/bin/sh\nsleep 120\n' >"$work/slow.sh"; chmod +x "$work/slow.sh"
printer() { # NAME PORT DIR KEYS ARGS...
    n=$1 p=$2 d=$3 k=$4; shift 4
    mkdir -p "$d" "$k"
    LD_PRELOAD=$preload ippeveprinter -p "$p" -n 127.0.0.1 -d "$d" -K "$k" -k "$@" "$n" >>"$d.log" 2>&1 &
    last=$!; pids="$pids $last"
    for i in $(seq 50); do port_up "$p" && return 0; sleep 0.2; done
    die "printer $n did not start"
}
printer "Prelum VM A" 8661 "$work/a" "$work/keys-a" -2 -f application/pdf,image/pwg-raster -M Atrium -m "VM A"; pid_a=$last
printer "Prelum VM B" 8662 "$work/b" "$work/keys-b" -f image/pwg-raster -M Atrium -m "VM B"
printer "Prelum VM C" 8663 "$work/c" "$work/keys-c" -f application/pdf -c "$work/slow.sh" -M Atrium -m "VM C"

STAGE=prelumd
say "$STAGE (jailed workers)"
g 'rm -rf /tmp/prelum-state /tmp/prelum-spool; daemon -f -o /tmp/prelumd.log atrium-prelumd --socket /tmp/prelum.sock --state /tmp/prelum-state --spool /tmp/prelum-spool </dev/null >/dev/null 2>&1; for i in $(seq 120); do [ -S /tmp/prelum.sock ] && exit 0; sleep 0.5; done; exit 1' || die "prelumd did not start: $(g 'cat /tmp/prelumd.log')"
ctl() { g "prelumctl --socket /tmp/prelum.sock $*"; }
phase() { ctl jobs | awk -v j="$1" '$1 == j { print $4 }'; }
wait_phase() { # JOB PHASE SECONDS
    t0=$(date +%s)
    while [ $(( $(date +%s) - t0 )) -lt "$3" ]; do
        [ "$(phase "$1")" = "$2" ] && return 0
        sleep 2
    done
    echo "  job $1: $(ctl jobs | awk -v j="$1" '$1 == j')"
    return 1
}
# Watch the guest's jails and pf anchors while jobs run. Render jails live
# for well under a second under TCG, so jls is sampled every 0.1 s; each
# networked jail's anchor is logged once per distinct rule set (jaild
# reuses names).
watch_jails() {
    g 'seen=" "
       for i in $(seq 3000); do
         jls -n name vnet ip4 2>/dev/null | grep "name=system-prelum-" | sed "s/^/JAIL /"
         for j in $(jls -n name vnet 2>/dev/null | grep "vnet=new" | sed -n "s/^name=\(system-prelum-[0-9]*\) .*/\1/p"); do
           r=$(pfctl -a "atrium/$j" -sr 2>/dev/null)
           [ -n "$r" ] || continue
           k="$j:$(echo "$r" | cksum | cut -d" " -f1)"
           case "$seen" in *" $k "*) continue;; esac
           echo "$r" | sed "s/^/RULE $j /"; seen="$seen$k "
         done
         sleep 0.1
       done' >"$work/jails.log" 2>/dev/null &
    watcher=$!; pids="$pids $watcher"
}
watch_jails

STAGE=pdf
say "$STAGE (IPPS printer A)"
out=$(ctl add ipps://$HOSTIP:8661/ipp/print)
echo "$out" | sed 's/^/  /'
fp=$(echo "$out" | sed -n 's/^ *key //p')
want="SHA256:$(openssl x509 -in "$work/keys-a/127.0.0.1.crt" -pubkey -noout | openssl pkey -pubin -outform der | openssl dgst -sha256 -binary | base64 | tr -d '=')"
check "A added over IPPS from a jail, key pinned" "$([ "$fp" = "$want" ]; echo $?)" "$fp"
j=$(ctl print /root/prelum/min.pdf --printer 1 | sed -n 's/^job //p')
wait_phase "$j" Done 300; check "PDF job $j printed on A" $? ""
f=$(ls "$work/a"/*.pdf 2>/dev/null | head -1)
check "A received a PDF from the guest" "$([ -n "$f" ] && head -c 5 "$f" | grep -q '%PDF-'; echo $?)" "$(wc -c <"$f" 2>/dev/null) bytes"

STAGE=raster
say "$STAGE (plain-IPP printer B)"
ctl add ipp://$HOSTIP:8662/ipp/print B >/dev/null || die "add B"
j=$(ctl print /root/prelum/min.pdf --printer B --gray --quality draft | sed -n 's/^job //p')
wait_phase "$j" Done 600; check "raster job $j printed on B" $? ""
f=$(ls "$work/b"/* 2>/dev/null | head -1)
check "B received PWG Raster drawn in the render jail" "$([ "$(head -c 4 "$f" 2>/dev/null)" = RaS2 ]; echo $?)" "$(wc -c <"$f" 2>/dev/null) bytes"

STAGE=jails
say "$STAGE"
kill "$watcher" 2>/dev/null
# A worker's jail, as watched: render jails have no vnet and ip4 disabled;
# ipp jails have their own vnet and an anchor passing only one printer.
# jaild reuses a name once its jail is gone, so a line is judged on its own.
render_jails=$(grep "^JAIL" "$work/jails.log" | grep "vnet=inherit ip4=disable" | sort -u | grep -c .)
ipp_jails=$(grep "^JAIL" "$work/jails.log" | grep "vnet=new ip4=inherit" | awk '{print $2}' | sort -u)
other=$(grep "^JAIL" "$work/jails.log" | grep -v "vnet=inherit ip4=disable" | grep -vc "vnet=new ip4=inherit")
check "render ran in jails without a network" "$([ "$render_jails" -gt 0 ] && [ "$other" = 0 ]; echo $?)" "seen ip4=disable, $other jails otherwise"
check "prelum-ipp ran in vnet jails" "$([ -n "$ipp_jails" ]; echo $?)" "$(echo $ipp_jails | wc -w) seen"
passes=$(grep "^RULE" "$work/jails.log" | grep " pass " | sed 's/^RULE [^ ]* //' | sort -u)
echo "$passes" | sed 's/^/  pass: /'
bad=$(echo "$passes" | grep -v "proto tcp from [0-9.]* to $HOSTIP port = 866[123] flags S/SA keep state\$" | grep -c .)
check "ipp anchors pass only a printer's address and port" "$([ -n "$passes" ] && [ "$bad" = 0 ]; echo $?)" "$(echo "$passes" | grep -c .) pass rules, $bad other"
sleep 20
left=$(g 'jls name | grep -c "^system-prelum-"')
check "every worker jail is gone after its job" "$([ "$left" = 0 ]; echo $?)" "$left left"

STAGE=identity
say "$STAGE (A with a new key)"
kill "$pid_a"; wait "$pid_a" 2>/dev/null
rm -rf "$work/keys-a"
printer "Prelum VM A" 8661 "$work/a" "$work/keys-a" -2 -f application/pdf,image/pwg-raster -M Atrium -m "VM A"
n0=$(ls "$work/a" | wc -l)
j=$(ctl print /root/prelum/min.pdf --printer 1 | sed -n 's/^job //p')
wait_phase "$j" Held 300; check "job $j held: A's key changed" $? ""
check "A received nothing while held" "$([ "$(ls "$work/a" | wc -l)" = "$n0" ]; echo $?)" ""
ctl repin 1 >/dev/null
wait_phase "$j" Done 300; check "job $j printed after the new identity was accepted" $? ""

STAGE=cancel
say "$STAGE (slow printer C)"
ctl add ipps://$HOSTIP:8663/ipp/print C >/dev/null || die "add C"
j=$(ctl print /root/prelum/min.pdf --printer C | sed -n 's/^job //p')
wait_phase "$j" Printing 300; check "job $j printing on C" $? ""
ctl cancel "$j" >/dev/null
wait_phase "$j" Cancelled 120; check "job $j cancelled" $? ""
sleep 5
check "C heard Cancel-Job from the guest" "$(grep -q 'Cancel-Job successful-ok' "$work/c.log"; echo $?)" ""

echo
if [ "$FAILS" = 0 ]; then
    echo "PASS: Prelum R2 in the VM"
else
    echo "FAILED: $FAILS"; echo "--- guest prelumd log"; g 'tail -30 /tmp/prelumd.log'; KEEP=1; exit 1
fi
