/*
 * b3_shim.c — portable-only dispatch for the vendored BLAKE3 reference
 * implementation, plus the tessera-facing one-shot entry point.
 *
 * Upstream splits compression backends behind blake3_dispatch.c (CPUID
 * feature detection, SIMD units). We deliberately do not vendor that:
 * every backend but portable touches vector state, which in the kernel
 * would require fpu_kern_enter() around each call. The portable
 * compression function runs on plain integer registers on every
 * architecture — that property is why BLAKE3 was chosen (see the
 * hash_alg format discussion). So the dispatch layer collapses to
 * direct calls into b3_blake3_portable.c.
 */

#include "b3_compat.h"
/* Any SIMD backend compiled in? In the kernel it needs fpu_kern regions. */
#if (defined(BLAKE3_USE_NEON) && BLAKE3_USE_NEON) || BLAKE3_USE_X86
#define TESSERA_B3_SIMD 1
#else
#define TESSERA_B3_SIMD 0
#endif
#if defined(TESSERA_KERNEL) && TESSERA_B3_SIMD
#include <sys/proc.h>		/* curthread */
#if BLAKE3_USE_X86
#include <machine/fpu.h>	/* fpu_kern_enter/leave */
#include <machine/md_var.h>	/* cpu_feature2, cpu_stdext_feature, xsave */
#include <machine/specialreg.h>
#else
#include <machine/vfp.h>	/* fpu_kern_enter/leave (#94) */
#endif
#endif
#include "b3_blake3.h"
#include "b3_blake3_impl.h"

#include "tessera/hash.h"
#include "tessera_compat.h"

#if BLAKE3_USE_X86
/*
 * amd64 runtime dispatch: 0 = portable, 1 = SSE4.1, 2 = AVX2. Detected once
 * (racing first callers compute the same value, so the lazy init is benign).
 * tessera_b3_simd_limit caps it, which is how the digest-equality test runs
 * every backend on the same machine.
 */
static int b3_x86_level = -1;
static int b3_x86_limit = 2;

static int
b3_x86_detect(void)
{
	int l = 0;
#ifdef TESSERA_KERNEL
	if (cpu_feature2 & CPUID2_SSE41)
		l = 1;
	/* AVX2 needs the CPU bit AND the OS saving YMM state: fpu_kern only
	 * preserves what XSAVE is enabled for. */
	if (l == 1 && (cpu_stdext_feature & CPUID_STDEXT_AVX2) &&
	    use_xsave && (xsave_mask & XFEATURE_AVX) == XFEATURE_AVX)
		l = 2;
#else
	__builtin_cpu_init();
	if (__builtin_cpu_supports("sse4.1"))
		l = 1;
	if (l == 1 && __builtin_cpu_supports("avx2"))
		l = 2;
#endif
	return (l);
}

static int
b3_level(void)
{
	int l = b3_x86_level;

	if (l < 0)
		b3_x86_level = l = b3_x86_detect();
	return (l < b3_x86_limit ? l : b3_x86_limit);
}

int
tessera_b3_simd_limit(int limit)
{
	if (limit >= 0)
		b3_x86_limit = limit;
	return (b3_level());
}
#else
int
tessera_b3_simd_limit(int limit)
{
	(void)limit;
	return (TESSERA_B3_SIMD);
}
#endif

void
blake3_compress_in_place(uint32_t cv[8], const uint8_t block[BLAKE3_BLOCK_LEN],
    uint8_t block_len, uint64_t counter, uint8_t flags)
{
#if BLAKE3_USE_X86
	if (b3_level() >= 1) {
		blake3_compress_in_place_sse41(cv, block, block_len, counter,
		    flags);
		return;
	}
#endif
	blake3_compress_in_place_portable(cv, block, block_len, counter,
	    flags);
}

void
blake3_compress_xof(const uint32_t cv[8], const uint8_t block[BLAKE3_BLOCK_LEN],
    uint8_t block_len, uint64_t counter, uint8_t flags, uint8_t out[64])
{
#if BLAKE3_USE_X86
	if (b3_level() >= 1) {
		blake3_compress_xof_sse41(cv, block, block_len, counter, flags,
		    out);
		return;
	}
#endif
	blake3_compress_xof_portable(cv, block, block_len, counter, flags,
	    out);
}

void
blake3_xof_many(const uint32_t cv[8], const uint8_t block[BLAKE3_BLOCK_LEN],
    uint8_t block_len, uint64_t counter, uint8_t flags, uint8_t out[64],
    size_t outblocks)
{
	for (size_t i = 0; i < outblocks; i++) {
		blake3_compress_xof(cv, block, block_len, counter + i,
		    flags, out + 64 * i);
	}
}

void
blake3_hash_many(const uint8_t *const *inputs, size_t num_inputs,
    size_t blocks, const uint32_t key[8], uint64_t counter,
    bool increment_counter, uint8_t flags, uint8_t flags_start,
    uint8_t flags_end, uint8_t *out)
{
#if BLAKE3_USE_NEON
	/* NEON hashes MAX_SIMD_DEGREE chunks in parallel; compress_in_place
	 * and compress_xof stay portable (upstream's NEON backend provides
	 * only hash_many). */
	blake3_hash_many_neon(inputs, num_inputs, blocks, key, counter,
	    increment_counter, flags, flags_start, flags_end, out);
#elif BLAKE3_USE_X86
	switch (b3_level()) {
	case 2:
		blake3_hash_many_avx2(inputs, num_inputs, blocks, key, counter,
		    increment_counter, flags, flags_start, flags_end, out);
		break;
	case 1:
		blake3_hash_many_sse41(inputs, num_inputs, blocks, key, counter,
		    increment_counter, flags, flags_start, flags_end, out);
		break;
	default:
		blake3_hash_many_portable(inputs, num_inputs, blocks, key,
		    counter, increment_counter, flags, flags_start, flags_end,
		    out);
	}
#else
	blake3_hash_many_portable(inputs, num_inputs, blocks, key, counter,
	    increment_counter, flags, flags_start, flags_end, out);
#endif
}

