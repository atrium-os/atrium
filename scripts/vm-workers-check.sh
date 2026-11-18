#!/bin/sh
# Dev-tools M5 (docs/spec/dev-tools-plan.md §6, app-composition.md §4): an
# app's [[workers]]. Runs on the HOST after
#   ARCH=amd64 sh scripts/cloud-vm.sh up      (booted on kernel.atrium)
#   sh scripts/vm-workers-check.sh
# Ends "workers: N failed". The filter denies (mode 2). No frescod.
#
# An app built with the atrium-worker crate (sdk/atrium-worker/examples:
# gate_core and gate_worker) is Run from Scratch with one worker, "echo"
# (cpu = 3s). The core calls it and prints what came back:
#   - the worker runs as a uid of its own, not the core's, the same uid
#     on the next call (per person, app and worker);
#   - a file the core passes is read by the worker;
#   - the worker's error, a crash (exit 3) and a call past 3 s come back as
#     errors, and the core carries on;
#   - past its memory (128M) it is killed, and that is an error too;
#   - the worker has no network and cannot call workers itself;
#   - it sees none of the host's network: no interface but its own lo0, no
#     MAC, no interface MIB or SIOCGIFCONF, only AF_UNIX sockets. On a
#     kernel with jail_netblind its jail has no vnet at all (portcullis.md
#     §9.1c); 20 calls are timed either way;
#   - the core's jail has the worker-call socket, which refuses anything
#     but WorkerCall, and no portcullisd socket;
#   - it tells the machine's local time (a copy of its zone file);
#   - a socket worker gets a descriptor past its three (a pipe, at 3 in
#     its jail) and reads from it;
#   - and after it all no worker jail is left.
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

echo "=== cross-build"
(cd "$ROOT/portcullis" && RUSTFLAGS="$CROSS_RUSTFLAGS" cargo build -q --release --target "$TARGET" \
    -p portcullisd -p portcullis-cli -p jaild --bin portcullisd --bin portcullis --bin atrium-jaild) \
    || { echo "build failed"; echo "workers: 1 failed"; exit 1; }
(cd "$ROOT/sdk" && RUSTFLAGS="$CROSS_RUSTFLAGS" cargo build -q --release --target "$TARGET" \
    -p atrium-worker --example gate_core --example gate_worker --example gate_sock) \
    || { echo "sdk build failed"; echo "workers: 1 failed"; exit 1; }

echo "=== install"
B=/var/tmp/workers-gate
V "pkill -x portcullisd; sleep 1; rm -rf $B; mkdir -p $B; chmod 755 $B"
for f in "$ROOT/portcullis/target/$TARGET/release/portcullisd" "$ROOT/portcullis/target/$TARGET/release/portcullis" \
         "$ROOT/portcullis/target/$TARGET/release/atrium-jaild" \
         "$ROOT/sdk/target/$TARGET/release/examples/gate_core" "$ROOT/sdk/target/$TARGET/release/examples/gate_worker" \
         "$ROOT/sdk/target/$TARGET/release/examples/gate_sock" \
         "$ROOT/etc/jaild.policy.toml"; do
    scp -q $K -P 2222 "$f" root@localhost:$B/ || { echo "copy $f failed"; echo "workers: 1 failed"; exit 1; }
done
PSOCK=/atrium/sockets/portcullis/portcullis.sock
LOG=$B/portcullisd.log
V 'sh -s' <<EOS
set -e
install -m 0755 $B/portcullisd /usr/local/bin/portcullisd
install -m 0755 $B/portcullis /usr/local/bin/portcullis
install -m 0755 $B/atrium-jaild /usr/local/bin/atrium-jaild
mkdir -p /etc/atrium
sed -e 's/^mode     = 1/mode     = 2/' $B/jaild.policy.toml > /etc/atrium/jaild.policy.toml
grep -q '^allow_rctl' /etc/atrium/jaild.policy.toml || printf '\n[resource_control]\nallow_rctl = true\n' >> /etc/atrium/jaild.policy.toml
id alice >/dev/null 2>&1 || pw useradd alice -m -s /bin/sh
service atrium-jaild restart >/dev/null 2>&1 || service atrium-jaild start >/dev/null 2>&1 || true
mkdir -p /atrium/sockets/portcullis; rm -f $PSOCK
: > $LOG
daemon -f -o $LOG env ATRIUM_ONESHOT_TIMING=1 /usr/local/bin/portcullisd --socket $PSOCK
sleep 3; test -S $PSOCK; chmod 666 $PSOCK
test -S /atrium/sockets/workers/workers.sock
EOS
[ $? = 0 ] && ok "portcullisd up, with the worker-call socket" || { bad "setup: $(V "tail -3 $LOG")"; echo "workers: $fails failed"; exit 1; }

