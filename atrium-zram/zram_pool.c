/*-
 * SPDX-License-Identifier: BSD-2-Clause
 *
 * zpool — packed object allocator for atrium_zram. See zram_pool.h.
 */

#ifdef _KERNEL
#include <sys/param.h>
#include <sys/systm.h>
#include <sys/kernel.h>
#include <sys/lock.h>
#include <sys/malloc.h>
#include <sys/mutex.h>
#include <sys/queue.h>
#include <vm/vm.h>
#include <vm/vm_extern.h>
#include <vm/uma.h>

MALLOC_DECLARE(M_ZRAM);
static uma_zone_t zp_hdr_zone;

/* Span memory: whole pages, contiguous KVA, wired. */
#define	ZP_PAGES_ALLOC(bytes)	kmem_malloc((bytes), M_NOWAIT | M_USE_RESERVE)
#define	ZP_PAGES_ALLOC_WAIT(bytes) kmem_malloc((bytes), M_WAITOK)
#define	ZP_PAGES_FREE(p, bytes)	kmem_free((p), (bytes))
#define	ZP_HDR_ALLOC()		uma_zalloc(zp_hdr_zone, M_NOWAIT)
#define	ZP_HDR_ALLOC_WAIT()	uma_zalloc(zp_hdr_zone, M_WAITOK)
#define	ZP_HDR_FREE(h)		uma_zfree(zp_hdr_zone, (h))
#define	ZP_ARRAY_ALLOC(bytes)	malloc((bytes), M_ZRAM, M_WAITOK | M_ZERO)
#define	ZP_ARRAY_FREE(p)	free((p), M_ZRAM)
#define	ZP_LOCK(zp)		mtx_lock((struct mtx *)(zp)->lock)
#define	ZP_UNLOCK(zp)		mtx_unlock((struct mtx *)(zp)->lock)
#else
#include "zpool_test_shim.h"
#endif

#include "zram_pool.h"

/* Span headers in reserve: enough for every reserve chunk plus slack. */
#define	ZP_HDR_RESERVE(n)	((n) * 2 + 64)

static inline int
zp_bit_test(const uint64_t *m, int i)
{
	return ((m[i >> 6] >> (i & 63)) & 1);
}

static inline void
zp_bit_set(uint64_t *m, int i)
{
	m[i >> 6] |= (uint64_t)1 << (i & 63);
}

static inline void
zp_bit_clear(uint64_t *m, int i)
{
	m[i >> 6] &= ~((uint64_t)1 << (i & 63));
}

/* First clear bit below n, or -1. */
static int
zp_bit_ffc(const uint64_t *m, int n)
{
	int w, b;

	for (w = 0; w * 64 < n; w++) {
		if (m[w] == ~(uint64_t)0)
			continue;
		b = __builtin_ctzll(~m[w]);
		if (w * 64 + b < n)
			return (w * 64 + b);
		return (-1);
	}
	return (-1);
}

int
zpool_class(size_t len)
{
	if (len > ZP_MAXOBJ)
		return (ZP_RAWCLASS);
	if (len == 0)
		len = 1;
	return ((int)((len + ZP_CLASS_STEP - 1) / ZP_CLASS_STEP) - 1);
}

int
zpool_init(struct zpool *zp, int reserve, void *lock)
{
	struct zclass *c;
	uint32_t size, k, best_k, n, waste, best_waste_ppm, ppm;
	int i;

#ifdef _KERNEL
	zp_hdr_zone = uma_zcreate("zram_span", sizeof(struct zspan), NULL,
	    NULL, NULL, NULL, UMA_ALIGN_PTR, 0);
#endif
	memset(zp, 0, sizeof(*zp));
	zp->lock = lock;
	LIST_INIT(&zp->spans);
	LIST_INIT(&zp->hdr_free);
	zp->hdr_target = ZP_HDR_RESERVE(reserve);
	for (i = 0; i <= ZP_NCLASS; i++) {
		c = &zp->cls[i];
		TAILQ_INIT(&c->partial);
		if (i == ZP_RAWCLASS) {
			c->size = ZP_PAGE;
			c->pages = 1;
			c->nobj = 1;
			continue;
		}
		size = (uint32_t)(i + 1) * ZP_CLASS_STEP;
		best_k = 1;
		best_waste_ppm = 1000000;
		/* Fewest pages that get within 1 % of the best tail waste. */
		for (k = 1; k <= ZP_MAXPAGES; k++) {
			n = k * ZP_PAGE / size;
			waste = k * ZP_PAGE - n * size;
			ppm = (uint32_t)((uint64_t)waste * 1000000 / (k * ZP_PAGE));
			if (ppm + 10000 < best_waste_ppm) {
				best_waste_ppm = ppm;
				best_k = k;
			}
		}
		c->size = size;
		c->pages = (uint16_t)best_k;
		c->nobj = (uint16_t)(best_k * ZP_PAGE / size);
	}
	zp->reserve_target = reserve;
	zp->reserve = ZP_ARRAY_ALLOC(sizeof(void *) * (reserve > 0 ? reserve : 1));
	zp->reserve_n = 0;
	zpool_refill(zp);
	return (zp->reserve_n == reserve && zp->hdr_n == zp->hdr_target ?
	    0 : ENOMEM);
}

