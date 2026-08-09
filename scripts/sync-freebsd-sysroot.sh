#!/bin/sh
# Stage FreeBSD target headers/libs into a cross sysroot on the HOST, so
# userspace pieces (libtessera_core.a and the Rust tools that link it) can be
# cross-built here instead of inside the VM.
#
# Why this exists. The tree carried a rule that libtessera_core.a "must be
# rebuilt IN-VM", justified by macOS `ar` mangling ELF objects. That is true of
# macOS's ar, but it indicts the archiver only — the cross toolchain ships
# llvm-ar, and the kmod has always cross-compiled here. The real blocker was
# headers: the obj-tree sysroot holds only what the KERNEL build installed
# (55 dirs, no stdlib.h), and `make includes` dies on macOS in the install step
# with _INCSINS error 64. So take the headers from the running guest, which is
# built from this very tree — no version skew by construction.
#
# One-time (re-run after a world update):  sh scripts/sync-freebsd-sysroot.sh
set -eu
BSD_DIR="$(cd "$(dirname "$0")/.." && pwd)"
DEST="${TESSERA_CROSS_SYSROOT:-$BSD_DIR/.bootstrap-state/freebsd-sysroot}"
SSH_KEY="${SSH_KEY:-$HOME/.ssh/fresco_bsd_ed25519}"
PORT="${PORT:-2222}"

echo "pulling target headers from the guest into $DEST"
mkdir -p "$DEST/usr"
ssh -i "$SSH_KEY" -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null \
    -o LogLevel=ERROR -o BatchMode=yes -p "$PORT" root@localhost \
    'tar czf - -C /usr include lib/libmd.a lib/libc.a 2>/dev/null' \
  | tar xzf - -C "$DEST/usr"

[ -f "$DEST/usr/include/stdlib.h" ] || { echo "FAILED: no stdlib.h in $DEST" >&2; exit 1; }
echo "  $(ls "$DEST/usr/include" | wc -l | tr -d ' ') header entries staged"
echo "  now: sh scripts/cross-build-core.sh"