A() { V "su -m alice -c 'cd /tmp; portcullis workspace $1'" 2>&1; }
W=/home/alice/.atrium/workspaces
A "stop scratch" >/dev/null
A "new scratch --network none" >/dev/null 2>&1
A "list" | grep -q scratch && ok "Scratch is there" || bad "no Scratch: $(A list)"
P=$W/scratch/work/wk
V 'sh -s' <<EOS
set -e
rm -rf $P; mkdir -p $P/bin
install -m 0755 $B/gate_core $P/bin/core
install -m 0755 $B/gate_worker $P/bin/echo
install -m 0755 $B/gate_sock $P/bin/sock
cat > $P/atrium.toml <<'T'
[app]
id = "org.example.wk"
name = "Workers gate"
version = "1"
entry = "bin/core"

[[workers]]
name = "echo"
entry = "bin/echo"
cpu = "3s"
memory = "128M"

[[workers]]
name = "sock"
entry = "bin/sock"
T
chown -R alice:files-alice $P
EOS

echo "=== Run, and the core's calls"
# The rctl rules on the worker jails while they run.
V "rm -f $B/rctl.txt; (i=0; while [ \$i -lt 240 ]; do rctl | grep 'w-echo' >> $B/rctl.txt; sleep 0.5; i=\$((i+1)); done) >/dev/null 2>&1 &"
out=$(A "run scratch wk")
case "$out" in dev.alice.scratch.org.example.wk*) ok "Run installs it" ;; *) bad "run: $out"; echo "workers: $fails failed"; exit 1 ;; esac
APP=/var/lib/atrium/apps/dev.alice.scratch.org.example.wk
[ "$(V "test -x $APP/bin/echo && echo 1")" = 1 ] && ok "the worker's entry is in the bundle" || bad "no bin/echo in the bundle"
i=0; while [ $i -lt 120 ] && ! V "grep -q 'workers-gate done' $LOG"; do sleep 3; i=$((i+3)); done
line() { V "grep 'workers-gate $1 ' $LOG | tail -1" | sed "s/.*workers-gate $1 //"; }
core=$(line core-uid); u1=$(line uid); u2=$(line uid-again)
case "$u1" in "ok "*) wu=${u1#ok }
    if [ "$wu" != "$core" ] && [ "$wu" -ge 50000 ] 2>/dev/null; then ok "the worker runs as uid $wu, the core as $core"; else bad "worker uid $wu, core $core"; fi ;;
    *) bad "uid: $u1 ($(V "grep -i worker $LOG | tail -5"))" ;; esac
