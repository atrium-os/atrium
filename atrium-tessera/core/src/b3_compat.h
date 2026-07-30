/*
 * b3_compat.h — freestanding-build shim for the vendored BLAKE3
 * reference C implementation (b3_blake3*.c/h).
 *
 * The upstream files include hosted libc headers; this header replaces
 * them so the same sources build in the kernel (TESSERA_KERNEL,
 * -nostdinc) and in the userspace core library.
 *
 * SIMD policy (revised, #94). This header used to assert "portable only,
 * because kernel NEON would require fpu_kern_enter() around every hash".
 * That rationale was never measured and is quantitatively WRONG:
 * fpu_kern_enter+leave costs 31 ns/pair, i.e. 0.05% of a 64 KiB hash. The
 * real objection was never the call cost but the REGION LENGTH — an
 * FPU_KERN_NOCTX region is non-preemptible, and wrapping a whole 1 MiB blob
 * would block a 2.67 ms audio quantum for ~486 us. b3_shim.c therefore
 * slices the hash, one region per 64 KiB (~32 us), and NEON is enabled for
 * the aarch64 kernel build (see kmod/Makefile).
 *
 * x86 SIMD stays off: those backends are not vendored, and unlike NEON on
 * ARMv8 they are not architecturally guaranteed, so they would need runtime
 * feature detection.
 */

#ifndef B3_COMPAT_H_
#define B3_COMPAT_H_

/* Default the SIMD backends off before b3_blake3_impl.h autodetects, and let
 * the build opt IN. The aarch64 kernel build now passes -DBLAKE3_USE_NEON=1
 * (sliced regions landed, #94); the userspace bench passes it too; every
 * other build still gets 0. */
#ifndef BLAKE3_USE_NEON
#define BLAKE3_USE_NEON 0
#endif
#define BLAKE3_NO_SSE2 1
#define BLAKE3_NO_SSE41 1
#define BLAKE3_NO_AVX2 1
#define BLAKE3_NO_AVX512 1

#ifdef TESSERA_KERNEL
#  include <sys/param.h>
#  include <sys/systm.h>	/* memcpy/memset/memcmp; bool via sys/types.h */
#  ifndef assert
#    define assert(x) do { (void)(x); } while (0)	/* keep operands "used" */
#  endif
#else
#  include <assert.h>
#  include <stdbool.h>
#  include <stddef.h>
#  include <stdint.h>
#  include <string.h>
#endif

#endif /* B3_COMPAT_H_ */
