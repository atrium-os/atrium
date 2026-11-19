#!/bin/sh
# M9: click through Curia in a dedicated FreeBSD amd64 guest, then verify
# actual Git operations using the jailed Arca agent. Builds run on the host.
# The guest must have rctl enabled and an x86-64-v3 CPU (e.g. Haswell-noTSX
# under recent QEMU TCG). Its pkg repositories or cached git-lite packages
# must be available. This gate replaces services and initializes test data.
#
#   VMDIR=/path/to/amd64-guest SSH_AGENT_GATE_PORT=4222 \
#     SSH_AGENT_GATE_KEY=/path/to/test-key sh scripts/vm-m9-curia-check.sh [out-dir]
#
# VMDIR/sysroot is the guest sysroot. With out-dir, save screenshots there.
set -eu
ROOT=$(cd "$(dirname "$0")/.." && pwd)
OUT=${1:-}
[ -z "$OUT" ] || mkdir -p "$OUT"
export ARCH=amd64
SMP=${SMP:-1}
set -- env
. "$ROOT/scripts/cloud-vm.sh" >/dev/null
set --
PORT=${SSH_AGENT_GATE_PORT:-2222}
KEY=${SSH_AGENT_GATE_KEY:-$HOME/.ssh/fresco_bsd_ed25519}
V() {
    ssh -i "$KEY" -p "$PORT" -o BatchMode=yes -o StrictHostKeyChecking=no \
        -o UserKnownHostsFile=/dev/null -o LogLevel=ERROR -o ConnectTimeout=5 \
        -o ServerAliveInterval=5 -o ServerAliveCountMax=3 root@localhost "$@"
}
copy() {
    scp -q -i "$KEY" -P "$PORT" -o BatchMode=yes -o StrictHostKeyChecking=no \
        -o UserKnownHostsFile=/dev/null -o LogLevel=ERROR -o ConnectTimeout=5 "$@"
}
B=/var/tmp/m9-curia-gate
phase='guest checks'
checks=0
trap 'status=$?; if [ "$status" -ne 0 ]; then
    echo "m9-curia: failed at $phase ($checks checks passed)" >&2
fi' EXIT
ok() { echo "  ok $1"; checks=$((checks+1)); }

[ "$(V 'uname -sm; sysctl -n kern.racct.enable')" = "$(printf 'FreeBSD amd64\n1')" ]
[ -f "$VMDIR/sysroot/usr/lib/Scrt1.o" ]
phase='host cross-build'
xb() (
    cd "$ROOT/$1"
    shift
    RUSTFLAGS="$CROSS_RUSTFLAGS" cargo build --locked --release --target "$TARGET" "$@"
)
xb frescod --bin frescod
xb curia --bin curia
xb portcullis -p opifex --bin opifex

phase='jailed SSH-agent prerequisite'
sh "$ROOT/scripts/vm-ssh-agent-check.sh"
ok 'jailed-agent gate passed'

phase='signed Curia and Fresco setup'
V 'sh -s' <<'GUEST'
set -eu
B=/var/tmp/m9-curia-gate
# The only processes of these names on this disposable guest are test fixtures.
pkill -x curia || true
pkill -x frescod || true
i=0
while { pgrep -x curia || pgrep -x frescod; } >/dev/null && [ "$i" -lt 20 ]; do
    sleep 1
    i=$((i+1))
done
! pgrep -x curia >/dev/null
! pgrep -x frescod >/dev/null
install -d -m 0755 "$B" /usr/local/share/fonts/ibm-plex
rm -f "$B"/screen.png "$B"/inject.sock "$B"/activation.sock "$B"/wm.sock
GUEST
for binary in frescod/target/"$TARGET"/release/frescod curia/target/"$TARGET"/release/curia \
    portcullis/target/"$TARGET"/release/opifex; do
    copy "$ROOT/$binary" "root@localhost:$B/"
