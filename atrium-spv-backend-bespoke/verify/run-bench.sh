#!/bin/sh
# bespoke-backend micro-benchmark, host + in-VM.
#
# Runs the bench_driver example over a fixed shader corpus
# and prints, per shader:
#
#   compile  — ns for backend::compile() (bespoke vs
#              Cranelift — same IR input, the fair compile
#              comparison). Cache-miss latency budget.
#   run      — ns per atrium_fs_main call. The per-draw hot
#              path (spec §8.1). bespoke vs Cranelift vs —
#              for shaders with a hand-written C reference
#              in native/ — `clang -O2`, the perf *bar*.
#              Cranelift dragged bespoke up to fast-tier
#              quality; the question now is the gap to a
#              real optimising native compiler.
#
# The same bench_driver binary is run on the macOS host
# (Aarch64Darwin) and, cross-built, inside the FreeBSD/
# aarch64 VM — so the report is a host vs on-target pair.
#
# Prereqs: dev VM up + reachable on localhost:2222 with the
# fresco_bsd key; host cross-compile toolchain configured.
#
# Usage:  sh atrium-spv-backend-bespoke/verify/run-bench.sh

HERE=$(cd "$(dirname "$0")" && pwd)
CRATE=$(cd "$HERE/.." && pwd)
WS=$(cd "$CRATE/.." && pwd)
KEY="$HOME/.ssh/fresco_bsd_ed25519"
SSHOPTS="-o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o BatchMode=yes -o ConnectTimeout=15"
HOST_BIN="$WS/atrium-spv-compile/target/release/examples/bench_driver"
VM_BIN="$WS/atrium-spv-compile/target/aarch64-unknown-freebsd/release/examples/bench_driver"

# Shader corpus: <label>|<kind>|<pc args>|<native-C ref>
# The pc args double as the bench_driver push-const args
# (same convention as the harness): the kind's first arg is
# both the shader parameter and the runtime push-const.
# The 4th field, when set, is a hand-written C reference
# (path relative to verify/) — bench_driver compiles it
# with `clang -O2` as the runtime perf bar. Only shaders
# whose runtime clears call overhead get one; `heavy` is
# the meaningful case.
CORPUS='
const|const||
ifelse|ifelse|0.2|
loop|loop|64 int|native/loop.c
switch|switch|2 int|
arith|arith|7 int|
bitwise|bitwise|0x53 int|
vecarith|vecarith||
dot|dot||
shuffle|shuffle||
cextract|cextract||
phi|phi|0.2|
unordcmp|unordcmp|0.2|
heavy|heavy|512 int|native/heavy.c
heavy4|heavy4|256 int|native/heavy4.c
'

echo "==> building bench_driver (host release + FreeBSD aarch64 cross)"
( cd "$WS/atrium-spv-compile" && \
  cargo build --release --example bench_driver ) >/dev/null 2>&1
( cd "$WS/atrium-spv-compile" && \
  cargo build --target aarch64-unknown-freebsd --release \
    --example bench_driver ) >/dev/null 2>&1
if [ ! -x "$HOST_BIN" ] || [ ! -x "$VM_BIN" ]; then
  echo "  FAIL  bench_driver build produced no binaries"
  exit 1
fi

scp -i "$KEY" $SSHOPTS -P 2222 "$VM_BIN" \
    root@localhost:/tmp/bench_driver >/dev/null 2>&1
ssh -i "$KEY" $SSHOPTS -p 2222 root@localhost \
    'chmod +x /tmp/bench_driver' >/dev/null 2>&1

# emit_spirv <kind> <emit-args...> -> writes /tmp/bench_X.spv
# returns 0 on success.
emit_spirv() {
  kind=$1; shift
  (cd "$CRATE" && cargo run --quiet --example emit_freebsd_obj \
     "/tmp/bench_$kind.spv" spirv "$kind" "$@" >/dev/null 2>&1)
}

run_one() {
  where=$1; label=$2; kind=$3; pc=$4; native=$5
  if [ "$where" = "host" ]; then
    native_arg=""
    [ -n "$native" ] && native_arg="--native $HERE/$native"
    "$HOST_BIN" "/tmp/bench_$kind.spv" $pc $native_arg 2>/dev/null \
      | grep -E "compile:|run:"
  else
    scp -i "$KEY" $SSHOPTS -P 2222 "/tmp/bench_$kind.spv" \
        root@localhost:"/tmp/bench_$kind.spv" >/dev/null 2>&1
    native_arg=""
    if [ -n "$native" ]; then
      scp -i "$KEY" $SSHOPTS -P 2222 "$HERE/$native" \
          root@localhost:"/tmp/bench_native_$kind.c" >/dev/null 2>&1
      native_arg="--native /tmp/bench_native_$kind.c"
    fi
    # -n: don't read stdin. Without it ssh drains the
    # `while read` loop's CORPUS pipe and the loop stops
    # after the first shader.
    ssh -n -i "$KEY" $SSHOPTS -p 2222 root@localhost \
      "/tmp/bench_driver /tmp/bench_$kind.spv $pc $native_arg" 2>/dev/null \
      | grep -E "compile:|run:"
  fi
}

run_corpus() {
  where=$1; title=$2
  echo
  echo "############################################################"
  echo "## $title  (lower ns = better; native = clang -O2 bar)"
  echo "############################################################"
  echo "$CORPUS" | while IFS='|' read -r label kind pc native; do
    [ -z "$label" ] && continue
    if ! emit_spirv "$kind" $pc; then
      echo "  $label  FAIL (host spirv emit)"
      continue
    fi
    run_one "$where" "$label" "$kind" "$pc" "$native"
  done
}

run_corpus host  "HOST (macOS / Aarch64Darwin)"
run_corpus vm    "IN-VM (FreeBSD / Aarch64FreeBSD)"

echo
echo "==> benchmark complete"
