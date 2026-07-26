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
#include "b3_blake3.h"
#include "b3_blake3_impl.h"

#include "tessera/hash.h"

void
blake3_compress_in_place(uint32_t cv[8], const uint8_t block[BLAKE3_BLOCK_LEN],
    uint8_t block_len, uint64_t counter, uint8_t flags)
{
	blake3_compress_in_place_portable(cv, block, block_len, counter,
	    flags);
}

void
blake3_compress_xof(const uint32_t cv[8], const uint8_t block[BLAKE3_BLOCK_LEN],
    uint8_t block_len, uint64_t counter, uint8_t flags, uint8_t out[64])
{
	blake3_compress_xof_portable(cv, block, block_len, counter, flags,
	    out);
}

void
blake3_xof_many(const uint32_t cv[8], const uint8_t block[BLAKE3_BLOCK_LEN],
    uint8_t block_len, uint64_t counter, uint8_t flags, uint8_t out[64],
    size_t outblocks)
{
	for (size_t i = 0; i < outblocks; i++) {
		blake3_compress_xof_portable(cv, block, block_len, counter + i,
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

/* One-shot 256-bit BLAKE3 of a contiguous buffer. */
void
tessera_blake3_256(const uint8_t *data, size_t len, tessera_hash_t out)
{
	blake3_hasher hasher;

	blake3_hasher_init(&hasher);
	blake3_hasher_update(&hasher, data, len);
	blake3_hasher_finalize(&hasher, out, TESSERA_HASH_SIZE);
}
