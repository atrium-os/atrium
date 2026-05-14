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
#   * arith  — `out = (n*2+1)*0.125`, no control flow. IMul,
#     IAdd, ConvertSToF (scvtf), FMul on the W-reg pool +
#     int->float path. On-target twin of the host
#     three_way_int_arith_and_convert differential test.
#   * bitwise — nibble/xor/or extraction from an i32 push-
#     const: AShr, BitAnd, BitXor, BitOr + ConvertSToF +
#     FDiv, all power-of-two normalised. On-target twin of
#     the host three_way_bitwise_and_shift differential test.
#   * vecarith — (a+b)*(a-b) over two constant vec4s: per-
#     lane FAdd/FSub/FMul + the V-reg vector lane allocator
#     across three chained vec4 ops. On-target twin of the
#     host three_way_vec_arithmetic differential test.
#   * switch — switch(n){0:red 1:green 2:blue default:white}
#     from an i32 push-const: OpSwitch multi-target jump
#     codegen + a 5-block CFG with four branch relocations.
#     Driven with n=0 (a case), n=2 (last case), n=7
#     (default) so the case path and the fall-through both
#     run. On-target twin of the host three_way_switch_*.
#   * phi — if(scale<0.5) 1.0 else 0.25, joined by an OpPhi
#     at the merge block (empty then/else blocks). Phi
#     convergence in a non-loop CFG: each arm's value must
#     land in the Phi's register on the correct predecessor
#     edge. Driven with 0.2 (then) and 0.8 (else). On-target
#     twin of the host three_way_phi_* differential tests.
#
# Prereqs: the dev VM is up (scripts/run-vm.sh) and
# reachable on localhost:2222 with the fresco_bsd key.
#
# Usage:  sh atrium-spv-backend-bespoke/verify/run-in-vm.sh
# NOTE: deliberately *not* `set -e`. The verify function
# tracks failures itself; aborting the whole script on a
# single slow ssh (the dev VM is often under concurrent
# host-build load) would mask the later shaders. Each
# step instead carries a generous ConnectTimeout and the
# function records FAILED on any mismatch-or-error.

HERE=$(cd "$(dirname "$0")" && pwd)
CRATE=$(cd "$HERE/.." && pwd)
KEY="$HOME/.ssh/fresco_bsd_ed25519"
SSHOPTS="-o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o BatchMode=yes -o ConnectTimeout=15"
OBJ=/tmp/atrium_fs_freebsd.o

# Ship the harness once. (Best-effort; verify() re-checks.)
scp -i "$KEY" $SSHOPTS -P 2222 "$HERE/harness.c" \
    root@localhost:/tmp/atrium_fs_harness.c >/dev/null 2>&1
ssh -i "$KEY" $SSHOPTS -p 2222 root@localhost \
    'cc -o /tmp/atrium_harness /tmp/atrium_fs_harness.c' >/dev/null 2>&1

FAILED=0

# verify <label> <harness-pc-args> <emit-args...>
#   <harness-pc-args> is passed verbatim to the harness
#   after the .so path (e.g. "0.2", "5 int", or "").
verify() {
  label=$1; pc=$2; shift 2
  expected=$(cd "$CRATE" && cargo run --quiet --example emit_freebsd_obj "$OBJ" "$@" 2>/dev/null)
  if [ -z "$expected" ]; then
    echo "  FAIL  $label  (host emit produced no output)"
    FAILED=1; return
  fi
  if ! scp -i "$KEY" $SSHOPTS -P 2222 "$OBJ" root@localhost:"$OBJ" >/dev/null 2>&1; then
    echo "  FAIL  $label  (scp to VM failed — is the VM up + idle?)"
    FAILED=1; return
  fi
  got=$(ssh -i "$KEY" $SSHOPTS -p 2222 root@localhost \
    "cd /tmp && cc -shared -o atrium_fs.so atrium_fs_freebsd.o \
     && ./atrium_harness ./atrium_fs.so $pc" 2>/dev/null)
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
verify "loop n=5"     "5 int"   loop 5
verify "loop n=4"     "4 int"   loop 4
verify "arith n=3"    "3 int"   arith 3
verify "arith n=1"    "1 int"   arith 1
verify "bitwise 0x53" "83 int"  bitwise 83
verify "bitwise 0xC1" "193 int" bitwise 193
verify "vecarith"     ""        vecarith
verify "switch n=0"   "0 int"   switch 0
verify "switch n=2"   "2 int"   switch 2
verify "switch n=7"   "7 int"   switch 7
verify "phi then"     "0.2"     phi 0.2
verify "phi else"     "0.8"     phi 0.8

if [ "$FAILED" = "0" ]; then
  echo "==> PASS — bespoke ELF + AAPCS64 codegen verified on FreeBSD aarch64"
  exit 0
else
  echo "==> FAIL — one or more shaders diverged on the target"
  exit 1
fi
