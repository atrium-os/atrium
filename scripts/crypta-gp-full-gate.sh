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
decline_relock=0
decline_final_lock=0
case "$hip" in
    tessera-relock-decline) hip=tessera-reconnect; decline_relock=1 ;;
    tessera-relock-reconnect) hip=tessera-reconnect; decline_relock=2 ;;
    tessera-cycle) hip=tessera-reconnect; decline_relock=2; decline_final_lock=2 ;;
    tessera-final-lock-decline) hip=tessera-reconnect; decline_relock=2; decline_final_lock=1 ;;
esac
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
if [ "$hip" = posture ]; then
    (cd "$ROOT/portcullis" && cargo build -q --release --target "$T" -p opifex --bin opifexd)
    EXTRA="$EXTRA $ROOT/portcullis/target/$T/release/opifexd"
fi
case "$hip" in
    tessera) tessera_fact='Mounted sealed Tessera volumes' ;;
    tessera-domain|tessera-rebind|tessera-rebind-decline|tessera-reconnect) tessera_fact='Tessera encrypted domains verified with active keys' ;;
    *) tessera_fact= ;;
esac
if [ -n "$tessera_fact" ]; then
    module=${CRYPTA_GATE_TESSERA_KMOD:?path to a host-built tessera_fs.ko}
    [ -f "$module" ] && [ "${module##*/}" = tessera_fs.ko ] ||
        fail "expected a host-built tessera_fs.ko"
    EXTRA="$EXTRA $module"
fi
setup

if [ -n "$tessera_fact" ]; then
    tessera_cleanup() {
        "$V" 'G=/tmp/crypta-gate
            umount "$G/tessera" 2>/dev/null || true
            if [ -f "$G/tessera-md" ]; then
                md=$(cat "$G/tessera-md")
                mdconfig -d -u "${md#md}" 2>/dev/null || true
            fi
            if [ -f "$G/tessera-kld" ]; then
                /root/sealed/tessera-keyload drop c0ffee00112233445566778899aabbcc >/dev/null 2>&1 || true
                kldunload -i "$(cat "$G/tessera-kld")" || true
            fi
            rm -f "$G/tessera-key" "$G/tessera.img"' || true
        cleanup
        kill "$LANPID" 2>/dev/null || true
    }
    trap tessera_cleanup EXIT
    "$V" "sh -s -- $hip" <<'GUEST'
set -eu
G=/tmp/crypta-gate
tools=/root/sealed
kid=c0ffee00112233445566778899aabbcc
test -x "$tools/tessera-mkfs-sealed"
test -x "$tools/tessera-keyload"
if kldstat -q -m tessera; then
    echo "The Tessera posture gate needs a guest without Tessera already mounted or loaded" >&2
    exit 1
fi
kldload "$G/tessera_fs.ko"
kldstat -n tessera_fs.ko | awk 'NR == 2 {print $1}' > "$G/tessera-kld"
test "$(sysctl -n kern.tessera.sealed_mounts)" = 0
mkdir "$G/tessera"
umask 077
head -c 32 /dev/random > "$G/tessera-key"
if [ "$1" != tessera ]; then
    test -x "$tools/tessera-quota-set"
    uuid=a1b2c3d4e5f60718293a4b5c6d7e8f90
    "$tools/tessera-mkfs-sealed" --plain --create -s 64 -j 64 --uuid "$uuid" "$G/tessera.img" >/dev/null
    mdconfig -a -t vnode -f "$G/tessera.img" > "$G/tessera-md"
    mount -t tessera "/dev/$(cat "$G/tessera-md")" "$G/tessera"
    mkdir "$G/tessera/persona"
    "$tools/tessera-quota-set" "$G/tessera/persona" 1000000000 >/dev/null
    "$tools/tessera-keyload" load-group "$G/tessera-key" "$kid" "$uuid" 9 >/dev/null
    "$tools/tessera-keyload" group-create "$G/tessera" "$kid" 9 >/dev/null
    # Keep this disposable, mode-0600 key until the live unlock check.
    printf '%s\n' "$uuid" > "$G/tessera-uuid"
    test "$(sysctl -n kern.tessera.verified_encrypted_domains)" = 0
    exit 0
fi
printf '%s\n' 'sealed HIP fixture' > "$G/tessera-content"
"$tools/tessera-mkfs-sealed" --key-file "$G/tessera-key" --key-id "$kid" --create -s 64 -j 64 \
    --seed-file probe --seed-content "$G/tessera-content" "$G/tessera.img" > "$G/tessera-format"
uuid=$(awk '/^uuid/ {print $2}' "$G/tessera-format")
test -n "$uuid"
"$tools/tessera-keyload" load "$G/tessera-key" "$kid" "$uuid" >/dev/null
rm -f "$G/tessera-key"
mdconfig -a -t vnode -f "$G/tessera.img" > "$G/tessera-md"
GUEST
fi

posture_cleanup() {
    "$V" 'G=/tmp/crypta-gate
        if [ -f "$G/opifex.pid" ]; then kill "$(cat "$G/opifex.pid")" 2>/dev/null || true; fi
        if [ -f "$G/hip-md" ]; then
            md=$(cat "$G/hip-md")
            umount "$G/hip-encrypted" 2>/dev/null || true
            geli detach "$md.eli" 2>/dev/null || true
            mdconfig -d -u "${md#md}" 2>/dev/null || true
        fi' || true
    cleanup
    kill "$LANPID" 2>/dev/null || true
}
if [ "$hip" = posture ]; then
    trap posture_cleanup EXIT
    "$V" 'sh -s' <<'GUEST'
set -eu
G=/tmp/crypta-gate
mkdir "$G/hip-encrypted" "$G/hip-seat" "$G/opifex-state" "$G/opifex-root"
md=$(mdconfig -a -t malloc -s 32m)
printf '%s\n' "$md" > "$G/hip-md"
geli onetime -e AES-XTS -l 256 "$md"
newfs -U "/dev/$md.eli" >/dev/null
mount "/dev/$md.eli" "$G/hip-encrypted"
printf 'alice\n' > "$G/hip-seat/active-session"
printf '\n' > "$G/hip-seat/session-locked"
# An isolated Opifex database seeded with a known prior successful check.
printf 'last_sync = 1760000000\n' > "$G/opifex-state/updates.toml"
env OPIFEX_SOCKET="$G/opifex.sock" OPIFEX_STATE="$G/opifex-state" OPIFEX_ROOT="$G/opifex-root" \
    OPIFEX_DAILY=0 PRAECO_SOCKET=none daemon -f -p "$G/opifex.pid" -o "$G/opifex.log" "$G/opifexd"
pkill -f '^/usr/local/bin/atrium-cryptad -v --sock /tmp/crypta-gate/crypta.sock'
sleep 1
env CRYPTA_HIP_OPIFEX_SOCKET="$G/opifex.sock" CRYPTA_HIP_SEAT="$G/hip-seat/active-session" \
    daemon -f -o "$G/cryptad.log" /usr/local/bin/atrium-cryptad -v --sock "$G/crypta.sock" \
        --state "$G/state" --net-sock "$G/net.sock" --arca "$G/arcad.sock"
