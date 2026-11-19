#!/bin/sh
# Called by crypta-gp-packet-check.py --full-vm or --portal-vm. Requires a dedicated,
# already-booted FreeBSD guest with the atrium-net simulated LAN on vtnet1.
set -eu
ROOT=$(cd "$(dirname "$0")/.." && pwd)
case "$1" in
    arm64) CRYPTA_GATE_TARGET=aarch64-unknown-freebsd; sysroot=sysroot ;;
    amd64) CRYPTA_GATE_TARGET=x86_64-unknown-freebsd; sysroot=sysroot-amd64 ;;
    *) exit 2 ;;
esac
export CRYPTA_GATE_TARGET
export CRYPTA_GATE_CONFIGURED_TOOLCHAIN=1 CRYPTA_GATE_OWN_LAN=1
export CRYPTA_GP_TEST_PORT=$2
trust=$3
portal_port=${4:-}
transport=${5:-}
hip=${6:-}
receipts=${7:-}
if [ -n "$transport" ]; then
    export CRYPTA_GP_TEST_ESP=4501
fi
if [ -n "$portal_port" ]; then
    export CRYPTA_GP_PORTAL_PORT=$portal_port
fi
headers="$ROOT/$sysroot"
[ -d "$headers/usr/include" ] || headers="$HOME/src/bsd/$sysroot"
llvm=${CRYPTA_LLVM_BIN:-/opt/homebrew/opt/llvm/bin}
suffix=$(printf %s "$CRYPTA_GATE_TARGET" | tr - _)
export "CC_$suffix=$llvm/clang" "AR_$suffix=$llvm/llvm-ar" "CFLAGS_$suffix=--sysroot=$headers"
CRYPTA_GATE_LIB=1 . "$ROOT/scripts/crypta-v1-gate.sh"
"$V" 'ifconfig vtnet1 >/dev/null' || fail "gate guest needs a simulated LAN on vtnet1"
(cd "$ROOT/crypta" && cargo build -q --release --target "$T" -p crypta-engine --bin atrium-crypta-ac)
(cd "$ROOT/arca/arcad" && cargo build -q --release --target "$T" --bin arca-probe)
EXTRA="$ROOT/crypta/target/$T/release/atrium-crypta-ac $ROOT/arca/arcad/target/$T/release/arca-probe"
setup

"$V" 'sh -s' <<'GUEST'
set -eu
G=/tmp/crypta-gate
install -m 755 "$G/atrium-crypta-ac" /usr/local/bin/atrium-crypta-ac
for user in cg-vest cg-ui; do
    id "$user" >/dev/null 2>&1 || pw useradd "$user" -d /nonexistent -s /usr/sbin/nologin
done
pkill -f '^/usr/local/bin/atrium-arcad-gate' || true
sleep 1
env ARCA_DIR="$G/arca" ARCA_SOCKET="$G/arcad.sock" ARCA_APP_REGISTRY="$G/apps.registry" \
    ARCA_SERVICES=cryptad=65052 ARCA_SIGILLUM_SOCKET=none \
    ARCA_VESTIBULUM_UID="$(id -u cg-vest)" ARCA_UI_UID="$(id -u cg-ui)" \
    daemon -f -o "$G/arcad.log" /usr/local/bin/atrium-arcad-gate
sleep 1
su -m cg-vest -c "$G/arca-probe $G/arcad.sock unlock alice correct-horse" | grep -q '^unlocked '
GUEST

