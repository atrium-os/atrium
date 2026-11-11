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

# ★ D3 (kernel security review 2026-11-07): the on-disk decoders are Rust.
# tessera_decode.rs is the crate root for ../rs/tessera-decode/src; built by
# freebsd-kpi-rs's kmod.rust.mk (sys/rust in the source tree, our patches in
# kmod-rust-spike/kpi-patches; scripts/tessera-kmod-rust.sh sets it up). With
# TESSERA_RUST_DECODE the C decoders in codec.c, manifest.c, pack.c, btree.c
# and journal.c become calls into it (../core/src/tessera_rs.h).
#
# TESSERA_C_DECODE=1 builds the old all-C module, for comparison runs only.
.if !defined(TESSERA_C_DECODE) || empty(TESSERA_C_DECODE)
CFLAGS+=    -DTESSERA_RUST_DECODE
RUST_SRCS=  tessera_decode.rs
.endif

# Cross-language LTO (TESSERA_LTO=1, scripts/tessera-kmod-rust.sh lto): C and
# Rust both become LLVM bitcode and are optimised together when ld.lld links
# the module (the -r link in kmod.mk), so the decoders can inline into the C
# that calls them. CC and ld.lld must be the Rust toolchain's LLVM version.
# The SIMD files below opt out: their functions run inside fpu_kern regions
# only, and keeping them separate objects keeps vector code from moving.
.if defined(TESSERA_LTO) && !empty(TESSERA_LTO)
CFLAGS+=    -flto=thin
.endif

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

# ★ #114: kernel stack-frame ceiling.
#
# FreeBSD's kstack is 4 pages (16 KiB) and the GC read path nests deeply
# (gc_data_zone_ex -> _GC_WALK_RECORD -> fetch_blob -> fetch_blob_ex ->
# btree_get). Three separate 4-8 KiB stack objects on that path overflowed it,
# and an arm64 stack overflow does NOT panic cleanly: it faults in a loop, so
# the thread pins a CPU at 100% with no console output and cannot be killed or
# even stopped for ddb. It took a long investigation to identify precisely
# because the symptom looks like an infinite loop in code that cannot loop.
#
# WARNING, not error, deliberately: six functions still exceed this and fixing
# them is separate work (see scratchpad frame-sweep logs / task #114). The point
# is that any NEW oversized frame shows up in the build output immediately.
# scripts/check-frames.sh ratchets the count so the list cannot grow silently.
#
# When a frame exceeds this and the declarations do not explain it, ask the
# compiler rather than reading the source:
#   -Rpass-analysis=stack-frame-layout   which slot
#   -Rpass=inline                        which callees were inlined in
#   __noinline on a callee + re-measure  attributes the delta
CFLAGS+=    -Wframe-larger-than=1024
# The kmod builds -Werror, so the line above would HARD-FAIL the build on the
# eleven frames that already exceed the limit. Downgrade just this one to a
# warning; scripts/check-frames.sh is what turns growth into a failure.
CFLAGS+=    -Wno-error=frame-larger-than
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
	${CC} -c ${CFLAGS:N-mgeneral-regs-only:N-flto*} -I${.CURDIR}/neon-compat \
	    -idirafter ${NEON_RESOURCE_DIR}/include ${WERROR} ${PROF} ${.IMPSRC}
	${CTFCONVERT_CMD}
.endif

