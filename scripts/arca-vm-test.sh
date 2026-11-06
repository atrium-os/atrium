#!/bin/sh
# Arca A0 gates in the FreeBSD dev VM (docs/spec/arca.md §11).
#
#   sh scripts/cloud-vm.sh up        # VM running, Portcullis stack deployed
#   sh scripts/arca-vm-test.sh       # cross-build, deploy, run the gates
#
# The VM's jaild must be current (allow.mlock): on a fresh VM, cross-build
# portcullis and run scripts/deploy-portcullis.sh first.
#
# 1. The arca-store and arcad test suites, as root and as an unprivileged
#    user (mlock limits differ).
# 2. arcad as a jailed system service: uid _arca, no network, untraceable.
# 3. Identity over the real socket: app B gets EACCES on app A's item, an
#    unregistered uid gets nothing, only ostiarius's uid unlocks and locks,
#    and a while-unlocked item is unreadable after lock.
set -eu
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
set -- env
. "$ROOT/scripts/cloud-vm.sh" >/dev/null
set --
KEY="$HOME/.ssh/fresco_bsd_ed25519"
SSHOPT="-i $KEY -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o LogLevel=ERROR"
g() { ssh $SSHOPT -o ConnectTimeout=30 -p 2222 root@localhost "$@"; }
put() { scp $SSHOPT -P 2222 -q "$1" "root@localhost:$2"; }
pass=0; fail=0
check() { # name expected actual
    if [ "$2" = "$3" ]; then echo "  PASS $1"; pass=$((pass+1)); else echo "  FAIL $1: expected [$2] got [$3]"; fail=$((fail+1)); fi
}

echo "=== cross-build"
BINS=""
for c in arca/arca-store arca/arcad arca/arca-ui arca/arca-courier; do
    out=$(cd "$ROOT/$c" && RUSTFLAGS="$CROSS_RUSTFLAGS" cargo test --release --target "$TARGET" --no-run --message-format=json 2>/dev/null \
        | python3 -c 'import sys,json
for l in sys.stdin:
    m=json.loads(l)
    if m.get("reason")=="compiler-artifact" and m.get("executable") and m["profile"]["test"]: print(m["executable"])')
    BINS="$BINS $out"
done
(cd "$ROOT/arca/arcad" && RUSTFLAGS="$CROSS_RUSTFLAGS" cargo build -q --release --target "$TARGET" --bins)
REL="$ROOT/arca/arcad/target/$TARGET/release"
(cd "$ROOT/arca/arca-courier" && RUSTFLAGS="$CROSS_RUSTFLAGS" cargo build -q --release --target "$TARGET" --bins --examples)
CREL="$ROOT/arca/arca-courier/target/$TARGET/release"

echo "=== deploy"
g "mkdir -p /root/arca-test && rm -f /root/arca-test/*"
for b in $BINS "$REL/arcad" "$REL/arca-probe" "$CREL/arca-courier" "$CREL/examples/courier-vm-probe"; do
    put "$b" /root/arca-test/
    h=$(sha256sum "$b" | cut -d' ' -f1)
    gh=$(g "sha256 -q /root/arca-test/$(basename "$b")")
    [ "$h" = "$gh" ] || { echo "ABORT: hash mismatch for $(basename "$b")"; exit 1; }
done

echo "=== 1. test suites"
if [ "${SKIP_SUITES:-0}" != 1 ]; then
for b in $BINS; do
    n=$(basename "$b")
    r=$(g "cd /tmp && /root/arca-test/$n -q 2>&1 | grep -E '^test result' | tail -1")
    case "$r" in *" 0 failed"*) check "root: $n" ok ok ;; *) check "root: $n" ok "$r" ;; esac
done
g "pw usershow arcatest >/dev/null 2>&1 || pw useradd arcatest -u 1999 -d /nonexistent -s /usr/sbin/nologin"
g "mkdir -p /tmp/arcatest && cp /root/arca-test/* /tmp/arcatest/ && chown -R arcatest /tmp/arcatest"
for b in $BINS; do
    n=$(basename "$b")
    r=$(g "cd /tmp/arcatest && su -m arcatest -c 'TMPDIR=/tmp/arcatest ./$n -q' 2>&1 | grep -E '^test result' | tail -1")
    case "$r" in *" 0 failed"*) check "unprivileged: $n" ok ok ;; *) check "unprivileged: $n" ok "$r" ;; esac
