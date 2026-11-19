#!/bin/sh
# Dev-tools M9 (docs/spec/dev-tools-plan.md, arca.md §9.1): SSH keys in
# Arca, used from workspaces through the agent. Runs on the HOST after
#   ARCH=amd64 sh scripts/cloud-vm.sh up
#   sh scripts/vm-ssh-agent-check.sh
# VMDIR provides the amd64 sysroot. SSH_AGENT_GATE_PORT and
# SSH_AGENT_GATE_KEY can select a separately forwarded disposable guest.
# Ends "ssh-agent: N failed".
#
# Run only in a dedicated test guest: the gate replaces portcullisd and uses
# arcad's production socket path with a private store. The shipped services.d
# entry starts the agent via jaild as _arca-ssh (65047). It checks:
#
#   - alice makes a key with arca-ssh; she gets its public line, no file;
#   - a workspace started with the agent running has SSH_AUTH_SOCK and its
#     own read-only folder holding only its socket;
#   - nothing is listed before a grant; after `arca-ssh grant`, ssh-add -L in
#     that workspace lists the key, and another workspace still lists none;
#   - `ssh-keygen -Y sign` in the granted workspace signs through the agent
#     and the signature verifies on the host; in the other it fails;
#   - ssh logs in to the VM's sshd with the key from the granted workspace
#     (as git over SSH would), and not from the other;
#   - no private key is anywhere in the workspace's files;
#   - the socket refuses a uid other than the workspace's (alice herself);
#   - an HTTPS token stored with `arca-ssh token` and granted to a
#     workspace: git there has credential.helper=arca, and
#     git-credential-arca hands it over for that host only, not to the
#     other workspace;
#   - revoke ends it; stopping a workspace closes its socket; a locked
#     store lists nothing. (Deleting a workspace needs a person's gesture,
#     so its grants ending is arcad's test, tests/ssh.rs.)
set -u
ROOT=$(cd "$(dirname "$0")/.." && pwd)
export ARCH=amd64
SMP=${SMP:-1}
set -- env
. "$ROOT/scripts/cloud-vm.sh" >/dev/null
set --
GATE_PORT=${SSH_AGENT_GATE_PORT:-2222}
GATE_KEY=${SSH_AGENT_GATE_KEY:-$HOME/.ssh/fresco_bsd_ed25519}
V() {
    ssh -i "$GATE_KEY" -p "$GATE_PORT" -o BatchMode=yes -o StrictHostKeyChecking=no \
        -o UserKnownHostsFile=/dev/null -o LogLevel=ERROR -o ConnectTimeout=5 \
        -o ServerAliveInterval=5 -o ServerAliveCountMax=3 root@localhost "$@"
}
put() {
    scp -q -i "$GATE_KEY" -P "$GATE_PORT" -o BatchMode=yes -o StrictHostKeyChecking=no \
        -o UserKnownHostsFile=/dev/null -o LogLevel=ERROR -o ConnectTimeout=5 \
        -o ServerAliveInterval=5 -o ServerAliveCountMax=3 "$1" "root@localhost:$2"
}
[ "$(V 'uname -m')" = amd64 ] || {
    echo "ssh-agent: dedicated FreeBSD amd64 guest is required"
    exit 1
}
fails=0
ok() { echo "  ok $1"; }
bad() { echo "  FAIL $1"; fails=$((fails+1)); }
check() { if [ "$2" = "$3" ]; then ok "$1"; else bad "$1: expected [$2] got [$3]"; fi; }
has() { case "$3" in *"$2"*) ok "$1" ;; *) bad "$1: no [$2] in [$3]" ;; esac; }
hasnt() { case "$3" in *"$2"*) bad "$1: [$2] in [$3]" ;; *) ok "$1" ;; esac; }

echo "=== cross-build"
(cd "$ROOT/portcullis" && RUSTFLAGS="$CROSS_RUSTFLAGS" cargo build -q --release --target "$TARGET" \
    -p jaild -p portcullisd -p portcullis-cli --bin atrium-jaild --bin portcullisd --bin portcullis --bin atrium-portcullisd-bootstrap) || { echo "ssh-agent: 1 failed"; exit 1; }