/*
 * Span headers. UMA's own zone reserve did not hold up: every header
 * allocation is a reserve allocation, so steady churn drains it and the zone
 * then needs a fresh slab, M_NOWAIT, at the worst moment (40 failed swap-outs
 * in-VM once stores ran on 4 CPUs at once). So the pool keeps its own list of
 * spare headers, used when UMA says no and refilled with zpool_refill().
 */
static struct zspan *
zp_hdr_get(struct zpool *zp)
{
	struct zspan *sp;

	sp = ZP_HDR_ALLOC();
	if (sp == NULL && (sp = LIST_FIRST(&zp->hdr_free)) != NULL) {
		LIST_REMOVE(sp, all);
		zp->hdr_n--;
		zp->hdr_reserve_used++;
	}
	return (sp);
}

static void
zp_hdr_put(struct zpool *zp, struct zspan *sp)
{
	if (zp->hdr_n < zp->hdr_target) {
		LIST_INSERT_HEAD(&zp->hdr_free, sp, all);
		zp->hdr_n++;
	} else
		ZP_HDR_FREE(sp);
}

static void
zp_span_release(struct zpool *zp, struct zspan *sp)
{
	LIST_REMOVE(sp, all);
	zp->pool_bytes -= sp->bytes;
	zp->nspans--;
	if (sp->bytes == ZP_CHUNK && zp->reserve_n < zp->reserve_target)
		zp->reserve[zp->reserve_n++] = sp->base;
	else
		ZP_PAGES_FREE(sp->base, sp->bytes);
	zp_hdr_put(zp, sp);
}

void
zpool_fini(struct zpool *zp)
{
	struct zspan *sp;
	int i;

	while ((sp = LIST_FIRST(&zp->spans)) != NULL) {
		LIST_REMOVE(sp, all);
		ZP_PAGES_FREE(sp->base, sp->bytes);
		ZP_HDR_FREE(sp);
	}
	while ((sp = LIST_FIRST(&zp->hdr_free)) != NULL) {
		LIST_REMOVE(sp, all);
		ZP_HDR_FREE(sp);
	}
	zp->hdr_n = 0;
	for (i = 0; i < zp->reserve_n; i++)
		ZP_PAGES_FREE(zp->reserve[i], ZP_CHUNK);
	zp->reserve_n = 0;
	ZP_ARRAY_FREE(zp->reserve);
	zp->reserve = NULL;
#ifdef _KERNEL
	uma_zdestroy(zp_hdr_zone);
#endif
}

static struct zspan *
zp_span_new(struct zpool *zp, int ci)
{
	struct zclass *c = &zp->cls[ci];
	struct zspan *sp;
	uint32_t bytes = (uint32_t)c->pages * ZP_PAGE;
	char *base;
	int from_reserve = 0;

	if (zp->limit != 0 && zp->pool_bytes + bytes > zp->limit) {
		zp->fail_limit++;
		return (NULL);
	}
	sp = zp_hdr_get(zp);
	if (sp == NULL) {
		zp->fail_nomem++;
		return (NULL);
	}
	base = ZP_PAGES_ALLOC(bytes);
	if (base == NULL && zp->reserve_n > 0) {
		/* The page allocator said no: build the span from a chunk. */
		base = zp->reserve[--zp->reserve_n];
		bytes = ZP_CHUNK;
		from_reserve = 1;
		zp->reserve_used++;
	}
	if (base == NULL) {
		zp_hdr_put(zp, sp);
		zp->fail_nomem++;
		return (NULL);
	}
	memset(sp, 0, sizeof(*sp));
	sp->base = base;
	sp->bytes = bytes;
	sp->cls = (uint16_t)ci;
	sp->nobj = c->nobj;
	sp->nfree = c->nobj;
	sp->from_reserve = (uint16_t)from_reserve;
	LIST_INSERT_HEAD(&zp->spans, sp, all);
	TAILQ_INSERT_HEAD(&c->partial, sp, link);
	zp->pool_bytes += bytes;
	zp->nspans++;
	return (sp);
}

