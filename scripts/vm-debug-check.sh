#!/bin/sh
# Dev-tools M10 (docs/spec/dev-tools-plan.md, dev-tools.md §7.2): Run under
# the debugger, and cores. Runs on the HOST after
#   ARCH=amd64 sh scripts/cloud-vm.sh up
#   sh scripts/vm-debug-check.sh
# Ends "debug: N failed". The filter denies (mode 2). No frescod: the dev
# apps are C programs that print to portcullisd's log.
#
#   - `portcullis workspace debug scratch dbg` installs the dev app and
#     answers an address on Scratch's own loopback; the app hasn't started;
#   - a dev build runs from its bundle's own path, and Scratch sees its own
#     dev bundles at that path (the kernel names what a jailed process maps
#     by the host's path; lldb wants the program where the process was
#     started), so lldb finds them;
#   - lldb in Scratch connects (gdb-remote), a breakpoint on a function
#     stops there with its argument visible, and continuing runs the app
#     to its end; it ran in its own jail, as its own uid, under lldb-server;
#   - the address is gone once used, and isn't reachable from the host;
#   - an installed app can't be debugged: lldb in Scratch can't attach to
#     it, nor see it;
#   - a dev app that crashes leaves a core in its overlay;
#     `portcullis workspace cores scratch` moves it into
#     work/.atrium/cores, the workspace's, and lldb there reads its stack.
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

echo "=== cross-build"
(cd "$ROOT/portcullis" && RUSTFLAGS="$CROSS_RUSTFLAGS" cargo build -q --release --target "$TARGET" \
    -p portcullisd -p portcullis-cli -p opifex --bin portcullisd --bin portcullis --bin opifex) \
    || { echo "build failed"; echo "debug: 1 failed"; exit 1; }

echo "=== install"
B=/var/tmp/debugchk
V "pkill -x portcullisd; sleep 1; rm -rf $B; mkdir -p $B; chmod 755 $B"
for b in portcullis/target/$TARGET/release/portcullisd portcullis/target/$TARGET/release/portcullis \
         portcullis/target/$TARGET/release/opifex etc/jaild.policy.toml; do
    scp -q $K -P 2222 "$ROOT/$b" root@localhost:$B/ || { echo "copy $b failed"; echo "debug: 1 failed"; exit 1; }
done
PSOCK=/atrium/sockets/portcullis/portcullis.sock
LOG=$B/portcullisd.log
SLEEPER=org.test.sleeper
V 'sh -s' <<EOS
set -e
for b in portcullisd portcullis opifex; do install -m 0755 $B/\$b /usr/local/bin/\$b; done
mkdir -p /etc/atrium
sed -e 's/^mode     = 1/mode     = $MODE/' $B/jaild.policy.toml > /etc/atrium/jaild.policy.toml
grep -q '^allow_rctl' /etc/atrium/jaild.policy.toml || printf '\n[resource_control]\nallow_rctl = true\n' >> /etc/atrium/jaild.policy.toml
id alice >/dev/null 2>&1 || pw useradd alice -m -s /bin/sh
service atrium-jaild restart >/dev/null 2>&1 || service atrium-jaild start >/dev/null 2>&1 || true
# An installed app that stays a while: the one lldb must not reach.
test -f /root/dbg-key.pem || openssl ecparam -name prime256v1 -genkey -noout -out /root/dbg-key.pem
mkdir -p /etc/atrium/publishers; openssl ec -in /root/dbg-key.pem -pubout -out /etc/atrium/publishers/dbg-gate.pem 2>/dev/null
O=$B/b-sleeper; rm -rf \$O; mkdir -p \$O/bin
printf '#include <unistd.h>\n#include <stdio.h>\nint main(void){puts("sleeper up");fflush(stdout);sleep(120);return 0;}\n' > $B/sleeper.c
cc -o \$O/bin/sleeper $B/sleeper.c
printf '[app]\nid = "$SLEEPER"\nname = "Sleeper"\nversion = "1"\nentry = "bin/sleeper"\n' > \$O/atrium.toml
openssl dgst -sha256 -sign /root/dbg-key.pem -out \$O/atrium.toml.sig \$O/atrium.toml
opifex uninstall $SLEEPER >/dev/null 2>&1 || true
opifex install \$O >/dev/null
mkdir -p /atrium/sockets/portcullis; rm -f $PSOCK
: > $LOG
daemon -f -o $LOG /usr/local/bin/portcullisd --socket $PSOCK
sleep 3; test -S $PSOCK; chmod 666 $PSOCK
EOS
[ $? = 0 ] && ok "portcullisd up (filter mode $MODE), lldb-server in the base: $(V 'test -x /usr/bin/lldb-server && echo yes')" || { bad "setup: $(V "tail -3 $LOG")"; echo "debug: $fails failed"; exit 1; }

