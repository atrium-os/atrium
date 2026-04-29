# tessera_fs kernel module Makefile (FreeBSD bsd.kmod.mk).
#
# Phase 0: skeleton only — module load/unload no-ops.
# Phase 4 fills in the VFS ops and wires the in-kernel core.

KMOD=       tessera_fs
SRCS=       tessera_fs.c \
            vnode_if.h

# Pick up tessera-core headers + canonical codec sources.
CFLAGS+=    -I${.CURDIR}/../core/include
CFLAGS+=    -DTESSERA_KERNEL=1

# Phase 1+: link against tessera-core .o objects compiled with TESSERA_KERNEL.
# For Phase 0 the kmod is self-contained.

.include <bsd.kmod.mk>
