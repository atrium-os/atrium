#!/bin/sh
# Attach aarch64-elf-gdb to the running QEMU's kernel.
#
# Prereqs:
#   - VM started with scripts/run-vm.sh --kgdb (opens stub on
#     127.0.0.1:1234).
#   - aarch64-elf-gdb installed (brew install aarch64-elf-gdb).
#   - LAMINAR-DEV kernel built in the VM (writes the kernel.full
#     under /usr/obj/usr/src/arm64.aarch64/sys/LAMINAR-DEV/,
#     visible from host via the 9p share at
#     ~/src/bsd/freebsd-src/usr/obj/...).
#
# Usage:
#   scripts/kgdb-attach.sh                  # attach to LAMINAR-DEV
#   scripts/kgdb-attach.sh ULE              # attach to GENERIC
#   scripts/kgdb-attach.sh <full-path>      # explicit kernel.full
#
# Hints once attached:
#   (gdb) bt                    backtrace at current pc
#   (gdb) b sched_laminar_setcpu set breakpoint at function
#   (gdb) b sched_laminar.c:340 set breakpoint at file:line
#   (gdb) c                     continue execution
#   (gdb) Ctrl-C                interrupt running guest
#   (gdb) p curthread->td_critnest
#   (gdb) info threads          (limited; gdb sees vCPUs as threads)
#   (gdb) quit / detach
#
# Tips:
#   - QEMU's stub is a "system" gdb stub: it sees vCPUs as gdb
#     threads (one per -smp count).  It does NOT see kernel-level
#     struct thread; we walk those via expressions.
#   - Hardware watchpoints work (HVF passes them through).
#   - Use Ctrl-C in gdb to interrupt; (gdb) c to resume.

set -eu

KERN_NAME="${1:-LAMINAR-DEV}"
BSD_DIR="$(cd "$(dirname "$0")/.." && pwd)"

case "$KERN_NAME" in
    /*)
        KERN_FULL="$KERN_NAME"
        ;;
    *)
        # The VM builds kernel.full to /usr/obj/usr/src/arm64.aarch64/
        # sys/<KERN_NAME>/kernel.full on its local ZFS.  We need that file
        # on the host for cross-gdb.  Mirror it to vm/kgdb-symbols/.
        SYMDIR="$BSD_DIR/vm/kgdb-symbols"
        KERN_FULL="$SYMDIR/$KERN_NAME-kernel.full"
        if [ ! -f "$KERN_FULL" ] || [ -n "${KGDB_REFRESH:-}" ]; then
            mkdir -p "$SYMDIR"
            VSSH="$BSD_DIR/scripts/vssh"
            VM_PATH="/usr/obj/usr/src/arm64.aarch64/sys/$KERN_NAME/kernel.full"
            echo "kgdb: copying $VM_PATH from VM (one-time per build)..."
            "$VSSH" "cp $VM_PATH /mnt/host/vm/kgdb-symbols/$KERN_NAME-kernel.full"
        fi
        ;;
esac

if [ ! -f "$KERN_FULL" ]; then
    echo "error: kernel.full not found at $KERN_FULL" >&2
    echo "       hint: build kernel in VM, then re-run.  Set" >&2
    echo "       KGDB_REFRESH=1 to force re-copy after rebuild." >&2
    exit 1
fi

if ! command -v aarch64-elf-gdb >/dev/null 2>&1; then
    echo "error: aarch64-elf-gdb not installed" >&2
    echo "       brew install aarch64-elf-gdb" >&2
    exit 1
fi

# Pre-flight: is QEMU listening?
if ! nc -z 127.0.0.1 1234 2>/dev/null; then
    echo "error: nothing listening on 127.0.0.1:1234" >&2
    echo "       hint: did you start the VM with --kgdb?" >&2
    exit 1
fi

echo "kgdb: attaching to running kernel via 127.0.0.1:1234"
echo "kgdb: symbols from $KERN_FULL"

exec aarch64-elf-gdb \
    -ex "set architecture aarch64" \
    -ex "set pagination off" \
    -ex "target remote 127.0.0.1:1234" \
    "$KERN_FULL"