field() {
    "$V" "$CTL list" | awk -v key="$1" '
        /^tunnel / && / name=gp-gate( |$)/ {
            for (i = 2; i <= NF; i++) {
                split($i, pair, "=")
                if (pair[1] == key) print pair[2]
            }
        }'
}
wait_field() {
    for _ in $(seq 1 30); do
        [ "$(field "$1")" != "$2" ] || return 0
        [ -z "$(field error)" ] || break
        sleep 1
    done
    fail "profile did not reach $1=$2: $("$V" "$CTL list")"
}
hex() { printf %s "$1" | od -An -tx1 | tr -d ' \n'; }
wait_prompt() {
    expected=$(hex "$1")
    for _ in $(seq 1 30); do
        prompt=$("$V" "$CTL prompt name=gp-gate" || true)
        if printf '%s\n' "$prompt" | grep -q " name=$expected "; then return 0; fi
        [ -z "$(field error)" ] || break
        sleep 1
    done
    fail "missing prompt field $1"
}
engine_jid() {
    "$V" 'p=$(pgrep -of "^/usr/local/bin/atrium-crypta-ac$") || exit 1
        procstat -s "$p" | awk "NR == 2 {print \$10}" | grep -q C || exit 1
        ps -o jid= -p "$p" | tr -d " "'
}
answer_password() {
    username=$1
    password=$2
    handle=$("$V" "sh -s -- '$password'" <<'GUEST' | sed -n 's/^status=0 body=//p'
G=/tmp/crypta-gate
pub=$(printf 'a%.0s' $(seq 64))
su -m cg-ui -c "$G/arca-probe $G/arcad.sock hold alice $pub org.atrium.curia '$1' vpn"
GUEST
)
    [ "${#handle}" -eq 32 ] || fail "Arca returned no credential handle"
    "$V" "$CTL answer name=gp-gate t0=$(hex "$username") h1=$handle ${3:-}" >/dev/null || fail "answer password"
}

if [ -n "$portal_port" ]; then
    "$V" "$CTL create-gp name=gp-gate host=198.51.100.54:450 portal=1 ca=$trust scoped=0" >/dev/null
    "$V" "$CTL up name=gp-gate" >/dev/null
    wait_prompt gp_password_once
    portal_jid=$(engine_jid) || fail "portal engine jail or capability mode"
    [ "$portal_jid" -gt 0 ] || fail "portal engine is outside a jail"
    answer_password portal-alice portal-secret keep=1
    wait_prompt gp_gateway
    selected=198.51.100.52:449
    printf '%s\n' "$prompt" | grep -q " value=$(hex "$selected") " || fail "portal gateway choice is missing"
    if "$V" "$CTL answer name=gp-gate t0=$(hex "198.51.100.55:449")" >/dev/null; then
        fail "an unadvertised gateway was accepted"
    fi
    "$V" "$CTL answer name=gp-gate t0=$(hex "$selected")" >/dev/null
    wait_prompt gp_password_once
    gateway_jid=$(engine_jid) || fail "selected gateway jail or capability mode"
    [ "$gateway_jid" -gt 0 ] && [ "$gateway_jid" != "$portal_jid" ] || fail "the gateway reused the portal jail"
    if "$V" "jls -j '$portal_jid' jid" >/dev/null 2>&1; then
        fail "the portal jail still exists"
    fi
    [ "$(field starts)" = 2 ] || fail "expected one portal engine and one gateway engine"
    ok "cryptad retired the portal jail and jaild launched a fresh gateway jail"
    answer_password alice 'a&+ b' keep=1
else
    "$V" "$CTL create-gp name=gp-gate host=198.51.100.52:449 pin=$trust scoped=0" >/dev/null
    "$V" "$CTL up name=gp-gate" >/dev/null
    wait_prompt password
    answer_password alice 'a&+ b'
fi
if [ -n "$hip" ]; then
    wait_prompt gp_hip_approval
    [ "$(field handshake)" = 0 ] || fail "the data tunnel opened before HIP approval"
    preview=$(printf '%s\n' "$prompt" | sed -n 's/^text k=banner v=//p' | tr -d '\n')
    forwarding=$("$V" 'sysctl -n net.inet6.ip6.forwarding')
    if [ "$forwarding" = 1 ]; then forwarded=yes; else forwarded=no; fi
    for line in 'VPN user  alice' 'VPN hostname  atrium' 'Assigned IPv4  10.77.0.2' \
        "Operating system  $("$V" 'uname -sr')" "IPv6 forwarding  $forwarded" 'Firewall  Atrium pf enabled: yes'; do
        case "$preview" in *"$(hex "$line")"*) ;; *) fail "HIP prompt omitted a report field" ;; esac
    done
    ok "Settings prompt exposes the HIP facts before the data tunnel is up"
    if [ "$hip" = decline ]; then
        "$V" "$CTL answer name=gp-gate cancel=1" >/dev/null
        wait_field up 0
        [ -n "$(field error)" ] || fail "declining HIP did not end sign-in"
        ok "declining HIP ended sign-in"
        echo "$pass checks passed"
        exit 0
    fi
    "$V" "$CTL answer name=gp-gate t0=$(hex submit)" >/dev/null
