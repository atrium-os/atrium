# tessera_fs kernel module Makefile (FreeBSD bsd.kmod.mk).
#
# Phase 4 round 1: real file-backed mount; root vnode synthesized.
# Subsequent rounds add vop_lookup over the real inode tree, vop_read
# over manifest+pack walk, mutation paths, and the journal-replay-on-
# mount step.

KMOD=       tessera_fs

# Sources: kmod glue + the tessera-core primitives that don't allocate
# heap memory (so they build cleanly in -nostdinc kernel mode without
# further refactoring).
.PATH:      ${.CURDIR} ${.CURDIR}/../core/src

SRCS=       tessera_fs.c \
            error.c \
            crc.c \
            codec.c \
            btree.c \
            manifest.c \
            hash.c \
            b3_blake3.c \
            b3_blake3_portable.c \
            b3_shim.c \
            extent.c \
            pack.c \
            journal.c \
            quota.c \
            quota_store.c \
            vnode_if.h

CFLAGS+=    -I${.CURDIR}/../core/include
CFLAGS+=    -I${.CURDIR}/../core/src
CFLAGS+=    -DTESSERA_KERNEL=1

# ★ #94: BLAKE3 NEON in the kernel, arm64 only.
#
# NEON/ASIMD is MANDATORY in ARMv8-A, so there is no runtime feature test to
# get wrong — if we are building for aarch64, the instructions are there.
# b3_shim.c slices the hash into one fpu_kern region per 64 KiB so the
# non-preemptible window stays ~32 us (see the rationale there); measured
# 1.9x on the hash, which lifts the read-verification ceiling from ~1150 to
# ~2100 MiB/s. Digests are byte-identical to the portable backend (#89),
# which is the property that makes this safe to switch on at all.
.if ${MACHINE_ARCH} == "aarch64"
CFLAGS+=    -DBLAKE3_USE_NEON=1
OBJS+=      b3_blake3_neon.o

# The NEON file needs its own rule, following the pattern FreeBSD uses for
# sys/crypto/armv8 (see sys/modules/armv8crypto/Makefile). Two kernel-build
# flags have to come off for it:
#   -nostdinc            hides the compiler's own arm_neon.h intrinsics header
#   -mgeneral-regs-only  is how the kernel FORBIDS FP/SIMD registers, so this
#                        translation unit is the sanctioned exception — and the
#                        reason b3_shim.c must bracket its calls with
#                        fpu_kern_enter/leave (#94).
b3_blake3_neon.o: b3_blake3_neon.c
	${CC} -c ${CFLAGS:N-nostdinc:N-mgeneral-regs-only} ${WERROR} ${PROF} 	    ${.IMPSRC}
	${CTFCONVERT_CMD}
.endif

.include <bsd.kmod.mk>