done

fi
echo "=== 2. arcad jailed"
J=/var/lib/atrium/jails/arcad
# A previous run's arcad holds the binary (ETXTBSY) and its mounts.
g "service atrium-portcullisd-bootstrap onestop >/dev/null 2>&1; sleep 3; pkill -x atrium-arcad; sleep 1; true"
# deploy-portcullis.sh can't replace the bootstrap while it runs (ETXTBSY);
# it reads allow_mlock from the manifest, so install the fresh one here.
BOOT="$ROOT/portcullis/target/$TARGET/release/atrium-portcullisd-bootstrap"
if [ -x "$BOOT" ]; then
    put "$BOOT" /tmp/atrium-portcullisd-bootstrap
    g "install -m 755 /tmp/atrium-portcullisd-bootstrap /usr/local/bin/atrium-portcullisd-bootstrap"
fi
g "pw usershow _arca >/dev/null 2>&1 || pw useradd _arca -u 65020 -d /nonexistent -s /usr/sbin/nologin"
g "pw usershow _ostiarius >/dev/null 2>&1 || pw useradd _ostiarius -u 50093 -d /nonexistent -s /usr/sbin/nologin"
g "pw usershow _arca-ui >/dev/null 2>&1 || pw useradd _arca-ui -u 65021 -d /nonexistent -s /usr/sbin/nologin"
g "install -m 755 /root/arca-test/arcad /usr/local/bin/atrium-arcad && install -m 755 /root/arca-test/arca-probe /usr/local/bin/arca-probe && install -m 755 /root/arca-test/arca-courier /usr/local/bin/arca-courier"
g "mkdir -p /var/db/arca /atrium/sockets/arca /var/run/atrium && chown _arca:_arca /var/db/arca /atrium/sockets/arca && chmod 700 /var/db/arca"
g "touch /var/run/atrium/app-registry /var/run/atrium/courier-registry"
g "mkdir -p $J/dev $J/libexec $J/lib $J/usr/lib $J/usr/local/lib $J/usr/local/bin $J/atrium/sockets/arca $J/var/db/arca $J/var/run/atrium && for f in app-registry courier-registry; do test -e $J/var/run/atrium/\$f || touch $J/var/run/atrium/\$f; done"
# jaild policy: the repo's (which carries _arca and Arca's mount sources),
# merged with the live one exactly as deploy-portcullis.sh does.
tmpd=$(mktemp -d)
g 'cat /etc/atrium/jaild.policy.toml' > "$tmpd/live.toml"
sh "$ROOT/scripts/jaild-policy-merge.sh" "$ROOT/etc/jaild.policy.toml" "$tmpd/live.toml" > "$tmpd/jaild.policy.toml" 2>/dev/null
put "$tmpd/jaild.policy.toml" /etc/atrium/jaild.policy.toml
put "$ROOT/arca/arcad/etc/services.d/42-atrium-arcad.toml" /etc/atrium/services.d/42-atrium-arcad.toml
# Only Arca's manifest: the smoke manifests would churn the bootstrap log.
g "mkdir -p /etc/atrium/services.d.off && mv /etc/atrium/services.d/*-atrium-smoke-*.toml /etc/atrium/services.d.off/ 2>/dev/null; true"
g "service atrium-jaild restart >/dev/null 2>&1; sleep 2; service atrium-portcullisd-bootstrap onestart >/dev/null 2>&1; true"
i=0; while [ $i -lt 60 ] && ! g "test -S /atrium/sockets/arca/arcad.sock"; do sleep 2; i=$((i+1)); done
check "socket bound" yes "$(g 'test -S /atrium/sockets/arca/arcad.sock && echo yes || echo no')"
PID=$(g "pgrep -x atrium-arcad | head -1" || true)
check "runs as _arca" _arca "$(g "ps -o user= -p $PID" | tr -d ' ')"
JID=$(g "ps -o jid= -p $PID" | tr -d ' ')
check "in a jail" yes "$( [ "${JID:-0}" != 0 ] && echo yes || echo no)"
check "jail may lock memory (allow.mlock)" true "$(g "jls -j $JID allow.mlock" | tr -d ' ')"
[ "$(g "jls -j $JID allow.mlock" | tr -d ' ')" = true ] || echo "  hint: the deployed jaild predates allow.mlock; cross-build portcullis and run scripts/deploy-portcullis.sh"
check "no network in the jail" "" "$(g "jls -j $JID ip4.addr ip6.addr 2>/dev/null | tr -d ' -'" || true)"
check "untraceable, even by root (procctl)" refused "$(g "timeout 5 truss -p $PID >/dev/null 2>&1 && echo allowed || echo refused")"

