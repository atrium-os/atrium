#!/bin/sh
# In-VM verification of the .pcmap sidecar round-trip.
#
# atrium-spv-compile writes a `<hash>.pcmap` sidecar next
# to every `<hash>.so` — the host-PC -> SPIR-V-offset map
# the daemon's crash handler uses for source attribution.
# atrium-spv-loader reads it back and parses it via
# `PcMap::from_bytes`, storing `Some(PcMap)` on the
# LoadedShader (or `None` if the file is missing/bad).
#
# This script drives loader_e2e_driver in `--check-pcmap`
# mode on the target: it loads each shader through the
# loader, then reports the parsed sidecar state instead of
# running the shader. A PASS means the `.pcmap` the
# production compile binary emitted on FreeBSD/aarch64
# round-trips cleanly through the loader's parser there —
# so crash-triage source attribution is wired end to end
# on the real target, not just the macOS host.
#
# Prereqs: dev VM up + reachable on localhost:2222 with the
# fresco_bsd key; host cross-compile toolchain configured.
#
# Usage: sh atrium-spv-backend-bespoke/verify/run-pcmap-e2e-in-vm.sh
# NOTE: deliberately not `set -e` — see run-in-vm.sh.

HERE=$(cd "$(dirname "$0")" && pwd)
CRATE=$(cd "$HERE/.." && pwd)
WS=$(cd "$CRATE/.." && pwd)
KEY="$HOME/.ssh/fresco_bsd_ed25519"
SSHOPTS="-o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o BatchMode=yes -o ConnectTimeout=15"
SPV=/tmp/atrium_pcmap_e2e.spv
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
  exit 1
fi

scp -i "$KEY" $SSHOPTS -P 2222 "$COMPILE_BIN" \
    root@localhost:/tmp/atrium-spv-compile >/dev/null 2>&1
scp -i "$KEY" $SSHOPTS -P 2222 "$DRIVER_BIN" \
    root@localhost:/tmp/loader_e2e_driver >/dev/null 2>&1
ssh -i "$KEY" $SSHOPTS -p 2222 root@localhost \
    'chmod +x /tmp/atrium-spv-compile /tmp/loader_e2e_driver' >/dev/null 2>&1

FAILED=0

# verify <label> <emit-kind> [emit-args...]
verify() {
  label=$1; shift
  if ! (cd "$CRATE" && \
        cargo run --quiet --example emit_freebsd_obj "$SPV" spirv "$@" \
        >/dev/null 2>&1); then
    echo "  FAIL  $label  (host spirv emit failed)"
    FAILED=1; return
  fi
  if ! scp -i "$KEY" $SSHOPTS -P 2222 "$SPV" root@localhost:"$SPV" >/dev/null 2>&1; then
    echo "  FAIL  $label  (scp of .spv to VM failed)"
    FAILED=1; return
  fi
  got=$(ssh -i "$KEY" $SSHOPTS -p 2222 root@localhost \
    "rm -rf /tmp/atrium_pcmap_cache && \
     /tmp/loader_e2e_driver $SPV /tmp/atrium-spv-compile \
       /tmp/atrium_pcmap_cache --check-pcmap" 2>/dev/null)
  # Expect: "pcmap present entries=N ... mid_lookup=Z" with
  # N >= 1 and a successful lookup (mid_lookup != none).
  case "$got" in
    "pcmap present entries=0 "*)
      echo "  FAIL  $label  pcmap parsed but empty: [$got]"
      FAILED=1 ;;
    *" mid_lookup=none")
      echo "  FAIL  $label  lookup() failed on-target: [$got]"
      FAILED=1 ;;
    "pcmap present entries="*" mid_lookup="*)
      echo "  PASS  $label  -> $got" ;;
    *)
      echo "  FAIL  $label  expected 'pcmap present ...' got [$got]"
      FAILED=1 ;;
  esac
}

echo "==> pcmap-sidecar in-VM verification (FreeBSD aarch64, localhost:2222)"
verify "const"     const
verify "ifelse"    ifelse 0.2
verify "loop"      loop 5
verify "switch"    switch 2
verify "unordcmp"  unordcmp 0.2

if [ "$FAILED" = "0" ]; then
  echo "==> PASS — .pcmap sidecar round-trips through the loader on FreeBSD aarch64"
  exit 0
else
  echo "==> FAIL — pcmap sidecar round-trip diverged on the target"
  exit 1
fi