(cd "$ROOT/arca/arcad" && RUSTFLAGS="$CROSS_RUSTFLAGS" cargo build -q --release --target "$TARGET" --bins) || { echo "ssh-agent: 1 failed"; exit 1; }
(cd "$ROOT/arca/arca-ssh-agent" && RUSTFLAGS="$CROSS_RUSTFLAGS" cargo build -q --release --target "$TARGET" --bins) || { echo "ssh-agent: 1 failed"; exit 1; }

echo "=== install"
B=/var/tmp/sshgate-bin
G=/tmp/sshgate
S=/atrium/sockets/arca/arcad.sock
AR=/atrium/sockets/ssh-agent
AGENT_LOG=/var/log/atrium/atrium-ssh-agent.log
V "test -f $G/arcad.pid || sockstat -lu | awk -v path='$S' '\$6 == path { found=1 } END { exit found }'" || {
    echo "ssh-agent: arcad already owns the production socket"
    exit 1
}
V "if test -s $G/bootstrap.pid; then kill \$(cat $G/bootstrap.pid) 2>/dev/null || true; fi
    i=0; while jls -j atrium-ssh-agent jid >/dev/null 2>&1 && test \$i -lt 20; do
        sleep 1; i=\$((i+1))
    done
    ! jls -j atrium-ssh-agent jid >/dev/null 2>&1" || {
    echo "ssh-agent: an existing agent jail is still running"
    exit 1
}
V "test ! -s $G/arcad.pid || kill \$(cat $G/arcad.pid) 2>/dev/null || true"
V "pkill -x portcullisd; pkill -f $B/; sleep 1; rm -rf $B $G; mkdir -p $B $G; chmod 755 $B"
for b in portcullis/target/$TARGET/release/atrium-jaild \
         portcullis/target/$TARGET/release/portcullisd portcullis/target/$TARGET/release/portcullis \
         portcullis/target/$TARGET/release/atrium-portcullisd-bootstrap \
         arca/arcad/target/$TARGET/release/arcad arca/arcad/target/$TARGET/release/arca-probe \
         arca/arca-ssh-agent/target/$TARGET/release/atrium-ssh-agent arca/arca-ssh-agent/target/$TARGET/release/arca-ssh \
         arca/arca-ssh-agent/target/$TARGET/release/git-credential-arca \
         arca/arca-ssh-agent/etc/services.d/44-atrium-ssh-agent.toml \
         scripts/ssh-agent-service.sh etc/jaild.policy.toml etc/atrium.pf etc/atrium.devfs.rules; do
    put "$ROOT/$b" "$B/" || { echo "copy $b failed"; echo "ssh-agent: 1 failed"; exit 1; }
