/*
 * tessera-core: SHA-256 dispatch (single-stream HW-accelerated).
 *
 * Userspace path uses libmd's SHA256_*; in-kernel build uses
 * <sys/sha256.h>. Both auto-dispatch to ARMv8 SHA-2 / Intel SHA-NI
 * on hosts that support the extensions.
 *
 * Multi-buffer SIMD batching is reserved for v2 (impl-plan §7.4).
 * For bulk parallelism, callers fan out across cores; each thread
 * uses single-stream HW SHA on its own core.
 */

#include "tessera/hash.h"
#include "tessera/error.h"

#include <stdlib.h>
#include <string.h>

#ifdef TESSERA_KERNEL
#  include <sys/types.h>
#  include <sys/systm.h>
#  include <sys/malloc.h>
#  include <crypto/sha2/sha256.h>
   typedef SHA256_CTX tessera_sha_state;
#  define tessera_alloc(n)  malloc((n), M_TEMP, M_WAITOK)
#  define tessera_free_(p)  free((p), M_TEMP)
#  define SHA_INIT(ctx)     SHA256_Init(ctx)
#  define SHA_UPDATE(ctx,d,n)  SHA256_Update((ctx), (d), (n))
#  define SHA_FINAL(d,ctx)  SHA256_Final((d), (ctx))
#else
#  include <sha256.h>      /* libmd, FreeBSD base — HW-accelerated */
   typedef SHA256_CTX tessera_sha_state;
#  define tessera_alloc(n)  malloc(n)
#  define tessera_free_(p)  free(p)
#  define SHA_INIT(ctx)     SHA256_Init(ctx)
#  define SHA_UPDATE(ctx,d,n)  SHA256_Update((ctx), (d), (n))
#  define SHA_FINAL(d,ctx)  SHA256_Final((d), (ctx))
#endif

struct tessera_sha256_ctx {
	tessera_sha_state state;
};

void
tessera_sha256(const uint8_t *data, size_t len, tessera_hash_t out)
{
	tessera_sha_state ctx;
	SHA_INIT(&ctx);
	SHA_UPDATE(&ctx, data, len);
	SHA_FINAL(out, &ctx);
}

tessera_sha256_ctx_t *
tessera_sha256_init(void)
{
	tessera_sha256_ctx_t *c = tessera_alloc(sizeof(*c));
	if (c == NULL) return NULL;
	SHA_INIT(&c->state);
	return c;
}

void
tessera_sha256_update(tessera_sha256_ctx_t *c, const uint8_t *data, size_t len)
{
	if (c == NULL) return;
	SHA_UPDATE(&c->state, data, len);
}

void
tessera_sha256_final(tessera_sha256_ctx_t *c, tessera_hash_t out)
{
	if (c == NULL) {
		memset(out, 0, sizeof(tessera_hash_t));
		return;
	}
	SHA_FINAL(out, &c->state);
}

void
tessera_sha256_free(tessera_sha256_ctx_t *c)
{
	if (c != NULL) tessera_free_(c);
}

int
tessera_hash_equal(const tessera_hash_t a, const tessera_hash_t b)
{
	/* Constant-time compare. */
	uint8_t diff = 0;
	for (size_t i = 0; i < sizeof(tessera_hash_t); i++)
		diff |= (uint8_t)(a[i] ^ b[i]);
	return diff == 0;
}

int
tessera_hash_is_null(const tessera_hash_t h)
{
	uint8_t any = 0;
	for (size_t i = 0; i < sizeof(tessera_hash_t); i++)
		any |= h[i];
	return any == 0;
}
