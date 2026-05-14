#!/bin/sh
# End-to-end in-VM verification of the *production* tier-2
# compile chain.
#
# run-in-vm.sh proves the bespoke backend's object emission
# + AAPCS64 codegen run on FreeBSD/aarch64 — but it emits
# the object *host-side* and only ships the .o. This script
# closes the remaining gap: it drives the real
# `atrium-spv-compile` binary (cross-built for FreeBSD
# aarch64) *inside the VM*, on a raw SPIR-V input, and
# checks the .so it produces:
#
#   SPIR-V file  ->  atrium-spv-compile (in VM)
#                ->  frontend + bespoke backend + `cc -shared`
#                ->  <hash>.so + <hash>.pcmap
#                ->  dlopen + atrium_fs_main  ->  pixels
#
# So this exercises the production binary's argument
# handling, the bespoke-first/Cranelift-fallback selection,
# the in-VM linker invocation, and the .pcmap sidecar — the
# whole chain the daemon's Tier2Backend shells out to, on
# the real target.
#
# Prereqs: dev VM up + reachable on localhost:2222 with the
# fresco_bsd key; host cross-compile toolchain configured
# (see RUNBOOK "D1 host cross-compile").
#
# Usage:  sh atrium-spv-backend-bespoke/verify/run-e2e-in-vm.sh
# NOTE: deliberately not `set -e` — see run-in-vm.sh.

HERE=$(cd "$(dirname "$0")" && pwd)
CRATE=$(cd "$HERE/.." && pwd)
WS=$(cd "$CRATE/.." && pwd)
KEY="$HOME/.ssh/fresco_bsd_ed25519"
SSHOPTS="-o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o BatchMode=yes -o ConnectTimeout=15"
SPV=/tmp/atrium_e2e.spv
COMPILE_BIN="$WS/atrium-spv-compile/target/aarch64-unknown-freebsd/release/atrium-spv-compile"

echo "==> cross-building atrium-spv-compile for FreeBSD aarch64"
( cd "$WS/atrium-spv-compile" && \
  cargo build --target aarch64-unknown-freebsd --release ) \
  >/dev/null 2>&1
if [ ! -x "$COMPILE_BIN" ]; then
  echo "  FAIL  cross-build produced no binary at $COMPILE_BIN"
  exit 1
fi

# Ship the harness + the compile binary once.
scp -i "$KEY" $SSHOPTS -P 2222 "$HERE/harness.c" \
    root@localhost:/tmp/atrium_fs_harness.c >/dev/null 2>&1
ssh -i "$KEY" $SSHOPTS -p 2222 root@localhost \
    'cc -o /tmp/atrium_harness /tmp/atrium_fs_harness.c' >/dev/null 2>&1
scp -i "$KEY" $SSHOPTS -P 2222 "$COMPILE_BIN" \
    root@localhost:/tmp/atrium-spv-compile >/dev/null 2>&1
ssh -i "$KEY" $SSHOPTS -p 2222 root@localhost \
    'chmod +x /tmp/atrium-spv-compile' >/dev/null 2>&1

FAILED=0

# verify <label> <expect-backend> <harness-pc-args> <emit-kind> [emit-args...]
#   <expect-backend> is "bespoke" or "cranelift" — the
#   backend atrium-spv-compile's metrics line must report.
#   A mismatch (e.g. a shader silently falling back, or a
#   regression that breaks the bespoke path) fails the row.
verify() {
  label=$1; want_backend=$2; pc=$3; shift 3
  expected=$(cd "$CRATE" && \
    cargo run --quiet --example emit_freebsd_obj "$SPV" spirv "$@" 2>/dev/null)
  if [ -z "$expected" ]; then
    echo "  FAIL  $label  (host spirv emit produced no output)"
    FAILED=1; return
  fi
  if ! scp -i "$KEY" $SSHOPTS -P 2222 "$SPV" root@localhost:"$SPV" >/dev/null 2>&1; then
    echo "  FAIL  $label  (scp of .spv to VM failed)"
    FAILED=1; return
  fi
  # In the VM: run the production compile binary on the
  # SPIR-V, then dlopen the .so it wrote. --hash e2e makes
  # the output filename deterministic. The binary defaults
  # to Target::host() == Aarch64FreeBSD inside the VM.
  got=$(ssh -i "$KEY" $SSHOPTS -p 2222 root@localhost \
    "rm -rf /tmp/atrium_e2e_cache && \
     /tmp/atrium-spv-compile --input $SPV \
       --output-dir /tmp/atrium_e2e_cache --hash e2e \
       >/dev/null 2>/tmp/atrium_e2e.json && \
     /tmp/atrium_harness /tmp/atrium_e2e_cache/e2e.so $pc" 2>/dev/null)
  backend=$(ssh -i "$KEY" $SSHOPTS -p 2222 root@localhost \
    "sed -n 's/.*\"backend\":\"\\([a-z]*\\)\".*/\\1/p' /tmp/atrium_e2e.json" \
    2>/dev/null)
  if [ "$got" != "$expected" ]; then
    echo "  FAIL  $label  expected [$expected] got [$got]  (backend=$backend)"
    FAILED=1
  elif [ "$backend" != "$want_backend" ]; then
    echo "  FAIL  $label  pixels OK [$got] but backend=$backend, wanted $want_backend"
    FAILED=1
  else
    echo "  PASS  $label  -> [$got]  (backend=$backend)"
  fi
}

echo "==> end-to-end in-VM verification (FreeBSD aarch64, localhost:2222)"
#       label          backend     pc        kind   [args]
verify "const"        bespoke   ""        const
verify "ifelse then"  bespoke   "0.2"     ifelse 0.2
verify "ifelse else"  bespoke   "0.8"     ifelse 0.8
verify "loop n=5"     bespoke   "5 int"   loop 5
verify "loop n=4"     bespoke   "4 int"   loop 4
verify "switch n=1"   bespoke   "1 int"   switch 1
verify "switch n=9"   bespoke   "9 int"   switch 9
# OpFUnordLessThan: bespoke has no arm for it, so the
# production binary must fall back to Cranelift and still
# produce correct pixels — the fallback-path probe.
verify "unordcmp lt"  cranelift "0.2"     unordcmp 0.2
verify "unordcmp ge"  cranelift "0.8"     unordcmp 0.8

if [ "$FAILED" = "0" ]; then
  echo "==> PASS — production atrium-spv-compile chain verified on FreeBSD aarch64"
  exit 0
else
  echo "==> FAIL — one or more shaders diverged through the production chain"
  exit 1
fi