fi
wait_field handshake 1
ok "GlobalProtect signed in using Arca's held password"
iface=$(field iface)
"$V" "route -n get 10.77.0.1 | grep -Eq 'interface: $iface$'" || fail "atrium-net did not install the tunnel route"
jid=$(engine_jid) || fail "engine jail or capability mode"
[ "$jid" -gt 0 ] || fail "engine is outside a jail"
if [ -n "$portal_port" ]; then
    items=$("$V" "su -m _crypta-key -c '$G/arca-probe $G/arcad.sock list'")
    printf '%s\n' "$items" | grep -q '^status=0 ' || fail "could not inspect Crypta's Arca items"
    for account in gp-gate/password gp-gate/session; do
        case "$items" in *"$(hex "$account")"*) fail "portal profile persisted credentials" ;; esac
    done
    ok "portal and gateway passwords and session stayed out of Arca storage"
fi
ok "jaild launched the corporate engine and atrium-net installed the route"
probe() {
    "$V" 'ping -n -c 1 -t 5 -S 10.77.0.2 10.77.0.1' || fail "$1"
    ok "$1"
}
if [ "$hip" = refresh ]; then
    original=$("$V" 'sysctl -n net.inet6.ip6.forwarding')
    case "$original" in
        0) changed=1; changed_word=yes ;;
        1) changed=0; changed_word=no ;;
        *) fail "IPv6 forwarding sysctl did not report a boolean" ;;
    esac
    trap '"$V" "sysctl net.inet6.ip6.forwarding=$original" >/dev/null || true; cleanup; kill "$LANPID" 2>/dev/null || true' EXIT
    probe "IPv4 echo before HIP refresh"
    wait_receipts() {
        for _ in $(seq 1 25); do
            [ "$(wc -l < "$receipts" | tr -d ' ')" -lt "$1" ] || return 0
            sleep 1
        done
        fail "HIP report did not reach the fixture"
    }
    wait_receipts 2
    [ "$(sed -n '1p' "$receipts")" = "$(sed -n '2p' "$receipts")" ] || fail "the unchanged reports differed"
    [ "$(field signin)" != prompt ] || fail "the unchanged report prompted again"
    ok "independent timer refreshed the unchanged report without asking"
    "$V" "sysctl net.inet6.ip6.forwarding=$changed" >/dev/null
    wait_prompt gp_hip_approval
    printf '%s\n' "$prompt" | sed -n 's/^text k=banner v=//p' | tr -d '\n' | grep -q "$(hex "IPv6 forwarding  $changed_word")" ||
        fail "review did not show the verified change"
    if grep -q "^$changed_word " "$receipts"; then
        fail "changed posture reached the gateway before approval"
    fi
    ok "verified posture change required fresh approval before upload"
    "$V" "$CTL answer name=gp-gate t0=$(hex submit)" >/dev/null || fail "approve changed HIP"
    for _ in $(seq 1 25); do
        if grep -q "^$changed_word " "$receipts"; then break; fi
        sleep 1
    done
    grep -q "^$changed_word " "$receipts" || fail "changed HIP did not reach the gateway"
    probe "IPv4 echo after approved HIP refresh"
    [ "$(field starts)" = 1 ] || fail "HIP refresh restarted the engine"
    "$V" "$CTL down name=gp-gate" >/dev/null
    ok "HIP refreshed on the original tunnel"
    echo "$pass checks passed"
    exit 0
fi
probe "IPv4 echo before rebind"
"$V" "$CTL rebind name=gp-gate" >/dev/null
# Wait for the second Up event, since the previous handshake can still read
# as up until the engine handles the rebind request.
for _ in $(seq 1 30); do
    connected=$("$V" "grep -c 'gp-gate: connected' $G/cryptad.log" || true)
    [ "$connected" != 2 ] || break
    sleep 1
done
[ "$connected" = 2 ] || fail "the second handshake never completed"
wait_field handshake 1
probe "IPv4 echo after rebind"
if [ -n "$transport" ]; then
    count=$("$V" "grep -c 'GlobalProtect data transport: $transport$' $G/cryptad.log" || true)
    [ "$count" = 2 ] || fail "expected $transport for both data connections"
    ok "engine selected $transport on both connections"
fi
"$V" "$CTL down name=gp-gate" >/dev/null
ok "GlobalProtect profile disconnected"
echo "$pass checks passed"
