#!/bin/sh
# Run the full in-VM verification suite for the tier-2
# software Vulkan shader pipeline, end to end, with a
# single pass/fail summary.
#
# Wraps the four focused scripts, each of which verifies a
# different layer of the production chain on the real
# FreeBSD/aarch64 target:
#
#   run-in-vm.sh          bespoke backend object emission +
#                         AAPCS64 codegen (host-emitted .o,
#                         linked + run in the VM)
#   run-e2e-in-vm.sh      the production atrium-spv-compile
#                         binary, run in the VM, incl. the
#                         bespoke-first / Cranelift-fallback
#                         backend selection
#   run-loader-e2e-in-vm.sh  atrium-spv-loader's ShaderCache:
#                         hashing, the compile-binary
#                         handshake, the path-versioned disk
#                         cache, dlopen + entry-point call
#   run-pcmap-e2e-in-vm.sh   the .pcmap sidecar round-trip
#                         through the loader's parser
#
# Each sub-script is self-contained (cross-builds what it
# needs, ships it, runs it); this wrapper just sequences
# them and tallies the result. A sub-script failure does
# not abort the run — every layer is exercised so a single
# break doesn't mask the others.
#
# Prereqs: dev VM up + reachable on localhost:2222 with the
# fresco_bsd key; host cross-compile toolchain configured.
#
# Usage:  sh atrium-spv-backend-bespoke/verify/run-all-in-vm.sh

HERE=$(cd "$(dirname "$0")" && pwd)

SCRIPTS="run-in-vm.sh run-e2e-in-vm.sh run-loader-e2e-in-vm.sh run-pcmap-e2e-in-vm.sh"

PASSED=0
FAILED=0
FAILED_NAMES=""

for s in $SCRIPTS; do
  echo
  echo "############################################################"
  echo "## $s"
  echo "############################################################"
  if sh "$HERE/$s"; then
    PASSED=$((PASSED + 1))
  else
    FAILED=$((FAILED + 1))
    FAILED_NAMES="$FAILED_NAMES $s"
  fi
done

echo
echo "============================================================"
echo "== in-VM verification summary"
echo "============================================================"
echo "   passed: $PASSED / $((PASSED + FAILED)) scripts"
if [ "$FAILED" = "0" ]; then
  echo "==> PASS — full tier-2 in-VM pipeline verified on FreeBSD aarch64"
  exit 0
else
  echo "   failed:$FAILED_NAMES"
  echo "==> FAIL — $FAILED in-VM verification script(s) diverged"
  exit 1
fi