int
zpool_alloc(struct zpool *zp, size_t len, struct zspan **spanp, uint16_t *idxp)
{
	struct zclass *c;
	struct zspan *sp;
	int ci, idx;

	ci = zpool_class(len);
	c = &zp->cls[ci];
	sp = TAILQ_FIRST(&c->partial);
	if (sp == NULL) {
		sp = zp_span_new(zp, ci);
		if (sp == NULL)
			return (zp->limit != 0 &&
			    zp->pool_bytes + (uint64_t)c->pages * ZP_PAGE >
			    zp->limit ? ENOSPC : ENOMEM);
	}
	idx = zp_bit_ffc(sp->map, sp->nobj);
	/* A span on the partial list always has a free object. */
	if (idx < 0)
		return (ENOMEM);
	zp_bit_set(sp->map, idx);
	if (--sp->nfree == 0)
		TAILQ_REMOVE(&c->partial, sp, link);
	zp->obj_bytes += c->size;
	*spanp = sp;
	*idxp = (uint16_t)idx;
	return (0);
}

void
zpool_free(struct zpool *zp, struct zspan *sp, uint16_t idx)
{
	struct zclass *c = &zp->cls[sp->cls];

	if (!zp_bit_test(sp->map, idx))
		return;		/* double free: ignore rather than corrupt */
	zp_bit_clear(sp->map, idx);
	zp->obj_bytes -= c->size;
	if (sp->nfree++ == 0)
		TAILQ_INSERT_TAIL(&c->partial, sp, link);
	if (sp->nfree == sp->nobj) {
		TAILQ_REMOVE(&c->partial, sp, link);
		zp_span_release(zp, sp);
	}
}

int
zpool_wants_refill(const struct zpool *zp)
{
	return (zp->reserve_n * 2 < zp->reserve_target ||
	    zp->hdr_n * 2 < zp->hdr_target);
}

void
zpool_refill(struct zpool *zp)
{
	struct zspan *sp;
	void *p;

	/* Headers first: they are small, and a span needs one either way. */
	for (;;) {
		if (zp->lock != NULL)
			ZP_LOCK(zp);
		if (zp->hdr_n >= zp->hdr_target) {
			if (zp->lock != NULL)
				ZP_UNLOCK(zp);
			break;
		}
		if (zp->lock != NULL)
			ZP_UNLOCK(zp);
		sp = ZP_HDR_ALLOC_WAIT();
		if (sp == NULL)
			break;
		if (zp->lock != NULL)
			ZP_LOCK(zp);
		if (zp->hdr_n < zp->hdr_target) {
			LIST_INSERT_HEAD(&zp->hdr_free, sp, all);
			zp->hdr_n++;
			sp = NULL;
		}
		if (zp->lock != NULL)
			ZP_UNLOCK(zp);
		if (sp != NULL)
			ZP_HDR_FREE(sp);
	}

	for (;;) {
		if (zp->lock != NULL)
			ZP_LOCK(zp);
		if (zp->reserve_n >= zp->reserve_target) {
			if (zp->lock != NULL)
				ZP_UNLOCK(zp);
			return;
		}
		if (zp->lock != NULL)
			ZP_UNLOCK(zp);
		p = ZP_PAGES_ALLOC_WAIT(ZP_CHUNK);
		if (p == NULL)
			return;
		if (zp->lock != NULL)
			ZP_LOCK(zp);
		if (zp->reserve_n < zp->reserve_target) {
			zp->reserve[zp->reserve_n++] = p;
			p = NULL;
		}
		if (zp->lock != NULL)
			ZP_UNLOCK(zp);
		if (p != NULL)
			ZP_PAGES_FREE(p, ZP_CHUNK);
	}
}