done
for font in "$ROOT"/fonts/ibm-plex/*.ttf "$ROOT"/fonts/phosphor/*.ttf; do
    copy "$font" root@localhost:/usr/local/share/fonts/ibm-plex/
done
V 'sh -s' <<'GUEST'
set -eu
B=/var/tmp/m9-curia-gate
P=/atrium/sockets/portcullis/portcullis.sock
F=/atrium/sockets/fresco/fresco.sock
C=org.atrium.curia
install -d -m 0755 /etc/atrium/publishers /atrium/sockets/fresco "$B/bundle/bin"
install -m 0755 "$B/curia" "$B/bundle/bin/curia"
cat > "$B/bundle/atrium.toml" <<'MANIFEST'
[app]
id = "org.atrium.curia"
name = "Settings"
version = "1"
entry = "bin/curia"

[capabilities]
graphics = "fresco"
app-grants = true
MANIFEST
umask 077
test -r "$B/publisher.pem" || openssl ecparam -name prime256v1 -genkey -noout -out "$B/publisher.pem"
umask 022
openssl ec -in "$B/publisher.pem" -pubout -out /etc/atrium/publishers/m9-gate.pem 2>/dev/null
openssl dgst -sha256 -sign "$B/publisher.pem" -out "$B/bundle/atrium.toml.sig" "$B/bundle/atrium.toml"
if ! cmp -s "$B/curia" "/var/lib/atrium/apps/$C/bin/curia" ||
   ! cmp -s "$B/bundle/atrium.toml" "/var/lib/atrium/apps/$C/atrium.toml"; then
    "$B/opifex" install "$B/bundle"
fi
cmp -s "$B/curia" "/var/lib/atrium/apps/$C/bin/curia"
pkill -x portcullisd
sleep 1
rm -f "$P" "$F"
daemon -f -o "$B/portcullisd.log" env PORTCULLISD_FRESCO_ACTIVATION="$B/activation.sock" \
    /usr/local/bin/portcullisd --socket "$P"
i=0
while [ "$i" -lt 20 ] && [ ! -S "$P" ]; do sleep 1; i=$((i+1)); done
chmod 666 "$P"
portcullis policy grant --system "$C"
cd "$B"
daemon -f -o "$B/frescod.log" env FRESCOD_SPLIT=0 FRESCOD_HEADLESS_PNG="$B/screen" \
    FRESCOD_SOCK="$F" FRESCOD_WM_SOCK="$B/wm.sock" FRESCOD_INPUT_SOCK="$B/inject.sock" \
    FRESCOD_ACTIVATION_SOCK="$B/activation.sock" FRESCOD_SLOT_UIDS=0 FRESCOD_DEV_CAPS=all \
    FRESCOD_INJECT_ACTIVATION=1 FRESCOD_ACTIVATION_UIDS=0 "$B/frescod"
i=0
while [ "$i" -lt 30 ] && { [ ! -S "$F" ] || [ ! -S "$B/inject.sock" ] || [ ! -S "$B/activation.sock" ]; }; do
    sleep 1
    i=$((i+1))
done
chmod 666 "$F"
test -S "$B/inject.sock"
test -S "$B/activation.sock"
su -m sg-unlock -c '/var/tmp/sshgate-bin/arca-probe /atrium/sockets/arca/arcad.sock unlock alice correct-horse'
# The prerequisite reset its private store and populated only fixture items.
items=$(su -m alice -c 'arca-ssh list' | cut -d ' ' -f 1)
for id in $items; do
    test "${#id}" = 32
    su -m alice -c "arca-ssh delete $id"
done
su -m alice -c 'arca-ssh new m9-click-key' > "$B/key-meta"
printf '%s\n' m9-synthetic-token | su -m alice -c 'arca-ssh token m9.invalid fixture-user' > "$B/token-id"
daemon -f -o "$B/curia.log" su -m alice -c "portcullis launch --no-prompt $C"
GUEST

phase='Git workspace setup'
V 'sh -s' <<'GUEST'
set -eu
B=/var/tmp/m9-curia-gate
W=/home/alice/.atrium/workspaces
cat > "$B/git-probe.sh" <<'PROBE'
#!/bin/sh
set -eu
export LC_ALL=C GIT_TERMINAL_PROMPT=0
export GIT_CONFIG_GLOBAL=/dev/null GIT_CONFIG_SYSTEM=/dev/null
cd m9-git
push() {
    export GIT_SSH_COMMAND="ssh -F /dev/null -o BatchMode=yes -o IdentitiesOnly=yes -i $PWD/key.pub -o UserKnownHostsFile=/dev/null -o StrictHostKeyChecking=no -o ConnectTimeout=4"
    git -C source push --receive-pack='/usr/local/bin/git receive-pack' \
        "ssh://$(id -un)@127.0.0.1:2223$PWD/target.git" "HEAD:refs/heads/$1"
}
case "$1" in
    prep)
        install -m 0600 key.pub authorized_keys
        host_keys=$(mktemp -d /tmp/m9-git-host.XXXXXX)
        trap 'rm -rf "$host_keys"' EXIT
        ssh-keygen -q -t ed25519 -N '' -f "$host_keys/key"
        /usr/sbin/sshd -f /dev/null -h "$host_keys/key" -p 2223 -o ListenAddress=127.0.0.1 \
            -o PidFile=/tmp/m9-git-sshd.pid -o StrictModes=no -o UsePAM=no \
            -o AuthorizedKeysFile="$PWD/authorized_keys"
        git init -q --bare target.git
        git init -q source
        git -C source -c user.name=M9 -c user.email=m9@example.invalid commit -qm 'M9 fixture' --allow-empty
        echo 'loopback Git fixture ready'
        ;;
    push)
        push main
        test "$(git -C source rev-parse HEAD)" = "$(git --git-dir=target.git rev-parse --verify refs/heads/main)"
        echo 'remote ref matches source'
        ;;
    deny-push)
        if output=$(push denied 2>&1); then exit 1; fi
        printf '%s\n' "$output" | grep -qF 'Permission denied (publickey'
        if git --git-dir=target.git show-ref --verify --quiet refs/heads/denied; then exit 1; fi
        echo 'SSH denied and remote ref absent'
        ;;
    fill)
        output=$(printf 'protocol=https\nhost=%s\n\n' "$2" | git -c credential.interactive=false credential fill)
        printf '%s\n' "$output" | grep -qx 'username=fixture-user'
        printf '%s\n' "$output" | grep -qx 'password=m9-synthetic-token'
        echo 'Git returned the fixture credential'
        ;;
    deny-fill)
        if output=$(printf 'protocol=https\nhost=%s\n\n' "$2" | git -c credential.interactive=false credential fill 2>&1); then exit 1; fi
        if printf '%s\n' "$output" | grep -q '^password='; then exit 1; fi
        printf '%s\n' "$output" | grep -Eq 'unable to get password|could not read Username|terminal prompts disabled'
        echo 'Git refused without a password'
        ;;
    *) exit 2 ;;
esac
PROBE
for workspace in keys other; do
    su -m alice -c "cd /tmp; portcullis workspace exec $workspace -- /usr/bin/true"
    jail=ws_alice_$workspace
    root=$(jls -j "$jail" path)
    if [ ! -x "$root/usr/local/bin/git" ]; then
        env ASSUME_ALWAYS_YES=yes pkg bootstrap -y
        archive=/var/cache/pkg/$(pkg query '%n-%v' git-lite || true).pkg
        if [ ! -f "$archive" ]; then
            pkg install -y git-lite
            pkg fetch -dy git-lite
            archive=/var/cache/pkg/$(pkg query '%n-%v' git-lite).pkg
        fi
        test -f "$archive"
        if ! pkg -r "$root" add "$archive"; then
            pkg fetch -dy git-lite
            pkg -r "$root" add "$archive"
        fi
    fi
    jexec "$jail" /sbin/ldconfig /usr/local/lib
    su -m alice -c "cd /tmp; portcullis workspace exec $workspace -- git --version" | grep '^git version '
    rm -rf "$W/$workspace/work/m9-git"
    su -m alice -c "cd /tmp; portcullis workspace exec $workspace -- /bin/mkdir m9-git"
    test -d "$W/$workspace/work/m9-git"
    install -m 0644 "$B/git-probe.sh" "$W/$workspace/work/m9-git-probe.sh"
done
sed 's/^[^ ]* //' "$B/key-meta" > "$W/keys/work/m9-git/key.pub"
chmod 0644 "$W/keys/work/m9-git/key.pub"
GUEST

run_git() {
    expected=$1 workspace=$2 operation=$3 host=${4:-}
    output=$(V "su -m alice -c 'cd /tmp; portcullis workspace exec $workspace -- /bin/sh m9-git-probe.sh $operation $host'" 2>&1)
    # The workspace CLI can exit successfully when the child fails. Require
    # the probe's success marker, emitted only after its assertions.
    if ! printf '%s\n' "$output" | grep -Fxq "$expected"; then
        printf "%s: Git probe '%s' did not report success\n%s\n" "$workspace" "$operation" "$output" >&2
        return 1
    fi
    ok "$workspace: $expected${host:+ ($host)}"
}
await_remote() {
    for attempt in 1 2 3 4 5 6 7 8 9 10 11 12 13 14 15; do
        V "$1" >/dev/null 2>&1 && return 0
        sleep 2
    done
    return 1
}
press() {
    V "printf '%s\\n' 'MOVE $1 $2' 'BTN 1 1' 'BTN 1 0' | nc -N -U $B/inject.sock"
    sleep 4
}
shot() {
    [ -z "$OUT" ] || copy "root@localhost:$B/screen.png" "$OUT/$1.png"
}
expect_grants() {
    # Match the freshly minted label and exact list of granted workspaces.
    # This is a read-only observer, independent of Curia's rendering.
    await_remote "su -m alice -c 'arca-ssh list' | awk '
        / ssh-ed25519 [^ ]+ m9-click-key granted=$1\$/ { key=1 }
        / token m9[.]invalid fixture-user granted=$2\$/ { token=1 }
        END { exit !(key && token && NR == 2) }
    '"
}

phase='untouched credentials and loopback Git baseline'
await_remote "pgrep -x curia && pgrep -x frescod && test -s $B/screen.png"
run_git 'loopback Git fixture ready' keys prep
expect_grants '' ''
run_git 'SSH denied and remote ref absent' keys deny-push
run_git 'Git refused without a password' keys deny-fill m9.invalid

# Fixed 960x720 Curia window at (0, 0), headless Fresco with no WM.
# Workspace list sorts Scratch first, then keys and other. Curia sorts
# SSH keys before HTTPS tokens, with exactly one row of each in this fixture.
phase='open Curia Workspace access'
press 110 415
shot workspaces
press 870 142
shot credentials

phase='click SSH key Grant and push'
press 885 129
expect_grants keys ''
ok 'Curia granted the SSH key to keys'
run_git 'remote ref matches source' keys push

phase='click HTTPS token Grant and fill'
press 885 185
expect_grants keys keys
ok 'Curia granted the HTTPS token to keys'
run_git 'Git returned the fixture credential' keys fill m9.invalid
run_git 'Git refused without a password' other deny-fill m9.invalid
run_git 'Git refused without a password' keys deny-fill other.invalid
shot granted

phase='click SSH key Revoke and reject push'
press 885 129
expect_grants '' keys
ok 'Curia revoked the SSH key'
run_git 'SSH denied and remote ref absent' keys deny-push
run_git 'Git returned the fixture credential' keys fill m9.invalid

phase='click HTTPS token Revoke and reject fill'
press 885 185
expect_grants '' ''
ok 'Curia revoked the HTTPS token'
run_git 'Git refused without a password' keys deny-fill m9.invalid
shot revoked

phase='stop test app and workspaces'
V "pkill -x curia || true; pkill -x frescod || true
   su -m alice -c 'cd /tmp; portcullis workspace stop keys && portcullis workspace stop other' >/dev/null &&
   ! jls -j ws_alice_keys jid >/dev/null 2>&1 &&
   ! jls -j ws_alice_other jid >/dev/null 2>&1"
echo "m9-curia: $checks checks passed, 0 failed"