A() { V "su -m alice -c 'cd /tmp; portcullis workspace $1'" 2>&1; }
W=/home/alice/.atrium/workspaces
ws_sh() { V "cat > $W/$1/work/.step.sh && chown alice:files-alice $W/$1/work/.step.sh && chmod 664 $W/$1/work/.step.sh" && \
          V "su -m alice -c 'cd /tmp; portcullis workspace exec $1 -- /bin/sh .step.sh'" 2>&1; }
logline() { # marker -> its line in the log, waiting up to 30 s
    i=0; while [ $i -lt 30 ] && ! V "grep -q '$1' $LOG"; do sleep 2; i=$((i+2)); done
    V "grep '$1' $LOG | tail -1"
}
A "stop scratch" >/dev/null
A list >/dev/null
J=ws_alice_scratch

echo "=== two dev apps built in Scratch"
out=$(ws_sh scratch <<'SH'
set -e
mkdir -p ~/work/dbg ~/work/crash
cd ~/work/dbg
cat > dbg.c <<'C'
#include <stdio.h>
int __attribute__((noinline)) marker(int answer) { return answer + 1; }
int main(void) {
    printf("debug-before\n"); fflush(stdout);
    int r = marker(41);
    printf("debug-after %d\n", r);
    return 0;
}
C
cc -g -O0 -o dbg dbg.c
printf '[app]\nid = "org.example.dbg"\nname = "Dbg"\nversion = "1"\nentry = "dbg"\n' > atrium.toml
cd ~/work/crash
cat > crash.c <<'C'
#include <stdio.h>
void __attribute__((noinline)) boom(int *p) { *p = 42; }
int main(void) { printf("crash-before\n"); fflush(stdout); boom(0); return 0; }
C
cc -g -O0 -o crash crash.c
printf '[app]\nid = "org.example.crash"\nname = "Crash"\nversion = "1"\nentry = "crash"\n' > atrium.toml
echo built
SH
)
case "$out" in *built*) ok "dbg and crash built" ;; *) bad "build: $out" ;; esac

echo "=== Debug"
out=$(A "debug scratch dbg")
ID=$(echo "$out" | awk -F'\t' '/^dev\./ {print $1}')
ADDR=$(echo "$out" | awk -F'\t' '/^dev\./ {print $2}')
PROG=$(echo "$out" | awk -F'\t' '/^dev\./ {print $3}')
case "$ADDR $PROG" in "127.0.0.1:"[0-9]*" /var/lib/atrium/apps/$ID/dbg") ok "debug answered $ID at $ADDR, program $PROG" ;; *) bad "debug: $out" ;; esac
check "the app hasn't started" "" "$(V "grep debug-before $LOG")"
check "Scratch sees the bundle where the host has it" "1" "$(V "jexec $J test -x /var/lib/atrium/apps/$ID/dbg && echo 1")"
check "and only its own bundles" "" "$(V "jexec $J ls /var/lib/atrium/apps" | grep -v '^dev\.alice\.scratch\.')"
check "not reachable from the host" "refused" "$(V "nc -z -w 2 127.0.0.1 ${ADDR#*:} && echo open || echo refused")"
out=$(ws_sh scratch <<SH
cd ~/work/dbg
lldb -b -o 'target create $PROG' -o 'gdb-remote $ADDR' -o 'breakpoint set -n marker' -o 'continue' \
     -o 'frame variable answer' -o 'bt 2' -o 'continue' 2>&1
SH
)
echo "$out" | grep -E "stop reason|answer|frame #|exited" | head -8 | sed 's/^/    /'
case "$out" in *"stop reason = breakpoint"*) ok "the breakpoint stopped it" ;; *) bad "no breakpoint: $(echo "$out" | tail -12 | tr '\n' ' ')" ;; esac
case "$out" in *"(int) answer = 41"*) ok "marker's argument is 41" ;; *) bad "frame variable: not 41" ;; esac
case "$out" in *"marker"*"main"*) ok "the stack is marker, main" ;; *) bad "bt" ;; esac
case "$out" in *"exited with status = 0"*) ok "it ran to its end" ;; *) bad "no exit" ;; esac
# lldb-server forwards the program's output to the debugger.
case "$out" in *"debug-after 42"*) ok "its output reached lldb: debug-after 42" ;; *) bad "no debug-after in lldb's output" ;; esac
case "$(V "grep -E '$ID (\\(debug\\) )?exited|starts under lldb-server' $LOG | head -2 | tr '\n' ' '")" in
    *"starts under lldb-server"*) ok "it ran under lldb-server, in its own jail" ;; *) bad "log: $(V "grep -i debug $LOG | tail -5")" ;; esac
