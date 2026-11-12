# hospes_trap.ko: syscall traps for Hospes (docs/spec/hospes.md §13 R1, step K1).
# Needs a kernel from atrium-os/freebsd-src with <sys/systrap.h>.
# Built by scripts/hospes-kmod.sh (buildkernel -DKERNFAST with LOCAL_MODULES).

KMOD=		hospes_trap
SRCS=		hospes_trap.c

# The range table and every check on it: Rust (kernel decision D2).
RUST_SRCS=	hospes_trap.rs

.include <bsd.kmod.mk>
