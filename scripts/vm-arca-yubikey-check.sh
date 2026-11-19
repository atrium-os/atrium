#!/bin/sh
# Optional manual hardware acceptance check. Routine tests do not invoke it.
# Dedicated disposable FreeBSD amd64 VM, forwarded YubiKey, host cross sysroot.
# Export VMDIR, SSH_AGENT_GATE_PORT and SSH_AGENT_GATE_KEY as for vm-ssh-agent-check.sh.
# sh scripts/vm-arca-yubikey-check.sh prepare    # once on a fresh guest
# sh scripts/vm-arca-yubikey-check.sh run        # from an interactive terminal
# Prepare replaces test services and sets alice's fixture passcode. Run reuses
# her enrollment on subsequent invocations. Type the hardware PIN only at the UI.
set -eu
ROOT=$(cd "$(dirname "$0")/.." && pwd)
: "${SSH_AGENT_GATE_PORT:?set the dedicated guest SSH port}" "${SSH_AGENT_GATE_KEY:?set its SSH identity file}"
[ "$SSH_AGENT_GATE_PORT" != 2222 ] || { echo 'Select the dedicated guest port' >&2; exit 1; }
case ${1:-} in
    prepare|run) ;;
    *) echo "Usage: sh $0 prepare|run" >&2; exit 2 ;;
esac
# Share one authenticated connection; only the PIN reader requests a PTY.
session=$(mktemp -d /tmp/arca-ssh.XXXXXXXX)
remote() {
    ssh -S "$session/control" -o UserKnownHostsFile="$session/known_hosts" -i "$SSH_AGENT_GATE_KEY" -p "$SSH_AGENT_GATE_PORT" -o BatchMode=yes \
        -o StrictHostKeyChecking=accept-new -o ConnectTimeout=10 "$@"
}
cleanup() {
    remote -O exit root@localhost >/dev/null 2>&1 || :
    rm -f "$session/known_hosts" "$session/control"
    rmdir "$session"
}
trap cleanup EXIT
remote -MNf root@localhost
B=/var/tmp/arca-physical
[ "$(remote root@localhost 'uname -sm')" = 'FreeBSD amd64' ]
case ${1:-} in
prepare)
    remote root@localhost "test ! -e $B" || { echo "Physical fixture already exists. Use run, or prepare a fresh guest." >&2; exit 1; }
    remote root@localhost 'usbconfig list | grep -i yubikey'
    export ARCH=amd64 SMP=${SMP:-1}
    set -- env
    . "$ROOT/scripts/cloud-vm.sh" >/dev/null
    (cd "$ROOT/sigillum" && RUSTFLAGS="$CROSS_RUSTFLAGS" cargo build --release --target "$TARGET" -p sigillumd -p sigillum-fido --bins --example sigillum-probe)
    (cd "$ROOT/portcullis" && RUSTFLAGS="$CROSS_RUSTFLAGS" cargo build --release --target "$TARGET" -p ostiarius --bin ostiarius)
    sh "$ROOT/scripts/vm-ssh-agent-check.sh"
    remote root@localhost "mkdir -m 755 $B"
    for file in "$ROOT/sigillum/target/$TARGET/release/sigillumd" "$ROOT/sigillum/target/$TARGET/release/sigillum-fido" \
        "$ROOT/sigillum/target/$TARGET/release/examples/sigillum-probe" "$ROOT/portcullis/target/$TARGET/release/ostiarius" \
        "$ROOT/portcullis/ostiarius/etc/services.d/45-atrium-ostiarius.toml" "$ROOT/portcullis/ostiarius/etc/pam.d/atrium-login"; do
        remote root@localhost "cat > $B/${file##*/}" < "$file"
    done
    remote root@localhost "chmod 755 $B/sigillumd $B/sigillum-fido $B/sigillum-probe $B/ostiarius; sh -s" < "$ROOT/scripts/arca-yubikey/prepare.sh"
    ;;
run)
    test -t 0 || { echo 'Run from a terminal for hidden PIN entry' >&2; exit 1; }
    remote root@localhost 'cat > /root/arca-yubikey-check-result.sh' < "$ROOT/scripts/arca-yubikey/check-result.sh"
    remote root@localhost 'cat > /root/arca-yubikey-worker.sh' < "$ROOT/scripts/arca-yubikey/worker.sh"
    state=$(remote root@localhost "set -e; test -S $B/ui.sock; usbconfig list | grep -iq yubikey; echo \$((1 + \$(wc -l < $B/sigillumd.log))) \$(if test -s $B/keys/alice.json; then echo 3; else echo 4; fi)")
    read -r first sheets <<STATE
$state
STATE
    remote -nT root@localhost "sh /root/arca-yubikey-worker.sh $first" &
    worker=$!
    remote -tt root@localhost "SPROBE_PASSCODES=correct-horse exec timeout --foreground 650 $B/sigillum-probe ui-tty $sheets $B/ui.sock"
    wait "$worker"
    ;;
esac
