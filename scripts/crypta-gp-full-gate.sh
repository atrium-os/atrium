#!/bin/sh
# Called by crypta-gp-packet-check.py --full-vm. Requires a dedicated,
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
pin=$3
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
"$V" "$CTL create-gp name=gp-gate host=198.51.100.52:449 pin=$pin scoped=0" >/dev/null
"$V" "$CTL up name=gp-gate" >/dev/null
wait_field signin prompt
prompt=$("$V" "$CTL prompt name=gp-gate")
printf '%s\n' "$prompt" | grep -q '^field i=0 kind=0 name=757365726e616d65' || fail "missing username prompt"
printf '%s\n' "$prompt" | grep -q '^field i=1 kind=1 name=70617373776f7264' || fail "missing Arca password prompt"
handle=$("$V" 'sh -s' <<'GUEST' | sed -n 's/^status=0 body=//p'
G=/tmp/crypta-gate
pub=$(printf 'a%.0s' $(seq 64))
su -m cg-ui -c "$G/arca-probe $G/arcad.sock hold alice $pub org.atrium.curia 'a&+ b' vpn"
GUEST
)
[ "${#handle}" -eq 32 ] || fail "Arca returned no credential handle"
"$V" "$CTL answer name=gp-gate t0=616c696365 h1=$handle" >/dev/null
wait_field handshake 1
ok "GlobalProtect signed in using Arca's held password"
iface=$(field iface)
"$V" "route -n get 10.77.0.1 | grep -Eq 'interface: $iface$'" || fail "atrium-net did not install the tunnel route"
"$V" 'p=$(pgrep -of "^/usr/local/bin/atrium-crypta-ac$"); [ -n "$p" ] &&
    [ "$(ps -o jid= -p "$p" | tr -d " ")" -gt 0 ] &&
    procstat -s "$p" | awk "NR == 2 {print \$10}" | grep -q C' || fail "engine is not jailed and in capability mode"
ok "jaild launched the corporate engine and atrium-net installed the route"
probe() {
    "$V" 'ping -n -c 1 -t 5 -S 10.77.0.2 10.77.0.1' || fail "$1"
    ok "$1"
}
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
"$V" "$CTL down name=gp-gate" >/dev/null
ok "GlobalProtect profile disconnected"
echo "$pass checks passed"
