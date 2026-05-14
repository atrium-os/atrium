#!/bin/sh
# In-VM verification of the atrium-spv-loader cache path.
#
# run-e2e-in-vm.sh drives the atrium-spv-compile binary
# directly. This script goes one layer up: it exercises
# atrium-spv-loader's ShaderCache — the daemon-side
# component that hashes SPIR-V, looks up the on-disk
# cache, spawns atrium-spv-compile only on a miss, dlopens
# the result, and hands back typed entry-point pointers.
#
# The driver (examples/loader_e2e_driver.rs) loads each
# shader twice:
#   1. cold — a real compile binary; cache miss spawns it.
#   2. warm — a *fresh* ShaderCache whose compile binary is
#      a bogus path. A miss would try to spawn it and fail,
#      so success proves the load came purely from the
#      on-disk <hash>.so — no re-compile.
# It then calls the disk-cache-loaded entry point and
# prints the RGBA. So a PASS proves: loader hashing, the
# loader<->compile-binary handshake, the disk cache, the
# dlopen/dlsym path, and the AAPCS64 entry-point call —
# all on the production FreeBSD/aarch64 target.
#
# Prereqs: dev VM up + reachable on localhost:2222 with the
# fresco_bsd key; host cross-compile toolchain configured.
#
# Usage: sh atrium-spv-backend-bespoke/verify/run-loader-e2e-in-vm.sh
# NOTE: deliberately not `set -e` — see run-in-vm.sh.

HERE=$(cd "$(dirname "$0")" && pwd)
CRATE=$(cd "$HERE/.." && pwd)
WS=$(cd "$CRATE/.." && pwd)
KEY="$HOME/.ssh/fresco_bsd_ed25519"
SSHOPTS="-o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o BatchMode=yes -o ConnectTimeout=15"
SPV=/tmp/atrium_loader_e2e.spv
COMPILE_BIN="$WS/atrium-spv-compile/target/aarch64-unknown-freebsd/release/atrium-spv-compile"
DRIVER_BIN="$WS/atrium-spv-loader/target/aarch64-unknown-freebsd/release/examples/loader_e2e_driver"

echo "==> cross-building atrium-spv-compile + loader_e2e_driver for FreeBSD aarch64"
( cd "$WS/atrium-spv-compile" && \
  cargo build --target aarch64-unknown-freebsd --release ) >/dev/null 2>&1
( cd "$WS/atrium-spv-loader" && \
  cargo build --target aarch64-unknown-freebsd --release \
    --example loader_e2e_driver ) >/dev/null 2>&1
if [ ! -x "$COMPILE_BIN" ] || [ ! -x "$DRIVER_BIN" ]; then
  echo "  FAIL  cross-build produced no binaries"
  echo "        compile: $COMPILE_BIN"
  echo "        driver:  $DRIVER_BIN"
  exit 1
fi

# Ship the two binaries once.
scp -i "$KEY" $SSHOPTS -P 2222 "$COMPILE_BIN" \
    root@localhost:/tmp/atrium-spv-compile >/dev/null 2>&1
scp -i "$KEY" $SSHOPTS -P 2222 "$DRIVER_BIN" \
    root@localhost:/tmp/loader_e2e_driver >/dev/null 2>&1
ssh -i "$KEY" $SSHOPTS -p 2222 root@localhost \
    'chmod +x /tmp/atrium-spv-compile /tmp/loader_e2e_driver' >/dev/null 2>&1

FAILED=0

# verify <label> <driver-pc-args> <emit-kind> [emit-args...]
verify() {
  label=$1; pc=$2; shift 2
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
  # In the VM: wipe the cache dir, then run the driver. The
  # driver does the cold-compile + warm-disk-cache load
  # itself and prints the RGBA from the disk-cache handle.
  got=$(ssh -i "$KEY" $SSHOPTS -p 2222 root@localhost \
    "rm -rf /tmp/atrium_loader_cache && \
     /tmp/loader_e2e_driver $SPV /tmp/atrium-spv-compile \
       /tmp/atrium_loader_cache $pc" 2>/dev/null)
  if [ "$got" = "$expected" ]; then
    echo "  PASS  $label  -> [$got]"
  else
    echo "  FAIL  $label  expected [$expected] got [$got]"
    FAILED=1
  fi
}

echo "==> loader-cache in-VM verification (FreeBSD aarch64, localhost:2222)"
verify "const"        ""        const
verify "ifelse then"  "0.2"     ifelse 0.2
verify "loop n=5"     "5 int"   loop 5
verify "switch n=2"   "2 int"   switch 2
# Cranelift-fallback shader through the loader path too.
verify "unordcmp lt"  "0.2"     unordcmp 0.2

if [ "$FAILED" = "0" ]; then
  echo "==> PASS — atrium-spv-loader cache path verified on FreeBSD aarch64"
  exit 0
else
  echo "==> FAIL — loader cache path diverged on the target"
  exit 1
fi