echo "=== 3. identity over the socket"
S=/atrium/sockets/arca/arcad.sock
g "pw usershow arcaapp1 >/dev/null 2>&1 || pw useradd arcaapp1 -u 50801 -d /nonexistent -s /usr/sbin/nologin"
g "pw usershow arcaapp2 >/dev/null 2>&1 || pw useradd arcaapp2 -u 50802 -d /nonexistent -s /usr/sbin/nologin"
PUB=$(printf 'a%.0s' $(seq 64))
g "grep -q '^50801 ' /var/run/atrium/app-registry || printf '50801 alice org.example.mail $PUB\n50802 alice org.example.evil $PUB\n' >> /var/run/atrium/app-registry"
P=/usr/local/bin/arca-probe
as() { g "su -m $1 -c '$P $S $2'"; }
check "root is no one" "status=5 body=" "$(g "$P $S list")"
check "app can't unlock" "status=1 body=" "$(as arcaapp1 'state alice')"
r=$(as _ostiarius 'unlock alice correct-horse')
case "$r" in unlocked*) check "ostiarius unlocks (first login creates)" ok ok ;; *) check "ostiarius unlocks" ok "$r" ;; esac
ID=$(as arcaapp1 'store hunter2' | sed -n 's/^status=0 body=//p')
check "app stores its secret" 32 "${#ID}"
check "app-held needs the user's grant" "status=8 body=" "$(as arcaapp1 "release $ID")"
check "another app: EACCES" "status=1 body=" "$(as arcaapp2 "release $ID")"
check "another app lists nothing" "status=0 body=00000000" "$(as arcaapp2 list)"
M=$(as arcaapp1 'mint chat.example' | sed -n 's/^status=0 body=//p')
KID=$(echo "$M" | cut -c1-32)
check "key minted (id + public key)" 96 "${#M}"
check "the app signs" 0 "$(as arcaapp1 "sign $KID hello" | sed -n 's/^status=\([0-9]*\).*/\1/p')"
check "another app can't sign" "status=1 body=" "$(as arcaapp2 "sign $KID hello")"
# Arca's own UI (arca-ui, uid 65021): the chip's list and a generated
# password, for the parent it names; no app may ask.
r=$(as _arca-ui "accounts alice $PUB org.example.mail")
# At least one (earlier runs' items stay in /var/db/arca).
case "$r" in "status=0 body=00000000"*) check "arca-ui lists the app's accounts" ok "$r" ;; "status=0 body="?*) check "arca-ui lists the app's accounts" ok ok ;; *) check "arca-ui lists the app's accounts" ok "$r" ;; esac
check "another publisher's app: nothing" "status=0 body=00000000" "$(as _arca-ui "accounts alice $(printf 'b%.0s' $(seq 64)) org.example.mail")"
check "arca-ui generates (a handle)" 32 "$(as _arca-ui "generate alice $PUB org.example.mail" | sed -n 's/^status=0 body=//p' | tr -d '\n' | wc -c | tr -d ' ')"
check "an app can't ask for the chip" "status=1 body=" "$(as arcaapp1 "accounts alice $PUB org.example.mail")"
check "app can't lock" "status=1 body=" "$(as arcaapp1 'lock alice')"
check "ostiarius locks" "status=0 body=" "$(as _ostiarius 'lock alice')"
check "while-unlocked gone after lock" "status=1 body=" "$(as arcaapp1 "sign $KID hello")"
r=$(as _ostiarius 'unlock alice wrong-horse')
case "$r" in *WRONG_ROOT*|*"Refused(9)"*) check "wrong passcode refused" ok ok ;; *) check "wrong passcode refused" ok "$r" ;; esac
r=$(as _ostiarius 'unlock alice correct-horse')
case "$r" in *Existing*) check "right passcode unlocks again" ok ok ;; *) check "right passcode unlocks again" ok "$r" ;; esac
check "and the key works again" 0 "$(as arcaapp1 "sign $KID hello" | sed -n 's/^status=\([0-9]*\).*/\1/p')"
check "audit log has no value" 0 "$(g 'grep -c hunter2 /var/db/arca/audit.log' || true)"
check "nothing in clear on disk" 0 "$(g 'grep -rl hunter2 /var/db/arca | wc -l' | tr -d ' ')"

