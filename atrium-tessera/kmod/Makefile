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
# sys/crypto/armv8 (see sys/modules/armv8crypto/Makefile). One kernel-build
# flag has to come off for it:
#   -mgeneral-regs-only  is how the kernel FORBIDS FP/SIMD registers, so this
#                        translation unit is the sanctioned exception — and the
#                        reason b3_shim.c must bracket its calls with
#                        fpu_kern_enter/leave (#94).
#
# It also needs the compiler's own arm_neon.h, which -nostdinc hides. We do NOT
# drop -nostdinc to get it: that exposes the compiler's whole include dir, and
# when CROSS-BUILDING from macOS the host clang's stdint.h then collides with
# the kernel's types (int_fast16_t as int16_t vs FreeBSD's __int_fast16_t as
# int, -Werror, build dies). In-VM it happens to work only because the compiler
# is FreeBSD's own clang. Instead keep -nostdinc and append JUST the resource
# dir with -idirafter: arm_neon.h lives only there so it is still found, while
# every header FreeBSD also provides is resolved from the -I paths first.
# It also needs the compiler's own arm_neon.h, which -nostdinc hides. We keep
# -nostdinc and append JUST the resource dir with -idirafter, plus neon-compat/
# ahead of it to satisfy arm_neon.h's `#include <stdint.h>` with the kernel's
# types. See neon-compat/stdint.h for why dropping -nostdinc (FreeBSD's own
# armv8crypto pattern) cannot work across two different toolchains.
NEON_RESOURCE_DIR!=	${CC} -print-resource-dir 2>/dev/null || echo /nonexistent
b3_blake3_neon.o: b3_blake3_neon.c
	${CC} -c ${CFLAGS:N-mgeneral-regs-only} -I${.CURDIR}/neon-compat \
	    -idirafter ${NEON_RESOURCE_DIR}/include ${WERROR} ${PROF} ${.IMPSRC}
	${CTFCONVERT_CMD}
.endif

.include <bsd.kmod.mk>
