/*
 * tessera/hash.h — SHA-256 dispatch.
 *
 * Single-stream HW-accelerated SHA-256. The userspace build uses
 * libmd's SHA256_*; the kernel build uses <sys/sha256.h>. Both
 * auto-dispatch to ARMv8 SHA-2 / Intel SHA-NI on hosts that
 * support them.
 *
 * For bulk parallelism (chunking, scrub, repack), call this
 * function from multiple threads — one HW SHA stream per core.
 * Multi-buffer SIMD batching is reserved for v2 (impl-plan §7.4).
 */

#ifndef TESSERA_HASH_H_
#define TESSERA_HASH_H_

#ifdef _KERNEL
#  include <sys/types.h>
#else
#  include <stdint.h>
#  include <stddef.h>
#endif
#include "tessera/format.h"

#ifdef __cplusplus
extern "C" {
#endif

/* One-shot hash of a contiguous buffer. */
void tessera_sha256(const uint8_t *data, size_t len, tessera_hash_t out);

/* Streaming hash for content that doesn't fit in memory. */
typedef struct tessera_sha256_ctx tessera_sha256_ctx_t;

tessera_sha256_ctx_t *tessera_sha256_init(void);
void tessera_sha256_update(tessera_sha256_ctx_t *, const uint8_t *data, size_t len);
void tessera_sha256_final(tessera_sha256_ctx_t *, tessera_hash_t out);
void tessera_sha256_free(tessera_sha256_ctx_t *);

/*
 * One-shot 256-bit BLAKE3 (portable compression only — plain integer
 * registers, no FPU/SIMD state on any architecture). Backend for
 * TESSERA_HASH_ALG_BLAKE3_256; implementation is the vendored
 * b3_blake3*.c.
 */
void tessera_blake3_256(const uint8_t *data, size_t len, tessera_hash_t out);

/*
 * Content-hash dispatch keyed on the volume's sb.hash_alg
 * (TESSERA_HASH_ALG_*). All content addressing must go through this
 * (or match it); callers guarantee `alg` was validated at mount or
 * format time.
 */
void tessera_content_hash(uint32_t alg, const uint8_t *data, size_t len,
                          tessera_hash_t out);

/* Compare two hashes for equality. Constant-time. */
int  tessera_hash_equal(const tessera_hash_t a, const tessera_hash_t b);

/* True iff hash is all-zero (the "null hash" sentinel). */
int  tessera_hash_is_null(const tessera_hash_t h);

#ifdef __cplusplus
}
#endif

#endif /* TESSERA_HASH_H_ */