[ "$u2" = "$u1" ] && ok "and as the same uid on the next call" || bad "uid-again: $u2"
reg=$(V "grep 'worker:dev.alice.scratch.org.example.wk:echo' /var/run/atrium/app-registry | head -1")
case "$reg" in "${wu:-x} alice "*) ok "registered: $reg" ;; *) bad "registry: $reg" ;; esac
r=$(line read); case "$r" in "ok [app]") ok "it reads the file the core passed" ;; *) bad "read: $r" ;; esac
r=$(line fail); case "$r" in "worker-error asked to fail") ok "its error comes back" ;; *) bad "fail: $r" ;; esac
r=$(line net); case "$r" in "ok refused"*) ok "no network: $r" ;; *) bad "net: $r" ;; esac
r=$(line ifs)
case "$r" in "ok ifs=["*) ;; *) bad "ifs: $r" ;; esac
case "$r" in *"ifs=[] "*|*"ifs=[lo0] "*) ok "it lists no host interface: ${r#ok }" ;; *) bad "host interfaces: $r" ;; esac
case "$r" in *"macs=[] "*) ok "and reads no MAC" ;; *) bad "MAC visible: $r" ;; esac
case "$r" in *"ifmib=refused ifconf=refused "*"sockets=[]"*) ok "no interface MIB, no SIOCGIFCONF, no inet/inet6/route/netlink socket" ;; *) bad "network view: $r" ;; esac
nb=$(V "sysctl -n kern.features.jail_netblind 2>/dev/null")
if [ "$nb" = 1 ]; then case "$r" in *"ipstats=refused"*) ok "and none of the host's network counters" ;; *) bad "host counters readable: $r" ;; esac; fi
nb=$(V "sysctl -n kern.features.jail_netblind 2>/dev/null"); [ "$nb" = 1 ] && echo "  (kernel has jail_netblind: worker jails without vnet)" || echo "  (kernel without jail_netblind: worker jails get an empty vnet)"
r=$(line tz); want=$(V "date +%z")
[ "$r" = "ok $want" ] && ok "it tells local time ($want), from the machine's zone file" || bad "tz: worker $r, machine $want"
# The jail decides what exists; capability mode stops the worker reaching
# it by name. ECAPMODE is 94.
r=$(line open); case "$r" in "ok refused: errno 94") ok "it can't open a path its jail has (capability mode)" ;; *) bad "open: $r" ;; esac
r=$(line timing); case "$r" in p50*) ok "20 calls: $r" ;; *) bad "timing: $r" ;; esac
echo "  where a call's time goes (median of the 20 timed calls, ms; teardown runs after the answer):"
V "grep 'oneshot-timing.*w-echo' $LOG | sed -n 8,27p" | python3 -c '
import sys, statistics
v = {}
for l in sys.stdin:
    head, _, rest = l.strip().rpartition(" ms: ")
    v.setdefault("total", []).append(float(head.split("total ")[-1]))
    for part in rest.split(", "):
        k, ms = part.rsplit(" ", 1); v.setdefault(k, []).append(float(ms))
print("    " + ", ".join(f"{k} {statistics.median(x):.0f}" for k, x in v.items()))'
r=$(line nest); case "$r" in "ok refused"*) ok "it calls no workers: $r" ;; *) bad "nest: $r" ;; esac
r=$(line crash); case "$r" in "crashed Some(3)") ok "a crash is an error: $r" ;; *) bad "crash: $r" ;; esac
r=$(line timeout); case "$r" in "timeout 3") ok "a call past 3 s is ended" ;; *) bad "timeout: $r" ;; esac
r=$(line memory); case "$r" in "crashed None") ok "past its 128M it is killed, and that is an error" ;; *) bad "memory: $r ($(V "dmesg | grep -i -e rctl -e 'out of swap' | tail -2"))" ;; esac
r=$(V "grep -m1 'memoryuse' $B/rctl.txt"); case "$r" in *memoryuse:sigkill=134217728*) ok "the worker's jail had the rule: $r" ;; *) bad "no memoryuse rule seen on a worker jail ($(V "sort -u $B/rctl.txt | head -3"))" ;; esac
r=$(line after-memory); [ "$r" = "$u1" ] && ok "and the next call works" || bad "after-memory: $r"
r=$(line nope); case "$r" in no-such-worker) ok "an undeclared worker is refused" ;; *) bad "nope: $r" ;; esac
r=$(line exec-other); case "$r" in "workers.sock"*"only WorkerCall"*) ok "the core has only the worker-call socket: $r" ;; *) bad "exec-other: $r" ;; esac
r=$(line extra-fd); case "$r" in "from-extra a line through the pipe (open errno 94) (exit Some(0))") ok "a socket worker reads the descriptor passed at 3 in its jail, and opens nothing by name" ;; *) bad "extra-fd: $r" ;; esac
sleep 3
left=$(V "jls name | grep -c 'org.example.wk.*w-'" | tr -d ' ')
[ "$left" = 0 ] && ok "no worker jail left" || bad "$left worker jails left: $(V "jls name | grep w-echo")"

A "stop scratch" >/dev/null
echo "workers: $fails failed"
[ $fails = 0 ]
