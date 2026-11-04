/*-
 * SPDX-License-Identifier: BSD-2-Clause
 *
 * zpool — the packed object allocator under atrium_zram (atrium-zram-v2.md
 * §4.2, increment 2).
 *
 * v1 stored each compressed page in its own malloc(9) buffer, and malloc's
 * power-of-two-ish zones cost 29 % of the store's real footprint in-VM (a
 * 2049-byte page takes a 4096-byte zone). zpool packs objects of one size
 * class into multi-page spans instead, the zsmalloc / macOS-segment shape:
 *
 *   - classes every 32 bytes from 32 to ZP_MAXOBJ (3072); a larger object
 *     is the caller's RAW case and uses the one-page class ZP_RAWCLASS;
 *   - each class carves objects from spans of k = 1..4 pages, k chosen per
 *     class to minimise the tail waste;
 *   - span memory is page-granular (kmem_malloc in the kernel) and
 *     contiguous in KVA, so an object may straddle a page boundary;
 *   - span headers are separate and come from a reserved UMA zone, so no
 *     header ever eats into a span;
 *   - a reserve of pre-allocated 4-page chunks backs allocations when the
 *     page allocator says no (v1 failed 356 swap-outs that way under
 *     pressure), refilled outside the I/O path;
 *   - a byte limit on the pool (`limit`) bounds what the compressed tier may
 *     take from the memory it is meant to save.
 *
 * Locking: the caller serializes every call on one pool, except
 * zpool_refill(), which allocates unlocked and takes `lock` to publish.
 *
 * The same source builds in the kernel and in a userspace test harness
 * (test/zpool_test.c), which supplies the ZP_* allocation hooks and can
 * inject allocation failures.
 */

#ifndef _ZRAM_POOL_H_
#define	_ZRAM_POOL_H_

#define	ZP_PAGE		4096
#define	ZP_CLASS_STEP	32
#define	ZP_MAXOBJ	3072
#define	ZP_NCLASS	(ZP_MAXOBJ / ZP_CLASS_STEP)	/* 96 packed classes */
#define	ZP_RAWCLASS	ZP_NCLASS			/* 4096-byte, 1-page spans */
#define	ZP_MAXPAGES	4
#define	ZP_CHUNK	(ZP_MAXPAGES * ZP_PAGE)		/* reserve chunk */
#define	ZP_MAXOBJS	(ZP_CHUNK / ZP_CLASS_STEP)	/* 512 per span, worst case */

struct zspan {
	TAILQ_ENTRY(zspan)	link;		/* class partial list */
	LIST_ENTRY(zspan)	all;		/* every live span */
	char			*base;
	uint32_t		bytes;		/* memory held (k pages, or a chunk) */
	uint16_t		cls;
	uint16_t		nobj;
	uint16_t		nfree;
	uint16_t		from_reserve;
	uint64_t		map[ZP_MAXOBJS / 64];	/* 1 = allocated */
};

struct zclass {
	uint32_t		size;		/* object size */
	uint16_t		pages;		/* span size in pages */
	uint16_t		nobj;		/* objects per span */
	TAILQ_HEAD(, zspan)	partial;
};

struct zpool {
	struct zclass		cls[ZP_NCLASS + 1];
	LIST_HEAD(, zspan)	spans;
	void			**reserve;	/* free 4-page chunks */
	int			reserve_n;
	int			reserve_target;
	void			*lock;		/* struct mtx * in the kernel */

	uint64_t		limit;		/* max pool_bytes, 0 = none */
	/* gauges */
	uint64_t		pool_bytes;	/* memory held by spans */
	uint64_t		obj_bytes;	/* class-rounded bytes in use */
	uint64_t		nspans;
	/* events */
	uint64_t		reserve_used;	/* spans built from the reserve */
	uint64_t		fail_nomem;
	uint64_t		fail_limit;
};

/* Which class an object of `len` bytes goes in (len <= ZP_PAGE). */
int	zpool_class(size_t len);
/* Set up classes and pre-allocate `reserve` chunks (may sleep). */
int	zpool_init(struct zpool *zp, int reserve, void *lock);
void	zpool_fini(struct zpool *zp);
/*
 * Allocate room for `len` bytes (len <= ZP_PAGE); never sleeps. 0, or
 * ENOSPC (over `limit`) / ENOMEM (no pages, reserve empty).
 */
int	zpool_alloc(struct zpool *zp, size_t len, struct zspan **spanp,
	    uint16_t *idxp);
void	zpool_free(struct zpool *zp, struct zspan *sp, uint16_t idx);
/* Top the reserve back up to its target (may sleep; takes zp->lock). */
void	zpool_refill(struct zpool *zp);
/* True when the reserve is below half its target. */
int	zpool_wants_refill(const struct zpool *zp);

static inline void *
zpool_ptr(const struct zspan *sp, uint16_t idx, uint32_t size)
{
	return (sp->base + (size_t)idx * size);
}

static inline uint32_t
zpool_objsize(const struct zpool *zp, const struct zspan *sp)
{
	return (zp->cls[sp->cls].size);
}

#endif /* !_ZRAM_POOL_H_ */