case "$(V "grep 'launching $ID as uid' $LOG | tail -1")" in *"as uid 5"*) ok "as its own uid: $(V "grep 'launching $ID as uid' $LOG | tail -1 | sed 's/.*as uid \([0-9]*\).*/\1/'")" ;; *) bad "uid line" ;; esac
check "the address is gone once used" "refused" "$(V "jexec $J nc -z -w 2 127.0.0.1 ${ADDR#*:} && echo open || echo refused")"

echo "=== an installed app can't be debugged"
V "su -m alice -c 'portcullis launch --no-prompt $SLEEPER' >/dev/null 2>&1 &" >/dev/null
L=$(logline 'sleeper up')
PID=$(V "pgrep -f bin/sleeper | head -1")
[ -n "$PID" ] && ok "the installed app runs (pid $PID)" || bad "no sleeper: $(V "tail -3 $LOG")"
check "Scratch can't see it" "0" "$(V "jexec $J ps -ax -o pid= | grep -c '^ *$PID\$'")"
out=$(ws_sh scratch <<SH
lldb -b -p $PID 2>&1 | tail -3
SH
)
case "$out" in *"error"*|*"failed"*|*"not"*) ok "lldb -p from Scratch is refused: $(echo "$out" | grep -iE 'error|fail' | head -1 | cut -c1-90)" ;; *) bad "attach: $out" ;; esac
V "pkill -f bin/sleeper" >/dev/null

echo "=== cores"
# ★ FreeBSD 16-CURRENT on ZFS fails every core with EFAULT ("no core dump -
# bad address", GENERIC too, 2026-11-18); tmpfs writes them. The VM's
# overlays are plain folders on ZFS (a real install's are Tessera volumes),
# so the crashing app's overlay is a tmpfs for the gate.
OV=/home/alice/.atrium/apps/dev.alice.scratch.org.example.crash
V "umount $OV 2>/dev/null; rm -rf $OV; mkdir -p $OV && mount -t tmpfs tmpfs $OV"
out=$(A "run scratch crash")
case "$out" in dev.alice.scratch.org.example.crash*) ok "Run of crash" ;; *) bad "run: $out" ;; esac
L=$(logline 'crash-before')
sleep 4
out=$(A "cores scratch")
CORE=$(echo "$out" | grep '\.core$' | head -1)
case "$CORE" in .atrium/cores/dev.alice.scratch.org.example.crash.*.core) ok "cores moved it: $CORE" ;; *) bad "cores: $out | $(V "grep -i 'core\|crash' $LOG | tail -4")" ;; esac
check "it is the workspace's" "$(V "stat -f %u $W/scratch/work")" "$(V "stat -f %u $W/scratch/work/$CORE 2>/dev/null")"
check "none left in the overlay" "" "$(V "find /home/alice/.atrium/apps/dev.alice.scratch.org.example.crash -name '*.core' 2>/dev/null")"
out=$(ws_sh scratch <<SH
cd ~/work/crash
lldb -b -c ../$CORE /var/lib/atrium/apps/dev.alice.scratch.org.example.crash/crash -o 'bt 2' 2>&1
SH
)
case "$out" in *boom*main*) ok "lldb reads the core: boom, main" ;; *) bad "lldb -c: $(echo "$out" | tail -6 | tr '\n' ' ')" ;; esac
check "a second cores finds none" "" "$(A "cores scratch" | grep '\.core$')"

if [ "$MODE" = 2 ]; then
    check "Scratch was denied nothing" "" "$(V "jls -j $J sysfil.seen.syscalls sysfil.seen.ioctls sysfil.seen.sockopts sysfil.seen.families" | tr ' ,' '\n\n' | grep -v '^kldload$' | grep -v '^\"\"$' | tr -d '\n')"
fi
V "umount $OV 2>/dev/null"
A "stop scratch" >/dev/null
echo "debug: $fails failed"
[ $fails = 0 ]
