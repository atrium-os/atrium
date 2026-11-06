#!/bin/sh
# A credential field end to end in the FreeBSD dev VM (arca-autofill.md §9,
# limen.md §12): an app's login window, limend, arca-ui, arcad and a
# headless frescod, each as its own user, and a fill by an injected click.
#
#   sh scripts/cloud-vm.sh up         # VM running, Portcullis stack deployed
#   sh scripts/limen-arca-vm-test.sh  # arcad via arca-vm-test.sh, then this
#
# Users: arcaapp1 (50801, org.example.mail, an account saved) and arcaapp2
# (50802, org.example.evil, same publisher, nothing saved), from
# arca-vm-test.sh; _arca-ui (65021); _limen (65022, made here).
# SKIP_ARCA=1 reuses an arcad already up from an earlier run.
set -eu
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
set -- env
. "$ROOT/scripts/cloud-vm.sh" >/dev/null
set --
KEY="$HOME/.ssh/fresco_bsd_ed25519"
SSHOPT="-i $KEY -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o LogLevel=ERROR"
g() { ssh $SSHOPT -o ConnectTimeout=30 -p 2222 root@localhost "$@"; }
put() { scp $SSHOPT -P 2222 -q "$1" "root@localhost:$2"; }

if [ "${SKIP_ARCA:-0}" != 1 ]; then
    echo "=== arcad (arca-vm-test.sh)"
    SKIP_SUITES=1 sh "$ROOT/scripts/arca-vm-test.sh" | tail -3
fi

echo "=== cross-build"
xb() { (cd "$ROOT/$1" && RUSTFLAGS="$CROSS_RUSTFLAGS" cargo build -q --release --target "$TARGET" $2); }
xb frescod "--bin frescod"
xb limen "--bin limend --example limen-arca-probe"
xb arca/arca-ui "--bin arca-ui"
L="$ROOT/limen/target/$TARGET/release"

echo "=== deploy"
D=/opt/limen-arca
g "pkill -x frescod; pkill -x limend; pkill -x arca-ui; pkill -x limen-arca-probe; sleep 1; rm -rf $D /tmp/lp; mkdir -p $D /tmp/lp; chmod 755 $D; chmod 777 /tmp/lp; true"
for b in "$ROOT/frescod/target/$TARGET/release/frescod" "$L/limend" "$L/examples/limen-arca-probe" "$ROOT/arca/arca-ui/target/$TARGET/release/arca-ui"; do
    put "$b" "$D/"
    # Under TCG a large scp can land with a run of zeros.
    [ "$(sha256sum "$b" | cut -d' ' -f1)" = "$(g "sha256 -q $D/$(basename "$b")")" ] || { echo "ABORT: hash mismatch for $(basename "$b")"; exit 1; }
done
g "pw usershow _limen >/dev/null 2>&1 || pw useradd _limen -u 65022 -d /nonexistent -s /usr/sbin/nologin"
g "mkdir -p /var/run/atrium && chmod 755 /var/run/atrium"

echo "=== start"
# frescod headless, direct (one process), injected presses mint tokens.
g "cd /tmp/lp && umask 0 && env FRESCOD_SPLIT=0 FRESCOD_HEADLESS_PNG=/tmp/lp/screen \
    FRESCOD_SOCK=/tmp/lp/fresco.sock FRESCOD_WM_SOCK=/tmp/lp/wm.sock FRESCOD_INPUT_SOCK=/tmp/lp/inject.sock \
    FRESCOD_INJECT_ACTIVATION=1 FRESCOD_SLOT_UIDS=65022 FRESCOD_ACTIVATION_UIDS=65021 \
    daemon -f -o /tmp/lp/frescod.log $D/frescod </dev/null"
i=0; while [ $i -lt 60 ] && ! g "test -S /tmp/lp/fresco.sock && test -S /var/run/atrium/fresco-activation.sock"; do sleep 2; i=$((i+1)); done
g "chmod 666 /tmp/lp/*.sock /var/run/atrium/fresco-activation.sock"
g "cd /tmp/lp && umask 0 && env LIMEN_SOCKET=/tmp/lp/limend.sock LIMEN_APP_REGISTRY=/var/run/atrium/app-registry \
    LIMEN_ARCA_UID=65021 daemon -f -o /tmp/lp/limend.log -u _limen $D/limend </dev/null"
i=0; while [ $i -lt 30 ] && ! g "test -S /tmp/lp/limend.sock"; do sleep 1; i=$((i+1)); done
g "cd /tmp/lp && env LIMEN_SOCKET=/tmp/lp/limend.sock FRESCO_SOCKET=/tmp/lp/fresco.sock \
    daemon -f -o /tmp/lp/arca-ui.log -u _arca-ui $D/arca-ui </dev/null"
sleep 5
g "ps -axo user,pid,command | grep -E '[f]rescod|[l]imend|[a]rca-ui|[a]trium-arcad'"

status=0
P="$D/limen-arca-probe /tmp/lp/fresco.sock /tmp/lp/limend.sock /tmp/lp/inject.sock"
echo "=== probe as org.example.mail (an account saved)"
g "su -m arcaapp1 -c '$P'" || status=1
g "cp /tmp/lp/screen.png /tmp/lp/after-fill.png 2>/dev/null; true"
echo "=== probe as org.example.evil (nothing saved)"
g "su -m arcaapp2 -c '$P expect-none'" || status=1

echo "=== logs"
for l in limend arca-ui; do echo "--- $l"; g "tail -20 /tmp/lp/$l.log"; done
echo "--- frescod (slot, activation)"
g "grep -iE 'slot|activation|refus|panic' /tmp/lp/frescod.log | tail -20; true"
exit $status
