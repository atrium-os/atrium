#!/bin/sh
# Dev-tools M0 (docs/spec/dev-tools-plan.md §2): the system call classes a
# workspace and a console need, checked with the filter DENYING (mode 2) on
# the atrium-os kernel (sysfil). Runs on the HOST after
#   ARCH=amd64 sh scripts/cloud-vm.sh up      (booted on kernel.atrium)
#   sh scripts/vm-sysfil-dev-check.sh
# Ends "sysfil-dev: N failed". No frescod: workspaces through the person's
# CLI (`portcullis workspace exec`, no press); the console classes checked in
# the kernel directly. The on-screen console gates (vm-console-check.sh,
# vm-terminal-check.sh) run with the same policy afterwards.
#
#   - the kernel has sysfil; policy mode 2;
#   - kernel, class by class (plain jails made here with the resolved
#     profiles): `app` and `networked` can't run a program from a shell
#     (fork), `terminal` can but can't ptrace (lldb fails), `develop` can
#     both;
#   - a workspace (as alice) is class develop with
#     allow.unprivileged_proc_debug; cc, make and ./a.out work in it; lldb
#     stops at a breakpoint in a test binary; the host's
#     security.bsd.unprivileged_proc_debug stays 0;
#   - an app manifest naming `develop` is refused at launch.
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
POL=$ROOT/etc/jaild.policy.toml

echo "=== cross-build"
(cd "$ROOT/portcullis" && RUSTFLAGS="$CROSS_RUSTFLAGS" cargo build -q --release --target "$TARGET" \
    -p portcullisd -p portcullis-cli -p opifex --bin portcullisd --bin portcullis --bin opifex) \
    || { echo "build failed"; echo "sysfil-dev: 1 failed"; exit 1; }

echo "=== install"
B=/var/tmp/sysfil-dev
R=/var/tmp/sysfil-dev-root   # ro nullfs of /: never under $B, never rm'd
V "pkill -x portcullisd; sleep 1; umount $R 2>/dev/null; mount -p | grep -q ' $R ' && { echo $R still mounted; exit 1; }; rm -rf $B; mkdir -p $B $R; chmod 755 $B" || { echo "sysfil-dev: 1 failed"; exit 1; }
for b in portcullisd portcullis opifex; do
    scp -q $K -P 2222 "$ROOT/portcullis/target/$TARGET/release/$b" root@localhost:$B/ || { echo "copy $b failed"; echo "sysfil-dev: 1 failed"; exit 1; }
done
scp -q $K -P 2222 "$POL" root@localhost:$B/jaild.policy.toml
PSOCK=/atrium/sockets/portcullis/portcullis.sock
LOG=$B/portcullisd.log
V 'sh -s' <<EOS
set -e
install -m 0755 $B/portcullisd /usr/local/bin/portcullisd
install -m 0755 $B/portcullis /usr/local/bin/portcullis
install -m 0755 $B/opifex /usr/local/bin/opifex
mkdir -p /etc/atrium
# This tree's policy, the filter DENYING, rctl kept on (cloud-vm notes).
sed -e 's/^mode     = 1/mode     = 2/' $B/jaild.policy.toml > /etc/atrium/jaild.policy.toml
grep -q '^allow_rctl' /etc/atrium/jaild.policy.toml || printf '\n[resource_control]\nallow_rctl = true\n' >> /etc/atrium/jaild.policy.toml
id alice >/dev/null 2>&1 || pw useradd alice -m -s /bin/sh
# As etc/sysctl.atrium.conf has it (the published image is stock FreeBSD, 1).
sysctl security.bsd.unprivileged_proc_debug=0 >/dev/null
# A test program on the root file system (/var/tmp is its own dataset, not
# in the nullfs of / the class jails below run on).
mkdir -p /usr/local/share/sysfil-dev
printf 'int main(void){return 42;}\n' > $B/t.c && cc -g -o /usr/local/share/sysfil-dev/t $B/t.c
service atrium-jaild status >/dev/null 2>&1 || service atrium-jaild start >/dev/null 2>&1 || true
mkdir -p /atrium/sockets/portcullis; rm -f $PSOCK
: > $LOG
daemon -f -o $LOG /usr/local/bin/portcullisd --socket $PSOCK
sleep 3; test -S $PSOCK; chmod 666 $PSOCK
EOS
[ $? = 0 ] && ok "portcullisd up with the policy in deny mode" || { bad "setup: $(V "tail -3 $LOG")"; echo "sysfil-dev: $fails failed"; exit 1; }

echo "=== the kernel"
check "sysfil is in this kernel" "yes" "$(V "sysctl -n security.jail.param.sysfil.mode >/dev/null 2>&1 && echo yes")"
check "the host keeps unprivileged_proc_debug=0" "0" "$(V "sysctl -n security.bsd.unprivileged_proc_debug")"