GUEST
fi

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
hip_choice() {
    printf '%s\n' "$prompt" | sed -n 's/^choice i=0 value=\([0-9a-f]*\) .*/\1/p'
}
approve_hip() {
    approval_choice=$(hip_choice)
    [ -n "$approval_choice" ] || fail "HIP approval choice missing"
    "$V" "$CTL answer name=gp-gate t0=$approval_choice" >/dev/null
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
    if [ "$hip" = posture ]; then
        for line in 'Mounted GELI encrypted providers  1' 'Opifex last check (Unix seconds)  1760000000' \
            'Opifex catalog updates listed  0' 'Active session locked  no'; do
            case "$preview" in *"$(hex "$line")"*) ;; *) fail "HIP review is missing $line" ;; esac
        done
        ok "review shows live GELI, Opifex and active-session lock facts"
    fi
    if [ -n "$tessera_fact" ]; then
        case "$preview" in *"$(hex "$tessera_fact  0")"*) ;;
            *) fail "initial HIP review omitted the verified zero Tessera count" ;;
        esac
        ok "review shows zero for $tessera_fact"
    fi
    ok "Settings prompt exposes the HIP facts before the data tunnel is up"
    if [ "$hip" = decline ]; then
        "$V" "$CTL answer name=gp-gate cancel=1" >/dev/null
        wait_field up 0
        [ -n "$(field error)" ] || fail "declining HIP did not end sign-in"
        ok "declining HIP ended sign-in"
        echo "$pass checks passed"
        exit 0
    fi
    approve_hip
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
if [ "$hip" = posture ]; then
    probe "IPv4 echo before screen-lock change"
    for _ in $(seq 1 20); do
        grep -q '^no ' "$receipts" && break
        sleep 1
    done
    grep -q '^no ' "$receipts" || fail "initial posture report did not reach peer"
    "$V" "printf '%s\n' alice > '$G/hip-seat/session-locked'"
    wait_prompt gp_hip_approval
    printf '%s\n' "$prompt" | sed -n 's/^text k=banner v=//p' | tr -d '\n' | grep -q "$(hex 'Active session locked  yes')" ||
        fail "review did not reflect the real seat lock marker"
    if grep -q '^yes ' "$receipts"; then fail "screen-lock change was uploaded without approval"; fi
    approve_hip
    for _ in $(seq 1 20); do
        grep -q '^yes ' "$receipts" && break
        sleep 1
    done
    grep -q '^yes ' "$receipts" || fail "approved screen-lock change did not reach peer"
    ok "changed screen lock required fresh approval before upload"
    probe "IPv4 echo after approved posture refresh"
    "$V" "$CTL down name=gp-gate" >/dev/null
    echo "$pass checks passed"
    exit 0