echo "=== 4. the courier (A1)"
# Two couriers for mail's persona: one bound to api.example, one to
# evil.example. Their labels come from the courier registry by uid.
MP=$(g "arca-probe - persona $PUB org.example.mail")
g "pw usershow arcacourier1 >/dev/null 2>&1 || pw useradd arcacourier1 -u 64001 -d /nonexistent -s /usr/sbin/nologin"
g "pw usershow arcacourier2 >/dev/null 2>&1 || pw useradd arcacourier2 -u 64002 -d /nonexistent -s /usr/sbin/nologin"
g "printf '64001 alice $MP https api.example 443\n64002 alice $MP https evil.example 443\n' > /var/run/atrium/courier-registry"
as _ostiarius 'unlock alice correct-horse' >/dev/null
TID=$(as arcaapp1 'store-for api.example hunter2-token' | sed -n 's/^status=0 body=//p')
check "app stores a token for api.example" 32 "${#TID}"
check "the app can't read it back" "status=1 body=" "$(as arcaapp1 "release $TID")"
CENV="env ARCA_COURIER_CA=/tmp/courier-ca.pem ARCA_COURIER_GRANT_PRIVATE_NETWORK=1 ARCA_SOCKET=$S su -m"
r=$(g "/root/arca-test/courier-vm-probe api.example /tmp/courier-ca.pem $TID \"$CENV arcacourier1 -c\"")
echo "$r" | sed 's/^/    /'
check "courier: the server got the token over TLS" true "$(echo "$r" | sed -n 's/^server_got_token=//p')"
check "courier: 200 to the client" 200 "$(echo "$r" | sed -n 's/^status=//p')"
check "courier: the echoed token is masked" "echo *************" "$(echo "$r" | sed -n 's/^body=//p')"
check "courier: Set-Cookie kept from the client" true "$(echo "$r" | sed -n 's/^cookie_hidden=//p')"
check "courier: a redirect comes back, not followed" "302 https://evil.example/" "$(echo "$r" | sed -n 's/^redirect=//p')"
check "courier: two requests at the server, no more" 2 "$(echo "$r" | sed -n 's/^requests_at_server=//p')"
r=$(as arcaapp1 'list-for api.example')
case "$r" in *"$(printf 'sid' | od -An -tx1 | tr -d ' \n')"*) check "the cookie is a record in Arca" ok ok ;; *) check "the cookie is a record in Arca" ok "$r" ;; esac
r=$(g "/root/arca-test/courier-vm-probe evil.example /tmp/courier-ca.pem $TID \"$CENV arcacourier2 -c\"")
check "courier for evil.example: Arca denies the token" "refused=Denied" "$(echo "$r" | head -1)"
check "and evil.example's server got no token" false "$(echo "$r" | sed -n 's/^server_got_token=//p')"
check "a courier uid is no app" "status=1 body=" "$(as arcacourier1 'store hunter3')"

echo "=== $pass passed, $fail failed"
[ "$fail" = 0 ]
