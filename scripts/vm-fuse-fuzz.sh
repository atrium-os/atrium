#!/bin/sh
# fusefs from the server side (docs/spec/network-shares.md §4.4, item 1): a
# hostile FUSE server (capsa-fuse's atrium-fuse-hostile) answers the kernel
# with mutated replies while unprivileged driver processes use the mount.
# Runs on the HOST against the cloud amd64 VM (scripts/cloud-vm.sh up):
#
#   sh scripts/vm-fuse-fuzz.sh [MINUTES] [FIRST_SEED]
#
# Each round is one seed for ROUND seconds (default 120); rounds continue
# until MINUTES (default 20) have passed. After each round it checks the VM
# answers, the mount came down, and the kernel said nothing alarming
# (panic, "lock order reversal", KASSERT, "fuse" warnings other than its
# protocol-violation notices) in dmesg. A
# round that kills the VM is reported with its seed, which reproduces it,
# and the serial console's last lines.
#
# Env: ROUND=secs, MUTATE=percent (30), DRIVERS (3), TIMEOUT=fusefs
# answer timeout (2), VMDIR (~/atrium-vm-amd64), OUT (scratch/fuse-fuzz).
set -u
ROOT=$(cd "$(dirname "$0")/.." && pwd)
MINUTES=${1:-20}
SEED=${2:-$(date +%s)}
ROUND=${ROUND:-120}
MUTATE=${MUTATE:-30}
DRIVERS=${DRIVERS:-3}
TIMEOUT=${TIMEOUT:-2}
VMDIR=${VMDIR:-$HOME/atrium-vm-amd64}
OUT=${OUT:-$ROOT/scratch/fuse-fuzz}
TRIPLE=x86_64-unknown-freebsd
mkdir -p "$OUT"
V() { "$ROOT/scripts/vssh" "$@"; }
K="-i $HOME/.ssh/fresco_bsd_ed25519 -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o LogLevel=ERROR"

echo "== build"
CROSS_RUSTFLAGS=$(cd "$ROOT" && ARCH=amd64 sh -c '. ./scripts/cloud-vm.sh >/dev/null; printf %s "$CROSS_RUSTFLAGS"' sh env)
( cd "$ROOT/capsa" && RUSTFLAGS="$CROSS_RUSTFLAGS" cargo build -q --release --target $TRIPLE -p capsa-fuse --no-default-features --bin atrium-fuse-hostile ) || { echo "build failed"; exit 1; }
B="$ROOT/capsa/target/$TRIPLE/release/atrium-fuse-hostile"
V true || { echo "the VM does not answer"; exit 1; }
V "mkdir -p /root/sc /tmp/fz/mnt"
scp -q $K -P 2222 "$B" root@localhost:/root/sc/atrium-fuse-hostile
[ "$(sha256sum "$B" | cut -d' ' -f1)" = "$(V 'sha256 -q /root/sc/atrium-fuse-hostile')" ] || { echo "copy corrupted"; exit 1; }
V "dmesg -c >/dev/null 2>&1; kldload -n fusefs 2>/dev/null; true"
echo "  kernel: $(V 'uname -v' | cut -c1-80)"
echo "  debug: $(V 'sysctl -n kern.conftxt | grep -E "^options[[:space:]]+(INVARIANTS|WITNESS|KASAN)$" | awk "{print \$2}" | tr "\n" " "')"

fails=0; rounds=0; reqs=0; muts=0
end=$(( $(date +%s) + MINUTES * 60 ))
while [ "$(date +%s)" -lt "$end" ]; do
  rounds=$((rounds + 1))
  s=$((SEED + rounds))
  line=$(V "timeout $((ROUND + 120)) /root/sc/atrium-fuse-hostile --mnt /tmp/fz/mnt --secs $ROUND --seed $s --mutate $MUTATE --drivers $DRIVERS --timeout $TIMEOUT --uid 65534 2>/tmp/fz/round.err; echo rc=\$?" 2>&1)
  if ! V true 2>/dev/null; then
    echo "  FAIL seed $s: the VM went away"
    python3 -I -c '
import socket,sys
s=socket.socket(socket.AF_UNIX); s.settimeout(2)
try:
    s.connect(sys.argv[1]); d=b""
    while True:
        b=s.recv(65536)
        if not b: break
        d+=b
except Exception: pass
sys.stdout.write(d.decode(errors="replace")[-4000:])' "$VMDIR/serial.sock" > "$OUT/seed-$s-serial.txt" 2>/dev/null
    tail -20 "$OUT/seed-$s-serial.txt" | sed 's/^/    /'
    fails=$((fails + 1))
    break
  fi
  summary=$(echo "$line" | grep 'fuse-hostile: seed' | tail -1)
  rc=$(echo "$line" | grep -o 'rc=[0-9]*' | tail -1)
  mounted=$(V "mount -p | grep -c ' /tmp/fz/mnt '")
  # fusefs's "FUSE protocol violation" warnings are the kernel catching the
  # server, which is the point; anything else from fuse is worth a look.
  bad=$(V "dmesg | grep -v 'FUSE protocol violation' | grep -iE 'panic|lock order reversal|KASSERT|fuse.*(warn|bad|invalid)|witness' | head -5")
  V "dmesg -c >/dev/null"
  r=$(echo "$summary" | sed -n 's/.*: \([0-9]*\) requests, \([0-9]*\) mutated.*/\1 \2/p')
  reqs=$((reqs + ${r%% *})); muts=$((muts + ${r##* }))
  if [ "$rc" = "rc=0" ] && [ "$mounted" = "0" ] && [ -z "$bad" ]; then
    echo "  ok seed $s: ${summary#*: }"
  else
    echo "  FAIL seed $s: $rc, mounted=$mounted, ${summary#*: }"
    [ -n "$bad" ] && echo "$bad" | sed 's/^/    dmesg: /'
    V "tail -5 /tmp/fz/round.err" | sed 's/^/    /'
    V "umount -f /tmp/fz/mnt 2>/dev/null; true"
    fails=$((fails + 1))
  fi
done
echo "fuse-fuzz: $rounds rounds, $reqs requests, $muts mutated replies, $fails failed"
exit $fails