fi
if [ -n "$tessera_fact" ]; then
    probe "IPv4 echo before the Tessera encryption change"
    grep -q '^0 ' "$receipts" || fail "initial Tessera report did not reach peer"
    checkpoint_tessera_receipts() {
        receipt_offset=$(awk 'END { print NR }' "$receipts")
    }
    reject_stale_hip_choice() {
        [ -n "$1" ] || fail "missing previous HIP choice"
        [ "$(hip_choice)" != "$1" ] || fail "the replacement review reused the previous HIP choice"
        if stale_reply=$("$V" "$CTL answer name=gp-gate t0=$1" 2>&1); then
            fail "the replacement review accepted the stale choice"
        fi
        [ "$stale_reply" = 'error why=bad-choice' ] ||
            fail "unexpected reply to stale HIP choice: $stale_reply"
    }
    reconnect_tessera() {
        "$V" "$CTL up name=gp-gate" >/dev/null
        wait_tessera_review
        reject_stale_hip_choice "$declined_choice"
        sleep 1
        [ "$(field handshake)" = 0 ] || fail "reconnect opened before HIP approval"
        reconnect_iface=$(field iface)
        [ -n "$reconnect_iface" ] || fail "reconnect has no tunnel interface"
        "$V" "if route -n get 10.77.0.1 2>/dev/null | grep -Eq 'interface: $reconnect_iface$'; then exit 1; fi" ||
            fail "reconnect installed its tunnel route before approval"
        "$V" 'if ping -n -c 1 -t 2 -S 10.77.0.2 10.77.0.1 >/dev/null 2>&1; then exit 1; fi' ||
            fail "IPv4 echo passed before reconnect approval"
        [ "$("$V" "grep -c 'gp-gate: connected$' $G/cryptad.log")" = "$1" ] ||
            fail "reconnect came up before fresh approval"
        if new_tessera_receipt; then fail "$change was uploaded before reconnect approval"; fi
        ok "explicit reconnect waited for a new HIP approval"
        touch "$receipts.$2"
    }
    approve_tessera_change() {
        expected_count=$1
        change=$2
        wait_tessera_review
        if [ "${3:-}" = rebind ]; then
            old_choice=$(hip_choice)
            [ -n "$old_choice" ] || fail "missing original HIP choice"
            "$V" "$CTL rebind name=gp-gate" >/dev/null
            wait_field handshake 0
            wait_tessera_review
            reject_stale_hip_choice "$old_choice"
            ok "rebind replaced the pending HIP review and rejected the stale choice"
        fi
        sleep 1
        if new_tessera_receipt; then fail "$change was uploaded without approval"; fi
        if [ "${3:-}" = rebind ] && [ "$hip" != tessera-rebind ]; then
            declined_choice=$(hip_choice)
            "$V" "$CTL answer name=gp-gate cancel=1" >/dev/null
            wait_field up 0
            [ "$(field error)" = "$(hex 'GlobalProtect device report was not approved')" ] ||
                fail "declining the rebound HIP review did not stop the profile"
            ok "declining the fresh HIP review stopped the rebound profile"
            # Cover another HIP timer interval and check for a transient Up.
            sleep 6
            [ "$(field up)" = 0 ] && [ -z "$(field engine)" ] ||
                fail "the rebound profile restarted after decline"
            [ "$("$V" "grep -c 'gp-gate: connected$' $G/cryptad.log")" = 1 ] ||
                fail "the rebound tunnel connected after decline"
            "$V" "if route -n get 10.77.0.1 2>/dev/null | grep -Eq 'interface: $iface$'; then exit 1; fi" ||
                fail "the declined profile retained its tunnel route"
            if new_tessera_receipt; then fail "$change was uploaded after decline"; fi
            ok "rebound tunnel stayed down and no changed HIP report arrived"
            if [ "$hip" = tessera-rebind-decline ]; then
                echo "$pass checks passed"
                exit 0
            fi
            reconnect_tessera 1 approved
        fi
        approve_hip
        for _ in $(seq 1 20); do
            new_tessera_receipt && break
            sleep 1
        done
        new_tessera_receipt || fail "approved report did not reach peer after $change"
        ok "$change required fresh approval before upload"
    }
    wait_tessera_review() {
        wait_prompt gp_hip_approval
        printf '%s\n' "$prompt" | sed -n 's/^text k=banner v=//p' | tr -d '\n' |
            grep -q "$(hex "$tessera_fact  $expected_count")" ||
            fail "HIP review omitted the count after $change"
    }
    new_tessera_receipt() {
        tail -n "+$((receipt_offset + 1))" "$receipts" | grep -q "^$expected_count "
    }
    check_unchanged_locked_report() {
        first_locked=$(awk -v offset="$receipt_offset" 'NR > offset && $1 == 0 { print NR; exit }' "$receipts")
        [ -n "$first_locked" ] || fail "expected an approved locked-key report"
        second_locked=$((first_locked + 1))
        for _ in $(seq 1 20); do
            [ "$(field signin)" = in ] || fail "unchanged reconnect HIP report prompted or disconnected"
            [ "$(wc -l < "$receipts" | tr -d ' ')" -lt "$second_locked" ] || break
            sleep 1
        done
        [ "$(wc -l < "$receipts" | tr -d ' ')" -ge "$second_locked" ] ||
            fail "unchanged reconnect HIP report did not reach the fixture"
        locked_report=$(sed -n "${first_locked}p" "$receipts")
        [ "$(sed -n "${second_locked}p" "$receipts")" = "$locked_report" ] ||
            fail "the unchanged reconnect reports differed"
        ok "unchanged HIP report refreshed after reconnect without prompting"
        probe "IPv4 echo after unchanged reconnect refresh"
    }
    unlock_tessera_key() {
        "$V" 'G=/tmp/crypta-gate; set -e
            /root/sealed/tessera-keyload load-group "$G/tessera-key" c0ffee00112233445566778899aabbcc \
                "$(cat "$G/tessera-uuid")" 9
            /root/sealed/tessera-keyload group-info "$G/tessera/persona" | grep -q " open$"
            test "$(sysctl -n kern.tessera.verified_encrypted_domains)" = 1'
    }
    rebind_unlocked_tessera() {
        unlocked_report=$(tail -n 1 "$receipts")
        [ "${unlocked_report%% *}" = 1 ] || fail "missing approved unlocked-key report"
        before_rebind=$(wc -l < "$receipts" | tr -d ' ')
        reviews=$("$V" "grep -c 'gp-gate: the gateway asks' $G/cryptad.log")
        "$V" "$CTL rebind name=gp-gate" >/dev/null
        # The old connection can still show handshake=1 until rebind is processed.
        for _ in $(seq 1 30); do
            [ "$(field signin)" != prompt ] || fail "unchanged unlocked report prompted on rebind"
            connected=$("$V" "grep -c 'gp-gate: connected$' $G/cryptad.log")
            [ "$connected" != "$1" ] || break
            sleep 1
        done
        [ "$connected" = "$1" ] || fail "post-unlock rebound tunnel did not connect"
        wait_field handshake 1
        [ "$("$V" "grep -c 'gp-gate: the gateway asks' $G/cryptad.log")" = "$reviews" ] ||
            fail "post-unlock rebind issued another review"
        [ "$(sed -n "$((before_rebind + 1))p" "$receipts")" = "$unlocked_report" ] ||
            fail "the post-unlock rebind reported different facts"
        ok "post-unlock rebind sent the unchanged HIP report without prompting"
    }
    decline_active_tessera_review() {
        wait_tessera_review
        declined_choice=$(hip_choice)
        [ "$(field handshake)" = 1 ] || fail "the rebound tunnel was down before decline"
        "$V" "$CTL answer name=gp-gate cancel=1" >/dev/null
        wait_field up 0
        [ "$(field error)" = "$(hex 'GlobalProtect device report was not approved')" ] ||
            fail "declining $change did not stop the profile"
        # Allow another refresh interval to catch retries or delayed uploads.
        sleep 6
        [ "$(field up)" = 0 ] && [ -z "$(field engine)" ] ||
            fail "the profile restarted after declining $change"
        [ "$("$V" "grep -c 'gp-gate: connected$' $G/cryptad.log")" = "$1" ] ||
            fail "another tunnel connected after declining $change"
        "$V" "if route -n get 10.77.0.1 2>/dev/null | grep -Eq 'interface: $2$'; then exit 1; fi" ||
            fail "the declined rebound tunnel retained its route"
        "$V" 'if ping -n -c 1 -t 2 -S 10.77.0.2 10.77.0.1 >/dev/null 2>&1; then exit 1; fi' ||
            fail "IPv4 echo passed after declining $change"
        if new_tessera_receipt; then fail "the declined count reached the gateway"; fi
        ok "declining $change shut down the rebound tunnel without uploading the changed count"
    }
    lock_tessera_key() {
        "$V" 'set -e
            /root/sealed/tessera-keyload drop c0ffee00112233445566778899aabbcc
            /root/sealed/tessera-keyload group-info /tmp/crypta-gate/tessera/persona | grep -q " locked$"
            test "$(sysctl -n kern.tessera.verified_encrypted_domains)" = 0'
    }
    checkpoint_tessera_receipts
    if [ "$hip" != tessera ]; then
        "$V" 'G=/tmp/crypta-gate; set -e
            /root/sealed/tessera-keyload quota-group "$G/tessera/persona" 9
            /root/sealed/tessera-keyload group-info "$G/tessera/persona" | grep -q " open$"
            test "$(sysctl -n kern.tessera.verified_encrypted_domains)" = 1
            test "$(sysctl -n kern.tessera.sealed_mounts)" = 0'
    else
        "$V" 'G=/tmp/crypta-gate; set -e
            mount -t tessera -r "/dev/$(cat "$G/tessera-md")" "$G/tessera"
            cmp "$G/tessera/probe" "$G/tessera-content"
            test "$(sysctl -n kern.tessera.sealed_mounts)" = 1'
    fi
    approve_tessera_change 1 "Tessera encryption change"
    probe "IPv4 echo after approved Tessera encryption change"
    if [ "$hip" != tessera ]; then
        checkpoint_tessera_receipts
        lock_tessera_key
        if [ "$hip" != tessera-domain ]; then
            approve_tessera_change 0 "Tessera domain key lock" rebind
            wait_field handshake 1
        else
            approve_tessera_change 0 "Tessera domain key lock"
        fi
        probe "IPv4 echo after approved Tessera domain key lock"
        if [ "$hip" = tessera-reconnect ]; then
            "$V" "route -n get 10.77.0.1 | grep -Eq 'interface: $reconnect_iface$'" ||
                fail "approved reconnect did not install its tunnel route"
            check_unchanged_locked_report
        fi

        checkpoint_tessera_receipts
        unlock_tessera_key
        # The second-decline reconnect needs the disposable key for one more unlock.
        if [ "$decline_relock" != 2 ]; then
            "$V" "rm -f '$G/tessera-key'"
        fi
        approve_tessera_change 1 "Tessera domain key unlock"
        probe "IPv4 echo after approved Tessera domain key unlock"
        if [ "$hip" = tessera-reconnect ]; then
            rebind_unlocked_tessera 3
            probe "IPv4 echo on the post-unlock rebound tunnel"

            checkpoint_tessera_receipts
            lock_tessera_key
            if [ "$decline_relock" != 0 ]; then
                expected_count=0
                change="Tessera domain key lock after rebind"
                decline_active_tessera_review 3 "$reconnect_iface"
                if [ "$decline_relock" = 1 ]; then
                    echo "$pass checks passed"
                    exit 0
                fi
                reconnect_tessera 3 relock-approved
            fi
            approve_tessera_change 0 "Tessera domain key lock after rebind"
            if [ "$decline_relock" = 2 ]; then
                wait_field handshake 1
                probe "IPv4 echo after approved second-decline reconnect"
                check_unchanged_locked_report
                checkpoint_tessera_receipts
                unlock_tessera_key
                if [ "$decline_final_lock" != 2 ]; then
                    "$V" "rm -f '$G/tessera-key'"
                fi
                approve_tessera_change 1 "Tessera domain key unlock after second-decline reconnect"
                probe "IPv4 echo after approved unlock on the second reconnect"
                rebind_unlocked_tessera 5
                probe "IPv4 echo after the second reconnect rebind"

                checkpoint_tessera_receipts
                lock_tessera_key
                if [ "$decline_final_lock" != 0 ]; then
                    expected_count=0
                    change="Tessera domain key lock after the second reconnect rebind"
                    decline_active_tessera_review 5 "$reconnect_iface"
                    if [ "$decline_final_lock" = 1 ]; then
                        echo "$pass checks passed"
                        exit 0
                    fi
                    reconnect_tessera 5 final-approved
                    approve_tessera_change 0 "Tessera domain key lock after final decline"
                    wait_field handshake 1
                    probe "IPv4 echo after reconnect from final decline"
                    check_unchanged_locked_report
                    checkpoint_tessera_receipts
                    unlock_tessera_key
                    "$V" "rm -f '$G/tessera-key'"
                    approve_tessera_change 1 "Tessera domain key unlock after final decline"
                    probe "IPv4 echo after approved unlock from final decline"
                    rebind_unlocked_tessera 7
                    probe "IPv4 echo on the final rebound tunnel"
                    "$V" "$CTL down name=gp-gate" >/dev/null
                    echo "$pass checks passed"
                    exit 0
                fi
                approve_tessera_change 0 "Tessera domain key lock after the second reconnect rebind"
                probe "IPv4 echo on the second rebound tunnel after approved key lock"
            else
                probe "IPv4 echo on the rebound tunnel after approved key lock"
            fi
        fi
    fi
    "$V" "$CTL down name=gp-gate" >/dev/null
    echo "$pass checks passed"
    exit 0
