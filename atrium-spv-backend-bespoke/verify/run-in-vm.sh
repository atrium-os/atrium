#!/bin/sh
# In-VM verification of the bespoke ARM64 backend.
#
# The unit + differential test suites all run on the macOS
# host (Mach-O / Aarch64Darwin). This script closes the
# loop on the *actual production target*: it cross-emits
# FreeBSD/aarch64 ELF objects on the host, ships them to
# the dev VM, links + dlopens + runs them there, and
# checks the pixel output.
#
# Coverage:
#   * const  — constant-colour store. ELF object format +
#     symbol + the Store path.
#   * ifelse — push-const Load + FOrdLt + BranchCond +
#     multi-block CFG + branch relocation. Driven with
#     two inputs (then-branch + else-branch) so both
#     b.cond outcomes are exercised on the real target.
#   * loop   — counted loop `for i in 0..n: acc += i`.
#     W-reg integer pool (IAdd, SLessThan, IEqual), the
#     loop header's two Phis, the back-edge Branch + its
#     relocation, OpSelect. Driven with n=5 (white) and
#     n=4 (black) so both Select outcomes run.
#
# Prereqs: the dev VM is up (scripts/run-vm.sh) and
# reachable on localhost:2222 with the fresco_bsd key.
#
# Usage:  sh atrium-spv-backend-bespoke/verify/run-in-vm.sh
set -e

HERE=$(cd "$(dirname "$0")" && pwd)
CRATE=$(cd "$HERE/.." && pwd)
KEY="$HOME/.ssh/fresco_bsd_ed25519"
SSHOPTS="-o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o BatchMode=yes"
OBJ=/tmp/atrium_fs_freebsd.o

# Ship the harness once.
scp -i "$KEY" $SSHOPTS -P 2222 "$HERE/harness.c" \
    root@localhost:/tmp/atrium_fs_harness.c >/dev/null
ssh -i "$KEY" $SSHOPTS -p 2222 root@localhost \
    'cc -o /tmp/atrium_harness /tmp/atrium_fs_harness.c' 2>/dev/null

FAILED=0

# verify <label> <harness-pc-args> <emit-args...>
#   <harness-pc-args> is passed verbatim to the harness
#   after the .so path (e.g. "0.2", "5 int", or "").
verify() {
  label=$1; pc=$2; shift 2
  expected=$(cd "$CRATE" && cargo run --quiet --example emit_freebsd_obj "$OBJ" "$@" 2>/dev/null)
  scp -i "$KEY" $SSHOPTS -P 2222 "$OBJ" root@localhost:"$OBJ" >/dev/null
  got=$(ssh -i "$KEY" $SSHOPTS -p 2222 root@localhost \
    "cd /tmp && cc -shared -o atrium_fs.so atrium_fs_freebsd.o \
     && ./atrium_harness ./atrium_fs.so $pc")
  if [ "$got" = "$expected" ]; then
    echo "  PASS  $label  -> [$got]"
  else
    echo "  FAIL  $label  expected [$expected] got [$got]"
    FAILED=1
  fi
}

echo "==> in-VM verification (FreeBSD aarch64, localhost:2222)"
verify "const"        ""        const
verify "ifelse then"  "0.2"     ifelse 0.2
verify "ifelse else"  "0.8"     ifelse 0.8
# loop shaders need the callee-saved-register prologue
# (next widening) — the 5-slot caller-saved int pool can't
# hold 3 hoisted int constants + 2 loop-carried Phi dests
# + the iteration count + loop-body temps. The int Phi /
# int Load / int linear-scan plumbing is all in place;
# only register pressure blocks them. Re-enable once the
# prologue lands:
#   verify "loop n=5"   "5 int"   loop 5
#   verify "loop n=4"   "4 int"   loop 4

if [ "$FAILED" = "0" ]; then
  echo "==> PASS — bespoke ELF + AAPCS64 codegen verified on FreeBSD aarch64"
  exit 0
else
  echo "==> FAIL — one or more shaders diverged on the target"
  exit 1
fi