# A plain jail per class on / (read-only nullfs of the host root), the
# resolved profile, mode 2, as uid 65534 (nobody): what the class allows.
V "mount -p | grep -q ' $R ' || mount -t nullfs -o ro / $R"
run_in() { # class command -> exit status
    P=$(python3 "$ROOT/scripts/sysfil/resolve-profile.py" "$POL" "$1" 2)
    V "jail -c name=sysfil_dev_$1 path=$R persist $P devfs_ruleset=4 mount.devfs allow.unprivileged_proc_debug=1 >/dev/null 2>&1; \
       jexec -U nobody sysfil_dev_$1 /bin/sh -c '$2' >/dev/null 2>&1; echo \$?; jail -r sysfil_dev_$1 >/dev/null 2>&1; umount $R/dev 2>/dev/null"
}
# Two commands: sh -c execs a lone one in place, without a fork.
for c in app networked; do
    r=$(run_in $c '/bin/echo x; /bin/echo y')
    [ "$r" != 0 ] && ok "class $c: a shell can't start a program" || bad "class $c ran a program"
done
check "class terminal: a shell runs a program" "0" "$(run_in terminal '/bin/echo x; /bin/echo y')"
LLDB="cd /; lldb -b -o \"b main\" -o run -o bt -o kill /usr/local/share/sysfil-dev/t 2>&1 | grep -q \"stop reason = breakpoint\""
r=$(run_in terminal "$LLDB")
[ "$r" != 0 ] && ok "class terminal: lldb can't trace" || bad "class terminal traced a process"
check "class develop: lldb stops at a breakpoint" "0" "$(run_in develop "$LLDB")"

echo "=== a workspace, as alice"
wx() { V "su -m alice -c 'cd /tmp; portcullis workspace exec scratch -- /bin/sh -c \"$1\"'" 2>&1; }
V "su -m alice -c 'portcullis workspace stop scratch'" >/dev/null 2>&1
L=$(V "su -m alice -c 'portcullis workspace list'" 2>&1)
case "$L" in scratch*) ok "Scratch is there" ;; *) bad "list: $L"; echo "sysfil-dev: $fails failed"; exit 1 ;; esac
out=$(wx 'echo ws-ok')
case "$out" in ws-ok*) ok "a command runs in Scratch" ;; *) bad "exec: $out" ;; esac
J=ws_alice_scratch
check "Scratch's jail allows its own debugging" "true" "$(V "jls -j $J allow.unprivileged_proc_debug")"
V "jls -j $J sysfil.syscalls | tr , '\n' | grep -qx ptrace" && ok "Scratch's filter has ptrace (class develop)" || bad "no ptrace in Scratch's filter"
check "Scratch's filter denies" "2" "$(V "jls -j $J sysfil.mode")"
out=$(wx 'cd ~/work && printf \"int main(void){return 42;}\\n\" > t.c && cc -g -o t t.c && ./t; echo rc=\$?')
case "$out" in *rc=42*) ok "cc and ./t work under deny" ;; *) bad "cc: $out" ;; esac
out=$(wx 'cd ~/work && printf \"all: t2\\nt2: t.c\\n\\tcc -o t2 t.c\\n\" > Makefile && make >/dev/null && ./t2; echo rc=\$?')
case "$out" in *rc=42*) ok "make works under deny" ;; *) bad "make: $out" ;; esac
out=$(wx 'cd ~/work && lldb -b -o \"b main\" -o run -o bt -o kill ./t 2>&1 | grep -q \"stop reason = breakpoint\" && echo lldb-stopped')
case "$out" in *lldb-stopped*) ok "lldb stops at a breakpoint in Scratch" ;; *) bad "lldb: $out" ;; esac

echo "=== an app can't ask for develop"
V 'sh -s' <<EOS >/dev/null 2>&1
test -f /root/of-key.pem || openssl ecparam -name prime256v1 -genkey -noout -out /root/of-key.pem
mkdir -p /etc/atrium/publishers; openssl ec -in /root/of-key.pem -pubout -out /etc/atrium/publishers/of-gate.pem 2>/dev/null
O=$B/b-dev; rm -rf \$O; mkdir -p \$O/bin; cp /usr/bin/true \$O/bin/true
printf '[app]\nid = "org.test.devclass"\nname = "Dev class"\nversion = "1"\nentry = "bin/true"\n\n[capabilities]\nsyscalls = "develop"\n' > \$O/atrium.toml
openssl dgst -sha256 -sign /root/of-key.pem -out \$O/atrium.toml.sig \$O/atrium.toml
opifex uninstall org.test.devclass; opifex install \$O
EOS
out=$(V "opifex install $B/b-dev 2>&1; opifex list 2>/dev/null | grep -c org.test.devclass")
case "$out" in *"workspaces only"*0) ok "refused at install: $(echo "$out" | head -1 | cut -c1-100)" ;; *) bad "an app with class develop: $out" ;; esac
V "opifex uninstall org.test.devclass >/dev/null 2>&1; su -m alice -c 'portcullis workspace stop scratch' >/dev/null 2>&1; umount $R 2>/dev/null"

echo "sysfil-dev: $fails failed"
[ $fails = 0 ]