fi
decline_after_esp_loss=0
retry_hip_submit=0
case "$hip" in
    esp-loss-decline) hip=esp-loss; decline_after_esp_loss=1 ;;
    esp-loss-reconnect) hip=esp-loss; decline_after_esp_loss=2 ;;
    esp-loss-hip-active-refresh-rebind-recover) hip=esp-loss; decline_after_esp_loss=2; retry_hip_submit=19 ;;
    esp-loss-hip-active-refresh-rebind-tls) hip=esp-loss; decline_after_esp_loss=2; retry_hip_submit=18 ;;
    esp-loss-hip-active-refresh-reconnect) hip=esp-loss; decline_after_esp_loss=2; retry_hip_submit=17 ;;
    esp-loss-hip-active-refresh-decline) hip=esp-loss; decline_after_esp_loss=2; retry_hip_submit=16 ;;
    esp-loss-hip-active-refresh-recheck) hip=esp-loss; decline_after_esp_loss=2; retry_hip_submit=15 ;;
    esp-loss-hip-active-refresh-retry) hip=esp-loss; decline_after_esp_loss=2; retry_hip_submit=14 ;;
    esp-loss-hip-submit-recovered-reconnect) hip=esp-loss; decline_after_esp_loss=2; retry_hip_submit=13 ;;
    esp-loss-hip-submit-recovered-decline) hip=esp-loss; decline_after_esp_loss=2; retry_hip_submit=12 ;;
    esp-loss-hip-submit-final-recover) hip=esp-loss; decline_after_esp_loss=2; retry_hip_submit=11 ;;
    esp-loss-hip-submit-final-fallback) hip=esp-loss; decline_after_esp_loss=2; retry_hip_submit=10 ;;
    esp-loss-hip-submit-final-rebind) hip=esp-loss; decline_after_esp_loss=2; retry_hip_submit=9 ;;
    esp-loss-hip-submit-rebind-reconnect) hip=esp-loss; decline_after_esp_loss=2; retry_hip_submit=8 ;;
    esp-loss-hip-submit-rebind-decline) hip=esp-loss; decline_after_esp_loss=2; retry_hip_submit=7 ;;
    esp-loss-hip-submit-rebind-recheck) hip=esp-loss; decline_after_esp_loss=2; retry_hip_submit=6 ;;
    esp-loss-hip-submit-rebind-retry) hip=esp-loss; decline_after_esp_loss=2; retry_hip_submit=5 ;;
    esp-loss-hip-submit-retry-reconnect) hip=esp-loss; decline_after_esp_loss=2; retry_hip_submit=4 ;;
    esp-loss-hip-submit-retry-decline) hip=esp-loss; decline_after_esp_loss=2; retry_hip_submit=3 ;;
    esp-loss-hip-submit-retry-recheck) hip=esp-loss; decline_after_esp_loss=2; retry_hip_submit=2 ;;
    esp-loss-hip-submit-retry) hip=esp-loss; decline_after_esp_loss=2; retry_hip_submit=1 ;;
    esp-loss-rebind-decline) hip=esp-loss; decline_after_esp_loss=3 ;;
    esp-loss-rebind-reconnect) hip=esp-loss; decline_after_esp_loss=4 ;;