# BLAKE3 SSE4.1 + AVX2 in the kernel, amd64.
#
# Not architecturally guaranteed (unlike NEON on ARMv8), so b3_shim.c picks
# the backend at RUNTIME: SSE4.1 from cpu_feature2, AVX2 only when the CPU has
# it AND the kernel enabled YMM state in XSAVE (fpu_kern can only preserve
# what XSAVE covers). Same sliced fpu_kern regions as NEON (#94). Measured on
# a Xeon host in userspace: portable 605, SSE4.1 1251, AVX2 1979 MiB/s on
# 64 KiB blobs. Digests byte-identical at every level (core test_b3_simd).
#
# The two backend files are the only translation units built with SSE/AVX
# allowed, so nothing else in the module can touch vector state outside an
# fpu_kern region. The kernel's -mno-sse/-mno-avx*/-msoft-float/-mno-mmx come
# off just for them; immintrin.h comes from the compiler's resource dir via
# -idirafter (same reason as the NEON rule above), and neon-compat/ supplies
# its <stdint.h>.
.if ${MACHINE_ARCH} == "amd64"
CFLAGS+=    -DBLAKE3_USE_X86=1
OBJS+=      b3_blake3_sse41.o b3_blake3_avx2.o
X86_RESOURCE_DIR!=	${CC} -print-resource-dir 2>/dev/null || echo /nonexistent
X86_SIMD_CFLAGS=	${CFLAGS:N-mno-sse*:N-mno-avx*:N-mno-mmx:N-msoft-float:N-mno-aes:N-flto*} \
		-I${.CURDIR}/neon-compat -idirafter ${X86_RESOURCE_DIR}/include \
		-Wno-cast-qual -Wno-cast-align	# vendored verbatim (upstream casts)
b3_blake3_sse41.o: b3_blake3_sse41.c
	${CC} -c ${X86_SIMD_CFLAGS} -msse4.1 ${WERROR} ${PROF} ${.IMPSRC}
	${CTFCONVERT_CMD}
b3_blake3_avx2.o: b3_blake3_avx2.c
	${CC} -c ${X86_SIMD_CFLAGS} -mavx2 ${WERROR} ${PROF} ${.IMPSRC}
	${CTFCONVERT_CMD}
.endif

.include <bsd.kmod.mk>

# The Rust half of TESSERA_LTO: every crate (core, kpi, tessera_decode) as
# bitcode for the linker, in place of rustc's own thin LTO. After the include:
# kern.rust.mk sets RUSTFLAGS and the rules expand it when they run.
#
# -strict-align: the aarch64-unknown-none-softfloat target turns it on and C
# does not, and LLVM inlines only where the callee's features are a subset of
# the caller's, so with it no decoder inlines into C. The kernel's C already
# assumes unaligned loads work.
#
# ld -r keeps every global symbol, so linking rustroot.a's bitcode as it is
# would carry all of core and kpi into the module (+260 KB of text). Instead
# its crates are merged into rustroot-lto.o and everything but the
# tessera_rs_* entry points is internalised and dropped before the link; the
# C side then sees the same symbols the non-LTO staticlib exports.
# (compiler_builtins is an empty native object and is left out.)
.if defined(TESSERA_LTO) && !empty(TESSERA_LTO) && defined(RUST_SRCS)
RUSTFLAGS:= ${RUSTFLAGS:N-Clto=*} -Clinker-plugin-lto -Ctarget-feature=-strict-align
TESSERA_LLVMBIN?=	/usr/bin
RUST_OBJS:=	${RUST_OBJS:N${RUSTROOT_A}} rustroot-lto.o
${KMOD}.kld ${FULLPROG}: rustroot-lto.o
rustroot-lto.o: ${RUSTROOT_A}
	rm -rf rustroot-lto.d && mkdir rustroot-lto.d
	(cd rustroot-lto.d && ${TESSERA_LLVMBIN}/llvm-ar x ../${RUSTROOT_A})
	${TESSERA_LLVMBIN}/llvm-link -o rustroot-lto.d/all.bc \
	    `for f in rustroot-lto.d/*.o; do \
	        [ "$$(head -c2 $$f)" = BC ] && echo $$f; \
	    done`
	${TESSERA_LLVMBIN}/opt -passes=internalize,globaldce -thinlto-bc \
	    -internalize-public-api-list='tessera_rs_*' \
	    -o ${.TARGET} rustroot-lto.d/all.bc
CLEANFILES+=	rustroot-lto.o
CLEANDIRS+=	rustroot-lto.d
.endif
