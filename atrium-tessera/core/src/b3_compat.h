/*
 * b3_compat.h — freestanding-build shim for the vendored BLAKE3
 * reference C implementation (b3_blake3*.c/h).
 *
 * The upstream files include hosted libc headers; this header replaces
 * them so the same sources build in the kernel (TESSERA_KERNEL,
 * -nostdinc) and in the userspace core library.
 *
 * Portable-only policy: SIMD backends are never compiled. In the
 * kernel, NEON/SSE would require fpu_kern_enter() around every hash;
 * the portable compression function needs no FPU state and is the
 * whole point of adopting BLAKE3 (see docs — hash_alg format field).
 */

#ifndef B3_COMPAT_H_
#define B3_COMPAT_H_

/* Force the portable code paths before b3_blake3_impl.h autodetects. */
#define BLAKE3_USE_NEON 0
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