esac
decline_esp_loss_review() {
    [ "$(field handshake)" = 1 ] || fail "the tunnel was down before decline"
    "$V" "$CTL answer name=gp-gate cancel=1" >/dev/null
    wait_field up 0
    [ "$(field error)" = "$(hex 'GlobalProtect device report was not approved')" ] ||
        fail "declining HIP did not stop the profile"
    sleep 6
    [ "$(field up)" = 0 ] && [ -z "$(field engine)" ] ||
        fail "the profile restarted after decline"
    [ "$("$V" "grep -o 'gp-gate: connected' $G/cryptad.log | wc -l | tr -d ' '")" = "$1" ] ||
        fail "the connection count changed after decline"
    "$V" "if route -n get 10.77.0.1 2>/dev/null | grep -Eq 'interface: $2$'; then exit 1; fi" ||
        fail "the declined tunnel retained its route"
    "$V" 'if ping -n -c 1 -t 2 -S 10.77.0.2 10.77.0.1 >/dev/null 2>&1; then exit 1; fi' ||
        fail "IPv4 echo passed after decline"
}
reconnect_after_esp_loss_decline() {
    "$V" "$CTL up name=gp-gate" >/dev/null
    wait_prompt gp_hip_approval
    printf '%s\n' "$prompt" | sed -n 's/^text k=banner v=//p' | tr -d '\n' |
        grep -q "$(hex "IPv6 forwarding  ${3:-$changed_word}")" || fail "reconnect reviewed different posture"
    [ "$(hip_choice)" != "$declined_choice" ] || fail "reconnect reused the declined approval"
    if stale_reply=$("$V" "$CTL answer name=gp-gate t0=$declined_choice" 2>&1); then
        fail "reconnect accepted the declined approval"
    fi
    [ "$stale_reply" = 'error why=bad-choice' ] || fail "unexpected stale approval reply: $stale_reply"
    sleep 1
    [ "$(field handshake)" = 0 ] || fail "the reconnect opened before approval"
    "$V" 'if ping -n -c 1 -t 2 -S 10.77.0.2 10.77.0.1 >/dev/null 2>&1; then exit 1; fi' ||
        fail "IPv4 echo passed before reconnect approval"
    if tail -n "+$(($1 + 1))" "$receipts" | grep -q "^${3:-$changed_word} "; then
        fail "changed posture reached the gateway before reconnect approval"
    fi
    ok "reconnect rejected the declined approval and withheld changed HIP and traffic"
    touch "$receipts.$2"
}
restore_forwarding_and_rebind() {
    before_restore=$(awk 'END { print NR }' "$receipts")
    "$V" "sysctl net.inet6.ip6.forwarding=$original" >/dev/null
    wait_prompt gp_hip_approval
    original_word=${initial_report%% *}
    printf '%s\n' "$prompt" | sed -n 's/^text k=banner v=//p' | tr -d '\n' |
        grep -q "$(hex "IPv6 forwarding  $original_word")" || fail "review omitted restored IPv6 forwarding"
    if tail -n "+$((before_restore + 1))" "$receipts" | grep -q "^$original_word "; then
        fail "restored posture reached the gateway before fresh approval"
    fi
    approve_hip
    for _ in $(seq 1 20); do
        if tail -n "+$((before_restore + 1))" "$receipts" | grep -Fxq "$initial_report"; then break; fi
        sleep 1
    done
    tail -n "+$((before_restore + 1))" "$receipts" | grep -Fxq "$initial_report" ||
        fail "the approved restored report did not reach the gateway"
    ok "restoring IPv6 forwarding required fresh approval"
    probe "IPv4 echo on reconnected ESP after restored posture approval"
    rebind_restored_forwarding "$1"
}
decline_failed_hip_retry() {
    "$V" "$CTL answer name=gp-gate cancel=1" >/dev/null
    wait_field up 0
    [ "$(field error)" = "$(hex 'GlobalProtect device report was not approved')" ] ||
        fail "declining retry HIP did not report the refusal"
    sleep 6
    [ "$(field up)" = 0 ] && [ -z "$(field engine)" ] ||
        fail "the profile restarted after declining retry HIP"
    [ "$(wc -l < "$receipts" | tr -d ' ')" = "$1" ] ||
        fail "declining retry HIP submitted the pending report"
    "$V" 'if ping -n -c 1 -t 2 -S 10.77.0.2 10.77.0.1 >/dev/null 2>&1; then exit 1; fi' ||
        fail "IPv4 echo passed after declining retry HIP"
}
rebind_restored_forwarding() {
    reviews=$("$V" "grep -o 'gp-gate: the gateway asks' $G/cryptad.log | wc -l | tr -d ' '")
    before_rebind=$(awk 'END { print NR }' "$receipts")
    "$V" "$CTL rebind name=gp-gate" >/dev/null
    if [ "$#" = 1 ] && [ "$retry_hip_submit" -ge 5 ]; then
        for _ in $(seq 1 25); do
            failures=$("$V" "grep -o 'gp-gate: not connected (GlobalProtect HIP submission failed:' $G/cryptad.log | wc -l | tr -d ' '")
            [ "$failures" != 2 ] || break
            sleep 1
        done
        [ "$failures" = 2 ] && [ "$(field handshake)" = 0 ] ||
            fail "the dropped rebind HIP response did not keep ESP down"
        "$V" 'if ping -n -c 1 -t 2 -S 10.77.0.2 10.77.0.1 >/dev/null 2>&1; then exit 1; fi' ||
            fail "IPv4 echo passed before the rebound HIP retry was acknowledged"
        ok "rebound ESP stayed down after the dropped HIP response"
        if [ "$retry_hip_submit" -ge 6 ]; then
            stale_choice=$approval_choice
            before_approval=$(awk 'END { print NR }' "$receipts")
            "$V" "sysctl net.inet6.ip6.forwarding=$changed" >/dev/null
            touch "$receipts.rebind-posture"
            wait_prompt gp_hip_approval
            printf '%s\n' "$prompt" | sed -n 's/^text k=banner v=//p' | tr -d '\n' |
                grep -q "$(hex "IPv6 forwarding  $changed_word")" || fail "rebind retry reviewed stale posture"
            if stale_reply=$("$V" "$CTL answer name=gp-gate t0=$stale_choice" 2>&1); then
                fail "rebind retry accepted the old HIP choice"
            fi
            [ "$stale_reply" = 'error why=bad-choice' ] || fail "unexpected stale approval reply: $stale_reply"
            [ "$(field handshake)" = 0 ] &&
                [ "$(awk 'END { print NR }' "$receipts")" = "$before_approval" ] ||
                fail "rebind retry submitted HIP or opened before fresh approval"
            if [ "$retry_hip_submit" -ge 7 ]; then
                declined_choice=$(hip_choice)
                decline_failed_hip_retry "$before_approval"
                ok "declining the rebind retry kept the changed report unsent and ESP down"
                if [ "$retry_hip_submit" = 7 ]; then
                    echo "$pass checks passed"
                    exit 0
                fi
                reconnect_after_esp_loss_decline "$before_approval" rebind-retry
                reviews=$((reviews + 1))
            fi
            reviews=$((reviews + 1))
            touch "$receipts.rebind-retry"
            approve_hip
            ok "rebind retry reviewed changed posture and rejected stale approval"
        else
            touch "$receipts.rebind-retry"
        fi
    fi
    for _ in $(seq 1 30); do
        [ "$(field signin)" != prompt ] || fail "unchanged restored HIP prompted on rebind"
        connected=$("$V" "grep -o 'gp-gate: connected' $G/cryptad.log | wc -l | tr -d ' '")
        [ "$connected" != "$1" ] || break
        sleep 1
    done
    [ "$connected" = "$1" ] || fail "the restored ESP tunnel did not rebind"
    wait_field handshake 1
    [ "$("$V" "grep -o 'gp-gate: the gateway asks' $G/cryptad.log | wc -l | tr -d ' '")" = "$reviews" ] ||
        fail "rebind issued another HIP review"
    [ "$(field signin)" = in ] &&
        [ "$(sed -n "$((before_rebind + 1))p" "$receipts")" = "${2:-$initial_report}" ] ||
        fail "the rebound ESP tunnel did not submit the unchanged restored report"
    if [ "$#" = 1 ] && [ "$retry_hip_submit" -ge 6 ]; then
        rebound_report=$(sed -n "$((before_approval + 1))p" "$receipts")
        [ "${rebound_report%% *}" = "$changed_word" ] || fail "rebind retry omitted the approved change"
        probe "IPv4 echo after fresh approval on rebound ESP"
        before_refresh=$(awk 'END { print NR }' "$receipts")
        wait_receipts "$((before_refresh + 1))"
        [ "$(field signin)" = in ] &&
            [ "$(sed -n "$((before_refresh + 1))p" "$receipts")" = "$rebound_report" ] &&
            [ "$("$V" "grep -o 'gp-gate: the gateway asks' $G/cryptad.log | wc -l | tr -d ' '")" = "$reviews" ] ||
            fail "changed HIP did not refresh silently on rebound ESP"
        ok "approved HIP refreshed silently on rebound ESP"
        probe "IPv4 echo after silent refresh on rebound ESP"
    else
        ok "rebind reused approval for unchanged posture"
        probe "IPv4 echo after restored ESP rebind"
    fi
}
if [ "$hip" = esp-loss ]; then
    initial_report=$(head -n 1 "$receipts")
    reviews=$("$V" "grep -c 'gp-gate: the gateway asks' $G/cryptad.log")
    probe "IPv4 echo on active ESP before outage"
    for _ in $(seq 1 50); do
        [ "$(field signin)" != prompt ] || fail "ESP loss prompted for unchanged HIP"
        if "$V" "grep -q 'GlobalProtect data transport: TLS' $G/cryptad.log"; then break; fi
        sleep 1
    done
    "$V" "grep -q 'not connected (GlobalProtect ESP lost; reconnecting over TLS)' $G/cryptad.log &&
          grep -q 'GlobalProtect data transport: TLS' $G/cryptad.log" ||
        fail "the active ESP tunnel did not recover over TLS"
    wait_field handshake 1
    [ "$("$V" "grep -c 'gp-gate: the gateway asks' $G/cryptad.log")" = "$reviews" ] ||
        fail "ESP recovery issued another review"
    [ "$(tail -n 1 "$receipts")" = "$initial_report" ] || fail "ESP recovery reported different HIP facts"
    ok "active ESP loss recovered over TLS with unchanged HIP and no prompt"
    before_refresh=$(awk 'END { print NR }' "$receipts")
    for _ in $(seq 1 20); do
        [ "$(field signin)" = in ] || fail "TLS recovery refresh prompted or disconnected"
        [ "$(awk 'END { print NR }' "$receipts")" -le "$before_refresh" ] || break
        sleep 1
    done
    [ "$(sed -n "$((before_refresh + 1))p" "$receipts")" = "$initial_report" ] ||
        fail "the recovered TLS tunnel did not refresh unchanged HIP"
    ok "unchanged HIP report refreshed on recovered TLS without prompting"
