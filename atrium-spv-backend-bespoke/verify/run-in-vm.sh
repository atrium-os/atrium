#!/bin/sh
# In-VM verification of the bespoke ARM64 backend.
#
# The unit + differential test suites all run on the macOS
# host (Mach-O / Aarch64Darwin). This script closes the
# loop on the *actual production target*: it cross-emits a
# FreeBSD/aarch64 ELF object on the host, ships it to the
# dev VM, links + dlopens + runs it there, and checks the
# pixel output.
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

# 1. Cross-emit the FreeBSD ELF object on the host. The
#    example prints the expected RGBA to stdout.
echo "==> emitting FreeBSD aarch64 ELF object on host"
EXPECTED=$(cd "$CRATE" && cargo run --quiet --example emit_freebsd_obj "$OBJ")
echo "    expected RGBA: $EXPECTED"

# 2. Ship object + harness into the VM.
echo "==> copying into VM (localhost:2222)"
scp -i "$KEY" $SSHOPTS -P 2222 "$OBJ" root@localhost:/tmp/atrium_fs_freebsd.o
scp -i "$KEY" $SSHOPTS -P 2222 "$HERE/harness.c" root@localhost:/tmp/atrium_fs_harness.c

# 3. Link + build harness + run, in the VM.
echo "==> linking + running in VM"
GOT=$(ssh -i "$KEY" $SSHOPTS -p 2222 root@localhost \
  'cd /tmp && cc -shared -o atrium_fs.so atrium_fs_freebsd.o \
   && cc -o harness atrium_fs_harness.c \
   && ./harness ./atrium_fs.so')
echo "    got RGBA:      $GOT"

# 4. Compare.
if [ "$GOT" = "$EXPECTED" ]; then
  echo "==> PASS — bespoke ELF output runs correctly on FreeBSD aarch64"
  exit 0
else
  echo "==> FAIL — expected [$EXPECTED], got [$GOT]"
  exit 1
fi