size_t
blake3_simd_degree(void)
{
	return (MAX_SIMD_DEGREE);
}

/*
 * One-shot 256-bit BLAKE3 of a contiguous buffer.
 *
 * ★ #94: when the NEON backend is compiled into the KERNEL, the update is
 * SLICED — one fpu_kern_enter/leave per TESSERA_B3_FPU_SLICE of input —
 * rather than one region around the whole blob.
 *
 * Why slicing rather than one big region: FPU_KERN_NOCTX regions are
 * NON-PREEMPTIBLE, and Atrium has hard real-time deadline lanes (lyrad's
 * audio quantum is 2.67 ms at 48 kHz / 128 frames). At the MEASURED 1.9x
 * NEON rate an unsliced 1 MiB blob is ~486 us of non-preemptible time —
 * ~18% of that quantum, which is not acceptable for a filesystem read. A
 * 64 KiB slice is ~32 us, which is.
 *
 * Why slicing is CORRECT: BLAKE3's incremental state lives in the hasher
 * struct in ordinary memory, not in FPU registers, so dropping and
 * re-taking the FPU between updates cannot lose state. Verified the strong
 * way rather than argued — digests are byte-identical to the portable
 * backend at 4/64/128/1024 KiB (#89), and a faster-but-different hash would
 * silently rewrite every content address in the store.
 *
 * Why it cannot sleep: FPU_KERN_NOCTX forbids sleeping, and the region here
 * contains only blake3_hasher_update over an already-resident kernel buffer
 * — no allocation, no locks, no faults. The hazard is excluded by the shape
 * of the call, not by an audit that could rot.
 *
 * fpu_kern_enter+leave measured at 31 ns/pair, i.e. 0.05% of a 64 KiB hash
 * (#89), so the slicing overhead is noise. That measurement is also what
 * refuted the original "NEON would need fpu_kern_enter around every hash"
 * objection: the cost was never the problem, the region length was.
 */
#if defined(TESSERA_KERNEL) && TESSERA_B3_SIMD
#define TESSERA_B3_FPU_SLICE	(64u * 1024u)

static void
b3_hash_with(blake3_hasher *hasher, const uint8_t *data, size_t len,
    tessera_hash_t out)
{
	size_t off = 0;

	blake3_hasher_init(hasher);
	while (off < len) {
		size_t n = len - off;

		if (n > TESSERA_B3_FPU_SLICE)
			n = TESSERA_B3_FPU_SLICE;
		fpu_kern_enter(curthread, NULL, FPU_KERN_NORMAL | FPU_KERN_NOCTX);
		blake3_hasher_update(hasher, data + off, n);
		(void)fpu_kern_leave(curthread, NULL);
		off += n;
	}
	/* finalize touches the same SIMD compression path */
	fpu_kern_enter(curthread, NULL, FPU_KERN_NORMAL | FPU_KERN_NOCTX);
	blake3_hasher_finalize(hasher, out, TESSERA_HASH_SIZE);
	(void)fpu_kern_leave(curthread, NULL);
}
#else
static void
b3_hash_with(blake3_hasher *hasher, const uint8_t *data, size_t len,
    tessera_hash_t out)
{
	blake3_hasher_init(hasher);
	blake3_hasher_update(hasher, data, len);
	blake3_hasher_finalize(hasher, out, TESSERA_HASH_SIZE);
}
#endif

#ifdef TESSERA_KERNEL
/*
 * ★ T2: the hasher is 1.9 KiB (its chaining-value stack is 55 x 32 bytes)
 * and it sat in this frame at the bottom of the deepest write path
 * (flush -> append -> fetch -> verify -> hash), where a timer interrupt on
 * top overran the 16 KiB kernel stack under KASAN (kmod-audit T2). So it
 * comes from the heap. M_NOWAIT, because callers may hold a mutex; if that
 * fails the hash still happens, on the stack, in a frame of its own that
 * the normal path never enters.
 */
static __noinline void
b3_hash_on_stack(const uint8_t *data, size_t len, tessera_hash_t out)
{
	blake3_hasher hasher;

	b3_hash_with(&hasher, data, len, out);
}

void
tessera_blake3_256(const uint8_t *data, size_t len, tessera_hash_t out)
{
	blake3_hasher *hasher = malloc(sizeof *hasher, M_TESSERA, M_NOWAIT);

	if (hasher == NULL) {
		b3_hash_on_stack(data, len, out);
		return;
	}
	b3_hash_with(hasher, data, len, out);
	free(hasher, M_TESSERA);
}
#else
void
tessera_blake3_256(const uint8_t *data, size_t len, tessera_hash_t out)
{
	blake3_hasher hasher;

	b3_hash_with(&hasher, data, len, out);
}
#endif
