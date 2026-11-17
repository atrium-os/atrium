#!/bin/sh
# Dev-tools M2 (docs/spec/dev-tools-plan.md §4): `pkg` in a workspace, as the
# workspace jail's root, never the host's. Runs on the HOST after
#   ARCH=amd64 sh scripts/cloud-vm.sh up      (booted on kernel.atrium)
#   sh scripts/vm-workspace-pkg-check.sh
# Ends "workspace-pkg: N failed". MODE=1 runs the filter in log mode and
# prints what pkg was denied (for profile work); the default is 2, denying.
#
#   - `portcullis workspace pkg scratch install -y PKG` (the person's CLI,
#     no press) bootstraps pkg and installs into Scratch alone: Scratch
#     runs it, a second workspace doesn't have it, the host's /var/db/pkg
#     is untouched;
#   - the jail can't mount, use raw sockets or set file flags;
#   - a package's root can't write the base (/usr/bin is read-only);
#   - subcommands other than the allowed ones, and words that aren't
#     package names, are refused; an app without a press is refused;
#   - the wrapper in the workspace refuses without a terminal.
# The proxy CA of a cloud session (CA=..., default /root/.ccr/ca-bundle.crt
# when it exists) is added to the VM's trust store for the run, and taken
# out at the end, so pkg.FreeBSD.org's certificate checks there.
set -u
ROOT=$(cd "$(dirname "$0")/.." && pwd)
export ARCH=amd64
set -- env
. "$ROOT/scripts/cloud-vm.sh" >/dev/null
set --
K="-i $HOME/.ssh/fresco_bsd_ed25519 -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o LogLevel=ERROR"
V() { "$ROOT/scripts/vssh" "$@"; }
fails=0
ok() { echo "  ok $1"; }
bad() { echo "  FAIL $1"; fails=$((fails+1)); }
check() { if [ "$2" = "$3" ]; then ok "$1"; else bad "$1: expected [$2] got [$3]"; fi; }
MODE=${MODE:-2}
PKG=${PKG:-tree}
# The program the package installs (helix's is hx).
case "$PKG" in helix) BIN=${BIN:-hx} ;; *) BIN=${BIN:-$PKG} ;; esac
CA=${CA:-/root/.ccr/ca-bundle.crt}

echo "=== cross-build"
(cd "$ROOT/portcullis" && RUSTFLAGS="$CROSS_RUSTFLAGS" cargo build -q --release --target "$TARGET" \
    -p portcullisd -p portcullis-cli --bin portcullisd --bin portcullis) \
    || { echo "build failed"; echo "workspace-pkg: 1 failed"; exit 1; }

echo "=== install"
B=/var/tmp/workspace-pkg
V "pkill -x portcullisd; sleep 1; rm -rf $B; mkdir -p $B; chmod 755 $B"
for b in portcullisd portcullis; do
    scp -q $K -P 2222 "$ROOT/portcullis/target/$TARGET/release/$b" root@localhost:$B/ || { echo "copy $b failed"; echo "workspace-pkg: 1 failed"; exit 1; }
done
scp -q $K -P 2222 "$ROOT/etc/jaild.policy.toml" root@localhost:$B/jaild.policy.toml
[ -f "$CA" ] && scp -q $K -P 2222 "$CA" root@localhost:$B/proxy-ca.pem
PSOCK=/atrium/sockets/portcullis/portcullis.sock
LOG=$B/portcullisd.log
V 'sh -s' <<EOS
set -e
install -m 0755 $B/portcullisd /usr/local/bin/portcullisd
install -m 0755 $B/portcullis /usr/local/bin/portcullis
mkdir -p /etc/atrium
sed -e 's/^mode     = 1/mode     = $MODE/' $B/jaild.policy.toml > /etc/atrium/jaild.policy.toml
grep -q '^allow_rctl' /etc/atrium/jaild.policy.toml || printf '\n[resource_control]\nallow_rctl = true\n' >> /etc/atrium/jaild.policy.toml
if [ -f $B/proxy-ca.pem ]; then
    [ -f /etc/ssl/cert.pem.pkg-bak ] || cp /etc/ssl/cert.pem /etc/ssl/cert.pem.pkg-bak
    cat /etc/ssl/cert.pem.pkg-bak $B/proxy-ca.pem > /etc/ssl/cert.pem
fi
id alice >/dev/null 2>&1 || pw useradd alice -m -s /bin/sh
service atrium-jaild status >/dev/null 2>&1 || service atrium-jaild start >/dev/null 2>&1 || true
mkdir -p /atrium/sockets/portcullis; rm -f $PSOCK
: > $LOG
daemon -f -o $LOG /usr/local/bin/portcullisd --socket $PSOCK
sleep 3; test -S $PSOCK; chmod 666 $PSOCK
EOS
[ $? = 0 ] && ok "portcullisd up (filter mode $MODE)" || { bad "setup: $(V "tail -3 $LOG")"; echo "workspace-pkg: $fails failed"; exit 1; }

A() { V "su -m alice -c 'cd /tmp; portcullis workspace $1'" 2>&1; }
wx() { A "exec $1 -- /bin/sh -c \"$2\""; }
J=ws_alice_scratch
H_BEFORE=$(V "ls /var/db/pkg 2>/dev/null | sort | md5")
for w in scratch other; do A "stop $w" >/dev/null; done
A "new other --network none" >/dev/null
case "$(A list)" in *scratch*other*|*other*scratch*) ok "Scratch and Other are there" ;; *) bad "list: $(A list)" ;; esac