fi
if [ "$hip" = refresh ] || [ "$hip" = esp-loss ]; then
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
    if [ "$decline_after_esp_loss" != 0 ]; then
        declined_choice=$(hip_choice)
        [ -n "$declined_choice" ] || fail "missing declined approval choice"
        decline_esp_loss_review 2 "$iface"
        if grep -q "^$changed_word " "$receipts"; then
            fail "changed posture reached the gateway after decline"
        fi
        ok "decline stopped recovered TLS and withheld changed HIP"
        if [ "$decline_after_esp_loss" = 1 ]; then
            echo "$pass checks passed"
            exit 0
        fi
        reconnect_after_esp_loss_decline 0 approved
    fi
    if [ "$retry_hip_submit" -ne 0 ]; then
        retry_reviews=$("$V" "grep -o 'gp-gate: the gateway asks' $G/cryptad.log | wc -l | tr -d ' '")
    fi
    approve_hip || fail "approve changed HIP"
    if [ "$retry_hip_submit" -ne 0 ]; then
        for _ in $(seq 1 25); do
            if "$V" "grep -q 'gp-gate: not connected (GlobalProtect HIP submission failed:' $G/cryptad.log"; then
                break
            fi
            sleep 1
        done
        "$V" "grep -q 'gp-gate: not connected (GlobalProtect HIP submission failed:' $G/cryptad.log" ||
            fail "the dropped HIP response did not produce a visible failure"
        [ "$(field handshake)" = 0 ] || fail "the tunnel opened after the dropped HIP response"
        "$V" 'if ping -n -c 1 -t 2 -S 10.77.0.2 10.77.0.1 >/dev/null 2>&1; then exit 1; fi' ||
            fail "IPv4 echo passed before the retry was acknowledged"
        ok "HIP submission failure was logged and kept the tunnel blocked"
        if [ "$retry_hip_submit" -ge 2 ]; then
            previous_choice=$approval_choice
            before_retry=$(wc -l < "$receipts" | tr -d ' ')
            "$V" "sysctl net.inet6.ip6.forwarding=$original" >/dev/null
            touch "$receipts.posture-restored"
            wait_prompt gp_hip_approval
            printf '%s\n' "$prompt" | sed -n 's/^text k=banner v=//p' | tr -d '\n' |
                grep -q "$(hex "IPv6 forwarding  ${initial_report%% *}")" || fail "retry reviewed stale posture"
            [ "$(hip_choice)" != "$previous_choice" ] || fail "retry reused the old choice"
            if stale_reply=$("$V" "$CTL answer name=gp-gate t0=$previous_choice" 2>&1); then
                fail "retry accepted the old HIP choice"
            fi
            [ "$stale_reply" = 'error why=bad-choice' ] || fail "unexpected stale approval reply: $stale_reply"
            [ "$(wc -l < "$receipts" | tr -d ' ')" = "$before_retry" ] &&
                [ "$(field handshake)" = 0 ] || fail "retry submitted HIP or opened before fresh approval"
            ok "retry reviewed restored posture and rejected stale approval"
            if [ "$retry_hip_submit" -ge 3 ]; then
                declined_choice=$(hip_choice)
                decline_failed_hip_retry "$before_retry"
                ok "declining retry HIP withheld the restored report and kept ESP down"
                if [ "$retry_hip_submit" = 3 ]; then
                    echo "$pass checks passed"
                    exit 0
                fi
                reconnect_after_esp_loss_decline "$before_retry" rebind-approved "${initial_report%% *}"
                retry_reviews=$((retry_reviews + 1))
            fi
            retry_reviews=$((retry_reviews + 1))
            touch "$receipts.retry"
            approve_hip
        else
            touch "$receipts.retry"
        fi
    fi
    for _ in $(seq 1 25); do
        if grep -q "^$changed_word " "$receipts"; then break; fi
        sleep 1
    done
    grep -q "^$changed_word " "$receipts" || fail "changed HIP did not reach the gateway"
    if [ "$decline_after_esp_loss" -ge 2 ]; then
        wait_field handshake 1
    fi
    probe "IPv4 echo after approved HIP refresh"
    [ "$(field starts)" = 1 ] || fail "HIP refresh restarted the engine"
    if [ "$decline_after_esp_loss" -ge 2 ]; then
        changed_report=$(tail -n 1 "$receipts")
        before_refresh=$(awk 'END { print NR }' "$receipts")
        wait_receipts "$((before_refresh + 1))"
        [ "$(field signin)" = in ] &&
            [ "$(sed -n "$((before_refresh + 1))p" "$receipts")" = "$changed_report" ] ||
            fail "unchanged HIP did not refresh silently on restored ESP"
        ok "unchanged HIP refreshed on restored ESP without prompting"
        if [ "$retry_hip_submit" -ne 0 ]; then
            [ "$("$V" "grep -o 'gp-gate: the gateway asks' $G/cryptad.log | wc -l | tr -d ' '")" = "$retry_reviews" ] ||
                fail "HIP retry or refresh opened an extra review"
            probe "IPv4 echo on restored ESP after silent HIP refresh"
            if [ "$retry_hip_submit" -ge 2 ]; then
                [ "$changed_report" = "$initial_report" ] || fail "retry submitted stale posture"
                rebind_restored_forwarding 4
                if [ "$retry_hip_submit" -ge 9 ]; then
                    rebind_restored_forwarding 5 "$rebound_report"
                    if [ "$retry_hip_submit" -ge 10 ]; then
                        [ "$("$V" "grep -o 'GlobalProtect data transport: TLS' $G/cryptad.log | wc -l | tr -d ' '")" = 2 ] ||
                            fail "final rebind did not select TLS"
                        before_refresh=$(awk 'END { print NR }' "$receipts")
                        wait_receipts "$((before_refresh + 1))"
                        [ "$(field signin)" = in ] &&
                            [ "$(sed -n "$((before_refresh + 1))p" "$receipts")" = "$rebound_report" ] &&
                            [ "$("$V" "grep -o 'gp-gate: the gateway asks' $G/cryptad.log | wc -l | tr -d ' '")" = "$reviews" ] ||
                            fail "unchanged HIP did not refresh silently after final TLS fallback"
                        ok "unchanged HIP refreshed silently after final TLS fallback"
                        probe "IPv4 echo after silent refresh on fallback TLS"
                        if [ "$retry_hip_submit" -ge 11 ]; then
                            rebind_restored_forwarding 6 "$rebound_report"
                            [ "$("$V" "grep -o 'GlobalProtect data transport: ESP' $G/cryptad.log | wc -l | tr -d ' '")" = 4 ] ||
                                fail "final recovery rebind did not restore ESP"
                            before_refresh=$(awk 'END { print NR }' "$receipts")
                            wait_receipts "$((before_refresh + 1))"
                            [ "$(field signin)" = in ] &&
                                [ "$(sed -n "$((before_refresh + 1))p" "$receipts")" = "$rebound_report" ] &&
                                [ "$("$V" "grep -o 'gp-gate: the gateway asks' $G/cryptad.log | wc -l | tr -d ' '")" = "$reviews" ] ||
                                fail "unchanged HIP did not refresh silently after ESP recovery"
                            ok "unchanged HIP refreshed silently after ESP recovery"
                            probe "IPv4 echo after silent refresh on recovered ESP"
                            before_restore=$(awk 'END { print NR }' "$receipts")
                            "$V" "sysctl net.inet6.ip6.forwarding=$original" >/dev/null
                            wait_prompt gp_hip_approval
                            printf '%s\n' "$prompt" | sed -n 's/^text k=banner v=//p' | tr -d '\n' |
                                grep -q "$(hex "IPv6 forwarding  ${initial_report%% *}")" ||
                                fail "recovered ESP did not review restored forwarding"
                            [ "$(hip_choice)" != "$approval_choice" ] ||
                                fail "recovered ESP reused the previous HIP choice"
                            if tail -n "+$((before_restore + 1))" "$receipts" | grep -Fxq "$initial_report"; then
                                fail "recovered ESP submitted restored forwarding before approval"
                            fi
                            before_approval=$(awk 'END { print NR }' "$receipts")
                            touch "$receipts.recovery-approved"
                            approve_hip
                            wait_receipts "$((before_approval + 1))"
                            [ "$(tail -n 1 "$receipts")" = "$initial_report" ] ||
                                fail "approved forwarding report did not reach the gateway"
                            ok "restoring IPv6 forwarding on recovered ESP required fresh approval"
                            probe "IPv4 echo after fresh HIP approval on the same recovered ESP tunnel"
                            before_refresh=$(awk 'END { print NR }' "$receipts")
                            wait_receipts "$((before_refresh + 1))"
                            [ "$(field signin)" = in ] &&
                                [ "$(sed -n "$((before_refresh + 1))p" "$receipts")" = "$initial_report" ] &&
                                [ "$("$V" "grep -o 'gp-gate: the gateway asks' $G/cryptad.log | wc -l | tr -d ' '")" = "$((reviews + 1))" ] ||
                                fail "freshly approved forwarding did not refresh silently"
                            ok "freshly approved forwarding refreshed silently on recovered ESP"
                            rebind_restored_forwarding 7 "$initial_report"
                            if [ "$retry_hip_submit" -ge 12 ]; then
                                rebound_iface=$(field iface)
                                [ -n "$rebound_iface" ] || fail "recovered ESP has no rebound interface"
                                before_decline=$(awk 'END { print NR }' "$receipts")
                                "$V" "sysctl net.inet6.ip6.forwarding=$changed" >/dev/null
                                wait_prompt gp_hip_approval
                                printf '%s\n' "$prompt" | sed -n 's/^text k=banner v=//p' | tr -d '\n' |
                                    grep -q "$(hex "IPv6 forwarding  $changed_word")" ||
                                    fail "final rebound review omitted changed forwarding"
                                declined_choice=$(hip_choice)
                                decline_esp_loss_review 7 "$rebound_iface"
                                if tail -n "+$((before_decline + 1))" "$receipts" | grep -q "^$changed_word "; then
                                    fail "the final rebound submitted declined forwarding posture"
                                fi
                                ok "declining changed forwarding stopped recovered ESP without submitting HIP"
                                if [ "$retry_hip_submit" = 12 ]; then
                                    echo "$pass checks passed"
                                    exit 0
                                fi
                                reconnect_after_esp_loss_decline "$before_decline" recovered-reconnect-approved
                                approve_hip
                                wait_field handshake 1
                                [ "$(tail -n 1 "$receipts")" = "$rebound_report" ] ||
                                    fail "reconnect submitted a different forwarding report"
                                probe "IPv4 echo after approval on reconnected recovered ESP"
                                before_refresh=$(awk 'END { print NR }' "$receipts")
                                wait_receipts "$((before_refresh + 1))"
                                [ "$(field signin)" = in ] &&
                                    [ "$(sed -n "$((before_refresh + 1))p" "$receipts")" = "$rebound_report" ] &&
                                    [ "$("$V" "grep -o 'gp-gate: the gateway asks' $G/cryptad.log | wc -l | tr -d ' '")" = "$((reviews + 2))" ] ||
                                    fail "reconnected recovered ESP did not refresh silently"
                                ok "unchanged forwarding refreshed silently after reconnect"
                                probe "IPv4 echo after silent refresh on reconnected ESP"
                                rebind_restored_forwarding 9 "$rebound_report"
                                before_refresh=$(awk 'END { print NR }' "$receipts")
                                wait_receipts "$((before_refresh + 1))"
                                [ "$(field signin)" = in ] &&
                                    [ "$(sed -n "$((before_refresh + 1))p" "$receipts")" = "$rebound_report" ] &&
                                    [ "$("$V" "grep -o 'gp-gate: the gateway asks' $G/cryptad.log | wc -l | tr -d ' '")" = "$reviews" ] ||
                                    fail "reconnected and rebound ESP did not refresh silently"
                                ok "unchanged forwarding refreshed silently after reconnected ESP rebound"
                                probe "IPv4 echo after silent refresh on the final rebound ESP"
                                if [ "$retry_hip_submit" -ge 14 ]; then
                                    touch "$receipts.drop-active-refresh"
                                    for _ in $(seq 1 25); do
                                        failures=$("$V" "grep -o 'gp-gate: not connected (GlobalProtect HIP submission failed:' $G/cryptad.log | wc -l | tr -d ' '")
                                        [ "$failures" != 3 ] || break
                                        sleep 1
                                    done
                                    [ "$failures" = 3 ] && [ "$(field handshake)" = 0 ] ||
                                        fail "the active HIP refresh failure did not take ESP down"
                                    "$V" 'if ping -n -c 1 -t 2 -S 10.77.0.2 10.77.0.1 >/dev/null 2>&1; then exit 1; fi' ||
                                        fail "IPv4 echo passed before the active refresh retry was acknowledged"
                                    ok "active HIP refresh failure kept traffic blocked until retry"
                                    retry_report=$rebound_report
                                    if [ "$retry_hip_submit" -ge 15 ]; then
                                        stale_choice=$approval_choice
                                        before_retry=$(awk 'END { print NR }' "$receipts")
                                        "$V" "sysctl net.inet6.ip6.forwarding=$original" >/dev/null
                                        touch "$receipts.active-refresh-posture"
                                        wait_prompt gp_hip_approval
                                        printf '%s\n' "$prompt" | sed -n 's/^text k=banner v=//p' | tr -d '\n' |
                                            grep -q "$(hex "IPv6 forwarding  ${initial_report%% *}")" ||
                                            fail "active HIP retry reviewed stale posture"
                                        if stale_reply=$("$V" "$CTL answer name=gp-gate t0=$stale_choice" 2>&1); then
                                            fail "active HIP retry accepted stale approval"
                                        fi
                                        [ "$stale_reply" = 'error why=bad-choice' ] &&
                                            [ "$(field handshake)" = 0 ] &&
                                            [ "$(awk 'END { print NR }' "$receipts")" = "$before_retry" ] ||
                                            fail "active HIP retry failed to withhold upload or traffic before fresh approval"
                                        if [ "$retry_hip_submit" -ge 16 ]; then
                                            declined_choice=$(hip_choice)
                                            decline_failed_hip_retry "$before_retry"
                                            ok "declining the active refresh retry withheld the report and kept ESP down"
                                            if [ "$retry_hip_submit" = 16 ]; then
                                                echo "$pass checks passed"
                                                exit 0
                                            fi
                                            reconnect_after_esp_loss_decline "$before_retry" active-refresh-reconnect "${initial_report%% *}"
                                            reviews=$((reviews + 1))
                                        fi
                                        reviews=$((reviews + 1))
                                        retry_report=$initial_report
                                        touch "$receipts.active-refresh-retry"
                                        approve_hip
                                        ok "active refresh retry rejected stale approval and reviewed restored forwarding"
                                    else
                                        touch "$receipts.active-refresh-retry"
                                    fi
                                    wait_field handshake 1
                                    [ "$(tail -n 1 "$receipts")" = "$retry_report" ] ||
                                        fail "active refresh retry submitted a different report"
                                    probe "IPv4 echo after active HIP refresh retry"
                                    before_refresh=$(awk 'END { print NR }' "$receipts")
                                    wait_receipts "$((before_refresh + 1))"
                                    [ "$(field signin)" = in ] &&
                                        [ "$(sed -n "$((before_refresh + 1))p" "$receipts")" = "$retry_report" ] &&
                                        [ "$("$V" "grep -o 'gp-gate: the gateway asks' $G/cryptad.log | wc -l | tr -d ' '")" = "$reviews" ] ||
                                        fail "active refresh retry prompted or changed the report"
                                    ok "unchanged HIP refreshed silently after active refresh retry"
                                    probe "IPv4 echo after silent refresh on the retry ESP association"
                                    if [ "$retry_hip_submit" -ge 17 ]; then
                                        rebind_restored_forwarding 11 "$retry_report"
                                        if [ "$retry_hip_submit" -ge 18 ]; then
                                            [ "$("$V" "grep -o 'GlobalProtect data transport: TLS' $G/cryptad.log | wc -l | tr -d ' '")" = 3 ] ||
                                                fail "the rebound timer-refresh retry did not select TLS"
                                        fi
                                        before_refresh=$(awk 'END { print NR }' "$receipts")
                                        wait_receipts "$((before_refresh + 1))"
                                        [ "$(field signin)" = in ] &&
                                            [ "$(sed -n "$((before_refresh + 1))p" "$receipts")" = "$retry_report" ] &&
                                            [ "$("$V" "grep -o 'gp-gate: the gateway asks' $G/cryptad.log | wc -l | tr -d ' '")" = "$reviews" ] ||
                                            fail "HIP refreshed differently or prompted after rebind of the retry tunnel"
                                        ok "the approved retry report refreshed silently after rebind"
                                        probe "IPv4 echo after silent refresh on the rebound retry tunnel"
                                        if [ "$retry_hip_submit" = 19 ]; then
                                            rebind_restored_forwarding 12 "$retry_report"
                                            before_refresh=$(awk 'END { print NR }' "$receipts")
                                            wait_receipts "$((before_refresh + 1))"
                                            [ "$(field signin)" = in ] &&
                                                [ "$(sed -n "$((before_refresh + 1))p" "$receipts")" = "$retry_report" ] &&
                                                [ "$("$V" "grep -o 'gp-gate: the gateway asks' $G/cryptad.log | wc -l | tr -d ' '")" = "$reviews" ] ||
                                                fail "HIP refreshed differently or prompted after ESP recovery"
                                            ok "the approved timer-retry report refreshed silently on recovered ESP"
                                            probe "IPv4 echo after silent refresh on recovered timer-retry ESP"
                                        fi
                                    fi
                                fi
                            fi
                        fi
                    fi
                fi
            fi
            "$V" "$CTL down name=gp-gate" >/dev/null
            echo "$pass checks passed"
            exit 0
        fi
        restore_forwarding_and_rebind 4
        if [ "$decline_after_esp_loss" -ge 3 ]; then
            rebound_iface=$(field iface)
            [ -n "$rebound_iface" ] || fail "the rebound ESP tunnel has no interface"
            before_decline=$(awk 'END { print NR }' "$receipts")
            "$V" "sysctl net.inet6.ip6.forwarding=$changed" >/dev/null
            wait_prompt gp_hip_approval
            printf '%s\n' "$prompt" | sed -n 's/^text k=banner v=//p' | tr -d '\n' |
                grep -q "$(hex "IPv6 forwarding  $changed_word")" || fail "review omitted the second change"
            declined_choice=$(hip_choice)
            [ -n "$declined_choice" ] || fail "missing declined rebound choice"
            decline_esp_loss_review 4 "$rebound_iface"
            if tail -n "+$((before_decline + 1))" "$receipts" | grep -q "^$changed_word "; then
                fail "the second changed report reached the gateway despite decline"
            fi
            ok "decline shut down rebound ESP without sending the changed report"
            if [ "$decline_after_esp_loss" = 3 ]; then
                echo "$pass checks passed"
                exit 0
            fi
            reconnect_after_esp_loss_decline "$before_decline" rebind-approved
            approve_hip
            wait_field handshake 1
            [ "$(tail -n 1 "$receipts")" = "$changed_report" ] ||
                fail "rebound reconnect did not submit the approved changed report"
            probe "IPv4 echo after approval on the new ESP tunnel"
            before_refresh=$(awk 'END { print NR }' "$receipts")
            wait_receipts "$((before_refresh + 1))"
            [ "$(field signin)" = in ] &&
                [ "$(sed -n "$((before_refresh + 1))p" "$receipts")" = "$changed_report" ] ||
                fail "the new ESP tunnel did not refresh unchanged HIP silently"
            ok "unchanged HIP refreshed silently on the new ESP tunnel"
            probe "IPv4 echo after silent HIP refresh on new ESP"
            restore_forwarding_and_rebind 6
        fi
    fi
    "$V" "$CTL down name=gp-gate" >/dev/null
    ok "HIP refreshed on the active tunnel"
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
