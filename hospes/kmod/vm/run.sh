#!/bin/sh
# K1 in the guest (scripts/hospes-k1-vm.sh copies this with the module, the fixture runner,
# the fixtures, hospes-cdm and the H3 test CDM). Needs the atrium-os kernel with the core
# hook (sys/systrap.h) booted. ARCH=x86_64|aarch64.
set -u
cd "$(dirname "$0")"
fails=0
pass() { echo "PASS $*"; }
fail() { echo "FAIL $*"; fails=$((fails + 1)); }
R=./bin/hospes-fixture-runner
F=./fixtures

echo "== kernel: $(sysctl -n kern.bootfile) ($(uname -r))"
kldstat -q -n hospes_trap && kldunload hospes_trap

# 1. No module: the host refuses to run the library (fails closed).
if $R trap $F/trap.so 2>/tmp/k1.err; then fail "trap ran without hospes_trap.ko"; else
    grep -q "no system call trap" /tmp/k1.err && pass "no module: the host refuses to start" || { fail "no module: wrong error"; cat /tmp/k1.err; }
fi

kldload ./hospes_trap.ko || { echo "FAIL kldload"; exit 1; }
echo "== $(dmesg | grep 'hospes_trap: syscall' | tail -1)"

# 2. Every fixture, under capability mode: loader tests, the isolation probe, the trap.
if sh ./hospes-build.sh test $R $F; then pass "fixtures (with the probe and trap.so)"; else fail "fixtures"; fi

# 3. The same as nobody.
chmod -R a+rX . 
if su -m nobody -c "$R trap $F/trap.so" 2>&1 | tail -1 | grep -q '^PASS'; then pass "trap.so as nobody"; else fail "trap.so as nobody"; fi

# 4. The module cannot unload while a process has a range table; it can once it exits.
$R trap-hold $F/trap.so 2>/dev/null &
hold=$!
sleep 3
if kldunload hospes_trap 2>/dev/null; then fail "unloaded under a live table"; kldload ./hospes_trap.ko; else pass "unload refused while a process has ranges"; fi
wait $hold
if kldunload hospes_trap; then pass "unload after the process exited (table freed)"; else fail "unload after exit"; fi
kldload ./hospes_trap.ko

# 5. H3's Clear Key CDM under hospes-cdm, which now traps (and needs the module).
if ./bin/hospes-cdm-check ./bin/hospes-cdm ./$ARCH-clearkey.so ./vectors > /tmp/k1.cdm 2>/tmp/k1.cdm.err &&
    diff ./vectors/transcript.txt /tmp/k1.cdm; then pass "H3 CDM transcript with the trap"; else fail "H3 CDM transcript"; tail -5 /tmp/k1.cdm.err; fi

kldunload hospes_trap
echo "k1 vm: $fails failed"
[ "$fails" -eq 0 ]