echo "=== the jail"
out=$(wx scratch 'echo started')
case "$out" in started*) ok "Scratch is up" ;; *) bad "exec: $out" ;; esac
for p in allow.mount allow.raw_sockets allow.chflags; do
    check "Scratch: $p is off" "false" "$(V "jls -j $J $p")"
done
check "Scratch: enforce_statfs=2" "2" "$(V "jls -j $J enforce_statfs")"
check "Scratch: no child jails" "0" "$(V "jls -j $J children.max")"
check "Scratch: the filter's mode" "$MODE" "$(V "jls -j $J sysfil.mode")"
out=$(wx scratch 'command -v pkg')
check "the wrapper is first on PATH" "/atrium/bin/pkg" "$out"
out=$(wx scratch 'pkg install foo </dev/null; echo rc=\$?')
case "$out" in *"needs your OK"*rc=1*) ok "the wrapper wants a terminal" ;; *) bad "wrapper: $out" ;; esac

echo "=== refusals"
case "$(A 'pkg scratch shell')" in *"isn't allowed"*) ok "pkg shell refused" ;; *) bad "pkg shell ran" ;; esac
case "$(A 'pkg scratch install a%b')" in *"isn't a package name"*) ok "a word that isn't a package name is refused" ;; *) bad "a%b: ran" ;; esac
case "$(A 'pkg scratch -r / install a')" in *"isn't allowed"*) ok "pkg -r refused" ;; *) bad "pkg -r: ran" ;; esac
case "$(V "su -m alice -c 'portcullis workspace pkg nosuch info'" 2>&1)" in *"no workspace"*|*"denied"*|*"not"*) ok "an unknown workspace is refused" ;; *) bad "nosuch accepted" ;; esac

echo "=== install $PKG into Scratch (bootstraps pkg)"
out=$(A "pkg scratch install -y $PKG")
echo "$out" | tail -4 | sed 's/^/    /'
case "$out" in *"pkg: exit 0"*) ok "pkg install $PKG: exit 0" ;; *) bad "pkg install $PKG failed" ;; esac
out=$(wx scratch "$BIN --version 2>&1; echo rc=\\\$?")
case "$out" in *rc=0*) ok "$BIN runs in Scratch: $(echo "$out" | head -1 | cut -c1-60)" ;; *) bad "$PKG in Scratch: $out" ;; esac
out=$(wx scratch "pkg info | grep -c '^$PKG-'")
check "pkg info (as the workspace's uid) lists it" "1" "$out"
out=$(wx other "command -v $BIN || echo none")
check "Other doesn't have it" "none" "$out"
check "the host's /var/db/pkg is untouched" "$H_BEFORE" "$(V "ls /var/db/pkg 2>/dev/null | sort | md5")"
check "the host has no $BIN" "none" "$(V "command -v $BIN || echo none")"
check "files pkg wrote are the jail root's" "0" "$(V "stat -f %u \$(jls -j $J path)/usr/local/bin/$BIN")"

echo "=== the jail's root can't write the base"
# As pkg's root would (pkg's own scripts run so): the base is read-only.
out=$(V "jexec -U root $J /bin/sh -c 'touch /usr/bin/x 2>&1; touch /bin/x 2>&1; mount -t tmpfs tmpfs /tmp 2>&1; chflags schg /root 2>&1; echo done'")
case "$out" in *"Read-only"*"Read-only"*mount*[Pp]ermi*|*"Read-only"*"Read-only"*mount*"not implemented"*) ;; *) out="no: $out" ;; esac
case "$out" in no:*) ;; *) case "$out" in *chflags*"not permitted"*done) ;; *) out="no: $out" ;; esac ;; esac
case "$out" in no:*) bad "jail root: $(echo "$out" | tr '\n' ' ')" ;; *) ok "jail root: base read-only, no mount, no chflags" ;; esac

echo "=== delete"
out=$(A "pkg scratch delete -y $PKG")
case "$out" in *"pkg: exit 0"*) ok "pkg delete: exit 0" ;; *) bad "pkg delete: $(echo "$out" | tail -2)" ;; esac
check "gone from Scratch" "none" "$(wx scratch "command -v $BIN || echo none")"

if [ "$MODE" = 1 ]; then
    echo "=== what the filter would deny (log mode)"
    for p in denied seen.syscalls seen.ioctls seen.sockopts seen.families; do echo "  $p: $(V "jls -j $J sysfil.$p")"; done
else
    # Expected: ifconfig's module load at network setup, the mount tried above.
    check "pkg was denied nothing" "" "$(V "jls -j $J sysfil.seen.syscalls sysfil.seen.ioctls sysfil.seen.sockopts sysfil.seen.families" | tr ' ,' '\n\n' | grep -v '^kldload$' | grep -v '^nmount$' | grep -v '^\"\"$' | tr -d '\n')"
fi

for w in scratch other; do A "stop $w" >/dev/null; done
V "[ -f /etc/ssl/cert.pem.pkg-bak ] && cp /etc/ssl/cert.pem.pkg-bak /etc/ssl/cert.pem; true"
echo "workspace-pkg: $fails failed"
[ $fails = 0 ]