done
put "$ROOT/portcullis/jaild/etc/atrium-jaild" "$B/jaild.rc" || exit 1
PSOCK=/atrium/sockets/portcullis/portcullis.sock
LOG=$B/portcullisd.log
V 'sh -s' <<EOS
set -e
mkdir -p /usr/local/bin /usr/local/etc/rc.d
for b in portcullisd portcullis arca-ssh git-credential-arca; do install -m 0755 $B/\$b /usr/local/bin/\$b; done
mkdir -p /etc/atrium; cp $B/jaild.policy.toml /etc/atrium/jaild.policy.toml
grep -q '^allow_rctl' /etc/atrium/jaild.policy.toml || printf '\n[resource_control]\nallow_rctl = true\n' >> /etc/atrium/jaild.policy.toml
id alice >/dev/null 2>&1 || pw useradd alice -m -s /bin/sh
id sshtest >/dev/null 2>&1 || pw useradd sshtest -m -s /bin/sh
for u in sg-unlock:50093 _arca-ssh:65047; do n=\${u%%:*}; i=\${u##*:}; id \$n >/dev/null 2>&1 || pw useradd \$n -u \$i -d /nonexistent -s /usr/sbin/nologin; done
service sshd onestatus >/dev/null 2>&1 || service sshd onestart >/dev/null 2>&1
if service atrium-jaild onestatus >/dev/null 2>&1; then service atrium-jaild onestop; fi
install -m 0555 $B/jaild.rc /usr/local/etc/rc.d/atrium-jaild
install -m 0555 $B/atrium-jaild /usr/local/bin/atrium-jaild
# Load the shipped isolation rules before allocating routed workspace jails.
if ! pfctl -s info 2>/dev/null | grep -q 'Status: Enabled'; then
    kldstat -q -m pf || kldload pf
    E=\$(route -n get default | awk '/interface:/ { print \$2 }')
    test -n "\$E"
    ifconfig "\$E" group atrium_egress
    pfctl -f $B/atrium.pf
    pfctl -e
fi
pfctl -s rules | grep -q 'anchor "atrium/\*"'
sysctl -q net.inet.ip.forwarding=1
if ! devfs rule -s 22 show 2>/dev/null | grep -q .; then
    mkdir -p /usr/local/etc/atrium
    install -m 0644 $B/atrium.devfs.rules /usr/local/etc/atrium/devfs.rules
    sysrc -q devfs_rulesets="/etc/defaults/devfs.rules /etc/devfs.rules /usr/local/etc/atrium/devfs.rules" devfs_load_rulesets=NO
    service atrium-jaild onedevfsrules
fi
devfs rule -s 4 show | grep -q .
devfs rule -s 22 show | grep -q .
service atrium-jaild onestart
mkdir -p /var/run/atrium /var/log/atrium /atrium/sockets/arca $G/services.d
touch /var/run/atrium/app-registry
# Recreate the package-relative path expected by the commissioning script.
mkdir -p $B/scripts $B/arca/arca-ssh-agent/etc/services.d
cp $B/ssh-agent-service.sh $B/scripts/
cp $B/44-atrium-ssh-agent.toml $B/arca/arca-ssh-agent/etc/services.d/
rm -rf $AR
sh $B/scripts/ssh-agent-service.sh $B
cp /etc/atrium/services.d/44-atrium-ssh-agent.toml $G/services.d/
rm -f $S
: > $AGENT_LOG
cd $G && env ARCA_DIR=$G/db ARCA_SOCKET=$S ARCA_APP_REGISTRY=/var/run/atrium/app-registry ARCA_VESTIBULUM_UID=50093 \
   ARCA_SERVICES=ssh-agent=65047 ARCA_COURIER_REGISTRY=$G/none ARCA_SHARE_REGISTRY=$G/none ARCA_SIGILLUM_SOCKET=none \
   daemon -p $G/arcad.pid -o $G/arcad.log $B/arcad </dev/null >/dev/null 2>&1
i=0; while [ \$i -lt 30 ] && ! test -S $S; do sleep 1; i=\$((i+1)); done
chmod 755 $G; chmod 666 $S
daemon -f -P $G/bootstrap.pid -o $G/bootstrap.log $B/atrium-portcullisd-bootstrap \
    --socket /var/run/atrium/jaild.sock --volumes-socket "" --services-dir $G/services.d
i=0; while [ \$i -lt 20 ] && ! test -S $AR/control.sock; do sleep 1; i=\$((i+1)); done
test -S $AR/control.sock
jid=\$(jls -j atrium-ssh-agent jid)
test "\$jid" -gt 0
ps -axo uid,jid,comm | awk -v jid="\$jid" '\$1 == 65047 && \$2 == jid && \$3 ~ /(^|\/)atrium-ssh-agent$/ { found=1 } END { exit !found }'
mkdir -p /atrium/sockets/portcullis /var/run/atrium; rm -f $PSOCK; touch /var/run/atrium/app-registry
: > $LOG
daemon -f -o $LOG /usr/local/bin/portcullisd --socket $PSOCK
sleep 3; test -S $PSOCK; chmod 666 $PSOCK
EOS
[ $? = 0 ] && ok "arcad, the jailed agent (uid 65047) and portcullisd are up" || { bad "setup: $(V "tail -8 $G/bootstrap.log $G/arcad.log $AGENT_LOG $LOG")"; echo "ssh-agent: $fails failed"; exit 1; }
r=$(V "su -m sg-unlock -c '$B/arca-probe $S unlock alice correct-horse'")
has "alice's store unlocked" "unlocked" "$r"

echo "=== a key"
SSHA() { V "su -m alice -c 'cd /tmp; arca-ssh $*'" 2>&1; }
NEW=$(SSHA new alice@atrium-test)
ID=$(echo "$NEW" | awk '{print $1}')
PUB=$(echo "$NEW" | cut -d' ' -f2-)
check "arca-ssh new: an id" 32 "${#ID}"
has "arca-ssh new: a public line" "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5" "$PUB"
check "nothing written to alice's home" "" "$(V 'ls /home/alice/.ssh 2>/dev/null')"
has "arca-ssh list shows it, granted to nothing" "$ID ssh-ed25519" "$(SSHA list)"

echo "=== workspaces"
A() { V "su -m alice -c 'cd /tmp; portcullis workspace $*'" 2>&1; }
for w in keys other; do A stop $w >/dev/null; A new $w --network outbound >/dev/null; done
W=/home/alice/.atrium/workspaces
SCRIPT=$(mktemp "${TMPDIR:-/tmp}/sshgate-script.XXXXXX") || exit 1
trap 'rm -f "$SCRIPT"' EXIT
# Run a script in workspace $1 (written to its work folder, run from there).
run() {
    printf '%s\n' "$2" > "$SCRIPT"
    put "$SCRIPT" "$W/$1/work/t.sh" || return
    V "chmod 644 $W/$1/work/t.sh"
    A exec $1 -- /bin/sh t.sh
}
out=$(run keys 'echo sock=$SSH_AUTH_SOCK; ls -A /atrium/ssh-agent; touch /atrium/ssh-agent/x 2>&1 || echo ro; ssh-add -L 2>&1')
has "SSH_AUTH_SOCK is set" "sock=/atrium/ssh-agent/agent.sock" "$out"
has "its folder holds only its socket" "agent.sock
" "$out"
has "and is read-only" "ro" "$out"
has "no identities before a grant" "no identities" "$out"
check "the workspace's socket folders" "alice.keys" "$(V "ls $AR | grep -v -e control -e state | tr '\n' ' ' | sed 's/ \$//'")"

echo "=== grant via the broker's privileged agent protocol"
agent_admin() { V "printf '%s\\n' '$*' | nc -w 5 -U '$AR/control.sock'"; }
has "broker lists key metadata" "$ID ssh-ed25519" "$(agent_admin list alice personal)"
r=$(V "printf '%s\\n' 'grant alice personal $ID keys' | su -m alice -c 'nc -w 5 -U $AR/control.sock'")
has "a person cannot use the broker command" "error bad request" "$r"
check "broker grants the key" "ok" "$(agent_admin grant alice personal "$ID" keys)"
out=$(run keys 'ssh-add -L 2>&1')
has "the granted workspace lists the key" "ssh-ed25519 $(echo "$PUB" | cut -d' ' -f2)" "$out"
out=$(run other 'ssh-add -L 2>&1')
has "another workspace lists none" "no identities" "$out"
has "arca-ssh list shows the grant" "granted=keys" "$(SSHA list)"

echo "=== sign"
SIGN='echo "$1" > key.pub; echo hello > msg; rm -f msg.sig; ssh-keygen -Y sign -n file -f key.pub msg 2>&1; test -s msg.sig && cat msg.sig'
out=$(run keys "set -- '$PUB'; $SIGN")
has "ssh-keygen -Y sign through the agent" "BEGIN SSH SIGNATURE" "$out"
V "cp $W/keys/work/msg.sig $W/keys/work/msg /tmp/; echo 'alice $PUB' > /tmp/allowed"
r=$(V "ssh-keygen -Y verify -f /tmp/allowed -I alice -n file -s /tmp/msg.sig < /tmp/msg 2>&1")
has "the signature verifies on the host" "Good" "$r"
out=$(run other "set -- '$PUB'; $SIGN")
hasnt "another workspace can't sign" "BEGIN SSH SIGNATURE" "$out"

echo "=== ssh login (git over SSH uses the same)"
# An sshd of the workspace's own (non-root: it serves only its own user) on
# its loopback, with the public line in authorized_keys; ssh then logs in
# through the agent. Each workspace has its own loopback and its own sshd.
LOGIN='set -- "$1"; mkdir -p $HOME/.ssh; echo "$1" > $HOME/.ssh/authorized_keys; chmod 700 $HOME/.ssh; chmod 600 $HOME/.ssh/authorized_keys
rm -f /tmp/hk /tmp/hk.pub; ssh-keygen -q -t ed25519 -N "" -f /tmp/hk
/usr/sbin/sshd -f /dev/null -h /tmp/hk -p 2222 -o ListenAddress=127.0.0.1 -o PidFile=/tmp/sshd.pid -o StrictModes=no -o UsePAM=no 2>&1
sleep 2; rm -f /tmp/hk /tmp/hk.pub
ssh -p 2222 -o BatchMode=yes -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o ConnectTimeout=10 $(id -un)@127.0.0.1 echo logged-in-through-the-agent 2>&1
kill $(cat /tmp/sshd.pid) 2>/dev/null'
out=$(run keys "set -- '$PUB'; $LOGIN")
has "ssh logs in with the key from Arca" "logged-in-through-the-agent" "$out"
out=$(run other "set -- '$PUB'; $LOGIN")
hasnt "not from another workspace" "logged-in-through-the-agent" "$out"

echo "=== no key material"
# Match the armored key header to avoid false positives from package documentation.
out=$(run keys 'grep -rEl "^-----BEGIN ([A-Z0-9]+ )*PRIVATE KEY-----$" "$HOME" /tmp /root /var /usr/local 2>/dev/null | head -3; ls -A "$HOME/.ssh" 2>/dev/null | grep -v -e known_hosts -e authorized_keys')
check "no private key file in the workspace" "" "$out"
r=$(V "su -m alice -c 'SSH_AUTH_SOCK=$AR/alice.keys/agent.sock ssh-add -L 2>&1'")
hasnt "the socket refuses alice's own uid" "ssh-ed25519" "$r"

echo "=== HTTPS token (git)"
r=$(V "echo ghp_gate_token | su -m alice -c 'cd /tmp; arca-ssh token github.com alice'" 2>&1)
TID=$(echo "$r" | head -1)
check "arca-ssh token: an id" 32 "${#TID}"
has "arca-ssh list shows it" "$TID token github.com alice granted=" "$(SSHA list)"
CRED='printf "protocol=https\nhost=$1\n\n" | git-credential-arca get; echo "helper=$GIT_CONFIG_KEY_0=$GIT_CONFIG_VALUE_0"; command -v git >/dev/null && printf "protocol=https\nhost=$1\n\n" | GIT_TERMINAL_PROMPT=0 git credential fill 2>&1 | sed s/^/git:/'
out=$(run keys "set -- github.com; $CRED")
has "git's helper is arca in the workspace" "helper=credential.helper=arca" "$out"
hasnt "nothing before a grant" "ghp_gate_token" "$out"
has "broker lists token metadata" "$TID token github.com alice granted=" "$(agent_admin list alice personal)"
check "broker grants the token" "ok" "$(agent_admin grant alice personal "$TID" keys)"
out=$(run keys "set -- github.com; $CRED")
has "the granted workspace gets the token" "username=alice
password=ghp_gate_token" "$out"
case "$out" in *git:*) has "git credential fill uses it" "git:password=ghp_gate_token" "$out" ;; *) echo "  (no git in the workspace; git credential fill skipped)" ;; esac
hasnt "not for another host" "ghp_gate_token" "$(run keys 'set -- gitlab.com; '"$CRED")"
hasnt "not in another workspace" "ghp_gate_token" "$(run other "set -- github.com; $CRED")"
has "the hand-over is logged" "gave its github.com token" "$(V "cat $AGENT_LOG")"
V "echo ghp_gate_token2 | su -m alice -c 'cd /tmp; arca-ssh token github.com alice'" >/dev/null 2>&1
has "a replaced token keeps its grant" "password=ghp_gate_token2" "$(run keys "set -- github.com; $CRED")"
TID2=$(SSHA list | awk '/ token github.com /{print $1}')
check "broker revokes the token" "ok" "$(agent_admin revoke alice personal "$TID2" keys)"
hasnt "revoked: none" "ghp_gate_token" "$(run keys "set -- github.com; $CRED")"

echo "=== revoke, lock, delete"
check "broker revokes the key" "ok" "$(agent_admin revoke alice personal "$ID" keys)"
has "after revoke: none" "no identities" "$(run keys 'ssh-add -L 2>&1')"
SSHA grant $ID keys >/dev/null
A stop other >/dev/null
check "a stopped workspace's socket is closed" "" "$(V "ls $AR/alice.other 2>/dev/null")"
V "su -m sg-unlock -c '$B/arca-probe $S lock alice'" >/dev/null 2>&1
has "a locked store lists nothing" "no identities" "$(run keys 'ssh-add -L 2>&1')"
n=$(V "grep -c 'signed with' $AGENT_LOG")
[ "${n:-0}" -ge 2 ] && ok "each signature is logged ($n)" || bad "agent log: $n"

for w in keys other; do A stop $w >/dev/null; done
echo "ssh-agent: $fails failed"
[ $fails = 0 ]
