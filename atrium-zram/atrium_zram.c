/*-
 * SPDX-License-Identifier: BSD-2-Clause
 *
 * atrium-zram — compressed-RAM swap codec + page store (atrium-memory-pressure.md
 * Phase 3, kernel half).
 *
 * The zram-equivalent compress.rs specified: a non-destructive, cooperation-free
 * reclaim tier that compresses cold anon pages in RAM rather than swapping to flash
 * or killing apps. This module is the codec + the **compressed page store** — the
 * data structure a swap block device sits on (page index -> compressed buffer),
 * with zero/same-filled-page detection (a large, codec-free fraction of the win) and
 * an incompressible-page fallback. Verified by a self-test; the block device +
 * swapon are the next increments, so there is still NO swap-path risk here.
 *
 * Codec = the kernel's in-tree zstd (ZSTD_* are global symbols, ZSTDIO compiled in)
 * via PRE-ALLOCATED contexts (created at load) — the reclaim path must never malloc.
 */

#include <sys/param.h>
#include <sys/systm.h>
#include <sys/bio.h>
#include <sys/kernel.h>
#include <sys/libkern.h>
#include <sys/lock.h>
#include <sys/malloc.h>
#include <sys/module.h>
#include <sys/mutex.h>
#include <sys/pcpu.h>
#include <sys/smp.h>
#include <sys/sbuf.h>
#include <sys/sysctl.h>
#include <sys/queue.h>
#include <sys/taskqueue.h>
#include <geom/geom_disk.h>
#include <machine/atomic.h>

#include "zram_pool.h"

/* In-kernel zstd context API (resolved at load against the kernel's globals). */
typedef struct ZSTD_CCtx_s ZSTD_CCtx;
typedef struct ZSTD_DCtx_s ZSTD_DCtx;
ZSTD_CCtx *ZSTD_createCCtx(void);
size_t ZSTD_freeCCtx(ZSTD_CCtx *);
size_t ZSTD_compressCCtx(ZSTD_CCtx *, void *dst, size_t dstCap, const void *src, size_t srcSize, int level);
ZSTD_DCtx *ZSTD_createDCtx(void);
size_t ZSTD_freeDCtx(ZSTD_DCtx *);
size_t ZSTD_decompressDCtx(ZSTD_DCtx *, void *dst, size_t dstCap, const void *src, size_t srcSize);
size_t ZSTD_compressBound(size_t srcSize);
unsigned ZSTD_isError(size_t code);

#define	ZRAM_PAGE	4096
#define	ZRAM_LEVEL	3
#define	ZRAM_NSLOTS	524288	/* store capacity in pages (524288 = 2 GiB device) */

MALLOC_DEFINE(M_ZRAM, "zram", "atrium zram store");

/* One stored page. SAME = a same-filled page (incl. all-zero) kept as a tag, no
 * buffer; COMP = zstd-compressed; RAW = incompressible (or compressed to more
 * than ZP_MAXOBJ, where packing saves nothing), stored verbatim. COMP and RAW
 * live in the packed pool (zram_pool.c), not in per-page malloc buffers. */
enum zram_state { ZRAM_EMPTY = 0, ZRAM_SAME, ZRAM_COMP, ZRAM_RAW };
struct zram_slot {
	enum zram_state	state;
	uint32_t	len;	/* COMP/RAW byte length */
	uint8_t		fill;	/* SAME fill byte */
	uint16_t	idx;	/* object index in span */
	struct zspan	*span;	/* COMP/RAW pool span, NULL for SAME/EMPTY */
};

static struct zram_slot	*zram_slots;
/*
 * Concurrency (atrium-zram-v2.md §4.1/§4.3, increment 3). v1 ran every
 * page-in and page-out on every CPU under one mutex. Now:
 *
 *   - a page's slot is guarded by one of ZRAM_NSLOTLOCKS hashed mutexes, so
 *     I/O on different pages proceeds in parallel;
 *   - each CPU has its own pre-allocated zstd contexts and compression
 *     buffer, each behind its own mutex (a thread that migrates after picking
 *     curcpu's context is still correct, just briefly sharing it);
 *   - the packed pool keeps one mutex (zram_pool_mtx), held only for the
 *     alloc/free bookkeeping, never across the codec.
 *
 * Lock order: slot -> codec context -> pool. A load decompresses straight
 * from pool memory under only the slot (and context) lock: an object is
 * freed only by a holder of its slot lock, and spans never move.
 * Counters are atomics.
 */
#define	ZRAM_NSLOTLOCKS	256
struct zram_ctx {
	struct mtx	mtx;
	ZSTD_CCtx	*cctx;	/* pre-allocated — no malloc on the hot path */
	ZSTD_DCtx	*dctx;
	char		comp[ZRAM_PAGE + 512]; /* >= ZSTD_compressBound(4096)=4174 */
} __aligned(CACHE_LINE_SIZE);
static struct zram_ctx	*zram_ctxs;
static int		 zram_nctx;
static struct mtx_padalign zram_slot_mtx[ZRAM_NSLOTLOCKS];
static struct mtx	 zram_pool_mtx;
static uint64_t		 zram_phys_bytes;	/* total physical bytes the store holds */

/* Hash on the slot's address, so private (selftest) slots work too. */
static inline struct mtx *
zram_slot_lock(const struct zram_slot *s)
{
	return ((struct mtx *)&zram_slot_mtx[((uintptr_t)s / sizeof(*s)) % ZRAM_NSLOTLOCKS]);
}

static struct zram_ctx *
zram_ctx_get(void)
{
	struct zram_ctx *c;

	c = &zram_ctxs[curcpu % zram_nctx];
	mtx_lock(&c->mtx);
	return (c);
}

#define	ZRAM_ADD(var, n)	atomic_add_64(&(var), (uint64_t)(int64_t)(n))
static struct disk	*zram_disk;	/* /dev/zram0 — the swap block device */

/*
 * The packed pool (atrium-zram-v2.md §4.2/§4.4, increment 2). Serialized by
 * zram_pool_mtx. kern.zram.mem_limit bounds what it may
 * hold; the reserve of 4-page chunks (kern.zram.reserve_chunks at load)
 * backs swap-outs when the page allocator says no, and a task refills it.
 */
static struct zpool	zram_pool;
static struct task	zram_refill_task;
static int		zram_reserve_chunks = 256;	/* 4 MiB */
TUNABLE_INT("kern.zram.reserve_chunks", &zram_reserve_chunks);

/*
 * Counters (atrium-zram-v2.md §4.6, increment 1). Atomics (ZRAM_ADD).
 * Gauges describe what the store holds NOW (they fall when a slot is freed or
 * overwritten); event counters only grow.
 *
 * ★ Measured in-VM (2026-11-03): FreeBSD's swap pager never tells the device
 * when it frees a swap block, so after a process exits its pages stay
 * compressed here until the block is reused: a 1.9 GB fill left 1.7 GB logical
 * / 0.96 GB of kernel memory held with swapinfo showing 107 MB used. The
 * gauges report that truthfully. The fix is a free-notification from
 * swap_pager (atrium-zram-v2.md §4.0); the device already honours BIO_DELETE
 * for it, and `swapon -E` uses it to clear the whole device at swapon.
 */
#define	ZRAM_HIST_BUCKET	256
#define	ZRAM_HIST_LEN		(ZRAM_PAGE / ZRAM_HIST_BUCKET + 1)	/* last = RAW */
static uint64_t	zram_pages_same, zram_pages_comp, zram_pages_raw;	/* gauges */
static uint64_t	zram_hist[ZRAM_HIST_LEN];				/* gauge */
static uint64_t	zram_reads, zram_writes, zram_deletes;			/* events, pages */
static uint64_t	zram_fail_nomem, zram_fail_limit, zram_fail_io;	/* events */

static void
zram_count(const struct zram_slot *s, int dir)
{
	switch (s->state) {
	case ZRAM_SAME:
		ZRAM_ADD(zram_pages_same, dir);
		break;
	case ZRAM_COMP:
		ZRAM_ADD(zram_pages_comp, dir);
		ZRAM_ADD(zram_hist[s->len / ZRAM_HIST_BUCKET], dir);
		break;
	case ZRAM_RAW:
		ZRAM_ADD(zram_pages_raw, dir);
		ZRAM_ADD(zram_hist[ZRAM_HIST_LEN - 1], dir);
		break;
	default:
		break;
	}
}

/* Is the page all one byte (zero or same-filled)? Returns the fill byte if so. */
static bool
zram_same_filled(const uint8_t *p, uint8_t *fill)
{
	uint8_t b = p[0];
	int i;

	for (i = 1; i < ZRAM_PAGE; i++)
		if (p[i] != b)
			return (false);
	*fill = b;
	return (true);
}

/* Caller holds the slot's lock (or owns the slot outright). */
static void
zram_free_slot(struct zram_slot *s)
{
	zram_count(s, -1);
	if (s->span != NULL) {
		ZRAM_ADD(zram_phys_bytes, -(int64_t)s->len);
		mtx_lock(&zram_pool_mtx);
		zpool_free(&zram_pool, s->span, s->idx);
		mtx_unlock(&zram_pool_mtx);
		s->span = NULL;
	}
	s->state = ZRAM_EMPTY;
	s->len = 0;
}

/* Store `page` in slot `s`. Same-filled -> tag (0 bytes); else compress; if it
 * doesn't shrink enough, store raw. Returns 0, ENOMEM or ENOSPC (at the limit). */
static int
zram_store(struct zram_slot *s, const void *page)
{
	struct mtx *lk = zram_slot_lock(s);
	struct zram_ctx *c = NULL;
	uint8_t fill;
	size_t clen;
	int err = 0, refill;

	mtx_lock(lk);
	zram_free_slot(s);

	if (zram_same_filled(page, &fill)) {
		s->state = ZRAM_SAME;
		s->fill = fill;
		goto out; /* 0 physical bytes — the zero/same-page win */
	}
	c = zram_ctx_get();
	clen = ZSTD_compressCCtx(c->cctx, c->comp, sizeof(c->comp), page, ZRAM_PAGE,
	    ZRAM_LEVEL);
	if (ZSTD_isError(clen) || clen > ZP_MAXOBJ)
		clen = ZRAM_PAGE;	/* RAW: incompressible, too big to pack, or error */
	mtx_lock(&zram_pool_mtx);
	err = zpool_alloc(&zram_pool, clen, &s->span, &s->idx);
	refill = zpool_wants_refill(&zram_pool);
	mtx_unlock(&zram_pool_mtx);
	if (refill)
		taskqueue_enqueue(taskqueue_thread, &zram_refill_task);
	if (err != 0) {
		s->span = NULL;
		ZRAM_ADD(*(err == ENOSPC ? &zram_fail_limit : &zram_fail_nomem), 1);
		goto out;
	}
	if (clen == ZRAM_PAGE) {
		memcpy(zpool_ptr(s->span, s->idx, ZRAM_PAGE), page, ZRAM_PAGE);
		s->state = ZRAM_RAW;
	} else {
		memcpy(zpool_ptr(s->span, s->idx, zpool_objsize(&zram_pool, s->span)),
		    c->comp, clen);
		s->state = ZRAM_COMP;
	}
	s->len = clen;
	ZRAM_ADD(zram_phys_bytes, s->len);
out:
	if (c != NULL)
		mtx_unlock(&c->mtx);
	zram_count(s, +1);	/* SAME, COMP or RAW; a failed store left it EMPTY */
	ZRAM_ADD(zram_writes, 1);
	mtx_unlock(lk);
	return (err);
}

/* Load slot `idx` into `page` (exactly ZRAM_PAGE bytes). 0, or EIO/ENOENT. */
static int
zram_load(struct zram_slot *s, void *page)
{
	struct mtx *lk = zram_slot_lock(s);
	struct zram_ctx *c;
	size_t r;
	int err = 0;

	mtx_lock(lk);
	switch (s->state) {
	case ZRAM_SAME:
		memset(page, s->fill, ZRAM_PAGE);
		break;
	case ZRAM_RAW:
		memcpy(page, zpool_ptr(s->span, s->idx, ZRAM_PAGE), ZRAM_PAGE);
		break;
	case ZRAM_COMP:
		c = zram_ctx_get();
		r = ZSTD_decompressDCtx(c->dctx, page, ZRAM_PAGE,
		    zpool_ptr(s->span, s->idx, zpool_objsize(&zram_pool, s->span)),
		    s->len);
		mtx_unlock(&c->mtx);
		if (ZSTD_isError(r) || r != ZRAM_PAGE) {
			ZRAM_ADD(zram_fail_io, 1);
			err = EIO;
		}
		break;
	default:
		/* never-written block reads as zeros (standard block-device / swap). */
		memset(page, 0, ZRAM_PAGE);
	}
	ZRAM_ADD(zram_reads, 1);
	mtx_unlock(lk);
	return (err);
}

/*
 * BIO_DELETE: the swap pager freed these blocks (swapon -E). Free every slot
 * the range fully covers, so the store stops holding pages nobody can read
 * again. A partial page at either end is left alone, which is always safe.
 */
static void
zram_delete(struct bio *bp)
{
	off_t first, end;
	uint32_t idx;

	first = howmany(bp->bio_offset, ZRAM_PAGE);
	end = (bp->bio_offset + bp->bio_length) / ZRAM_PAGE;
	if (end > ZRAM_NSLOTS)
		end = ZRAM_NSLOTS;
	for (idx = first; idx < end; idx++) {
		struct zram_slot *sl = &zram_slots[idx];
		struct mtx *lk;

		if (sl->state == ZRAM_EMPTY)	/* unlocked peek; rechecked below */
			continue;
		lk = zram_slot_lock(sl);
		mtx_lock(lk);
		if (sl->state != ZRAM_EMPTY) {
			zram_free_slot(sl);
			ZRAM_ADD(zram_deletes, 1);
		}
		mtx_unlock(lk);
	}
	bp->bio_resid = 0;
	biofinish(bp, NULL, 0);
}

/*
 * Block-device strategy: map each page of a BIO to a store slot. Swap (and dd
 * bs=4096) do page-aligned I/O; reject anything else. This is /dev/zram0's hot
 * path — store on write, load on read.
 */
static void
zram_strategy(struct bio *bp)
{
	uint8_t *data;
	off_t off;
	uint32_t idx;
	int err = 0;

	if (bp->bio_cmd == BIO_DELETE) {
		zram_delete(bp);
		return;
	}
	if (bp->bio_cmd != BIO_READ && bp->bio_cmd != BIO_WRITE) {
		biofinish(bp, NULL, EOPNOTSUPP);
		return;
	}
	if ((bp->bio_offset % ZRAM_PAGE) != 0 || (bp->bio_length % ZRAM_PAGE) != 0) {
		biofinish(bp, NULL, EINVAL);
		return;
	}
	data = bp->bio_data;
	for (off = 0; off < bp->bio_length; off += ZRAM_PAGE) {
		idx = (uint32_t)((bp->bio_offset + off) / ZRAM_PAGE);
		if (idx >= ZRAM_NSLOTS) { err = EINVAL; break; }
		err = (bp->bio_cmd == BIO_READ) ?
		    zram_load(&zram_slots[idx], data + off) :
		    zram_store(&zram_slots[idx], data + off);
		if (err != 0)
			break;
	}
	bp->bio_resid = (err != 0) ? bp->bio_length : 0;
	biofinish(bp, NULL, err);
}

/* Self-test: store a zero page, a compressible page, and an incompressible
 * (random) page; load each back and verify; report the compression. */
static int
zram_selftest(SYSCTL_HANDLER_ARGS)
{
	/*
	 * Private slots, never the device's: with /dev/zram0 swapped on, any
	 * table slot may hold a live swapped-out page, and v1's use of slots
	 * 0..2 here could overwrite one (the swap pager reserves only the
	 * first BBSIZE of the device).
	 */
	struct zram_slot t[3];
	char *p, *back, res[160];
	int i, fails = 0;
	uint64_t before;

	memset(t, 0, sizeof(t));
	p = malloc(ZRAM_PAGE, M_ZRAM, M_WAITOK);
	back = malloc(ZRAM_PAGE, M_ZRAM, M_WAITOK);
	before = zram_phys_bytes;

	/* slot 0: all-zero -> SAME, 0 physical bytes. */
	memset(p, 0, ZRAM_PAGE);
	zram_store(&t[0], p);
	if (zram_load(&t[0], back) != 0 || memcmp(p, back, ZRAM_PAGE) != 0) fails++;

	/* slot 1: half-zero + low-entropy -> COMP. */
	memset(p, 0, ZRAM_PAGE);
	for (i = ZRAM_PAGE / 2; i < ZRAM_PAGE; i++) p[i] = (char)(i & 0x1f);
	zram_store(&t[1], p);
	if (zram_load(&t[1], back) != 0 || memcmp(p, back, ZRAM_PAGE) != 0) fails++;

	/* slot 2: random -> incompressible -> RAW fallback. */
	arc4random_buf(p, ZRAM_PAGE);
	zram_store(&t[2], p);
	if (zram_load(&t[2], back) != 0 || memcmp(p, back, ZRAM_PAGE) != 0) fails++;

	snprintf(res, sizeof(res),
	    "%s | 3 pages (zero/compressible/random) = 12288 logical -> %ju physical bytes "
	    "[slot0=%s slot1=%s(%u) slot2=%s(%u)]",
	    fails ? "FAIL" : "OK round-trips", (uintmax_t)(zram_phys_bytes - before),
	    "SAME", t[1].state == ZRAM_COMP ? "COMP" : "?", t[1].len,
	    t[2].state == ZRAM_RAW ? "RAW" : "?", t[2].len);

	/* leave the store clean for repeat reads (private slots: no lock). */
	zram_free_slot(&t[0]);
	zram_free_slot(&t[1]);
	zram_free_slot(&t[2]);
	free(p, M_ZRAM);
	free(back, M_ZRAM);
	return (sysctl_handle_string(oidp, res, sizeof(res), req));
}

static SYSCTL_NODE(_kern, OID_AUTO, zram, CTLFLAG_RD | CTLFLAG_MPSAFE, NULL,
    "atrium compressed-RAM swap store");
SYSCTL_PROC(_kern_zram, OID_AUTO, selftest,
    CTLTYPE_STRING | CTLFLAG_RD | CTLFLAG_MPSAFE, NULL, 0,
    zram_selftest, "A", "Store+load zero/compressible/random pages, verify + report");
SYSCTL_U64(_kern_zram, OID_AUTO, phys_bytes, CTLFLAG_RD, &zram_phys_bytes, 0,
    "Physical bytes the compressed store currently holds");

/* Logical bytes held = every non-empty slot is one page the VM swapped out. */
static int
zram_orig_bytes(SYSCTL_HANDLER_ARGS)
{
	uint64_t v;

	v = (atomic_load_64(&zram_pages_same) + atomic_load_64(&zram_pages_comp) +
	    atomic_load_64(&zram_pages_raw)) * ZRAM_PAGE;
	return (sysctl_handle_64(oidp, &v, 0, req));
}
SYSCTL_PROC(_kern_zram, OID_AUTO, orig_bytes,
    CTLTYPE_U64 | CTLFLAG_RD | CTLFLAG_MPSAFE, NULL, 0, zram_orig_bytes, "QU",
    "Logical (uncompressed) bytes the store currently holds");
SYSCTL_U64(_kern_zram, OID_AUTO, pages_same, CTLFLAG_RD, &zram_pages_same, 0,
    "Pages held as a fill byte (zero or same-filled), 0 data bytes");
SYSCTL_U64(_kern_zram, OID_AUTO, pages_comp, CTLFLAG_RD, &zram_pages_comp, 0,
    "Pages held compressed");
SYSCTL_U64(_kern_zram, OID_AUTO, pages_raw, CTLFLAG_RD, &zram_pages_raw, 0,
    "Pages held verbatim (did not compress below a page)");
SYSCTL_U64(_kern_zram, OID_AUTO, reads, CTLFLAG_RD, &zram_reads, 0,
    "Pages read (swap-ins), since load");
SYSCTL_U64(_kern_zram, OID_AUTO, writes, CTLFLAG_RD, &zram_writes, 0,
    "Pages written (swap-outs), since load");
SYSCTL_U64(_kern_zram, OID_AUTO, deletes, CTLFLAG_RD, &zram_deletes, 0,
    "Held pages freed by BIO_DELETE (swap trim), since load");
SYSCTL_U64(_kern_zram, OID_AUTO, fail_nomem, CTLFLAG_RD, &zram_fail_nomem, 0,
    "Stores that failed for lack of memory, since load");
SYSCTL_U64(_kern_zram, OID_AUTO, fail_io, CTLFLAG_RD, &zram_fail_io, 0,
    "Loads that failed to decompress, since load");
SYSCTL_U64(_kern_zram, OID_AUTO, fail_limit, CTLFLAG_RD, &zram_fail_limit, 0,
    "Stores refused because the pool is at mem_limit, since load");

/* The packed pool. pool_bytes is the store's real memory footprint. */
SYSCTL_U64(_kern_zram, OID_AUTO, mem_limit, CTLFLAG_RW, &zram_pool.limit, 0,
    "Max bytes the packed pool may hold (0 = unlimited)");
SYSCTL_U64(_kern_zram, OID_AUTO, pool_bytes, CTLFLAG_RD, &zram_pool.pool_bytes, 0,
    "Memory held by the packed pool's spans (the store's footprint)");
SYSCTL_U64(_kern_zram, OID_AUTO, obj_bytes, CTLFLAG_RD, &zram_pool.obj_bytes, 0,
    "Class-rounded bytes of live objects (pool_bytes - obj_bytes = fragmentation)");
SYSCTL_U64(_kern_zram, OID_AUTO, spans, CTLFLAG_RD, &zram_pool.nspans, 0,
    "Live pool spans");
SYSCTL_INT(_kern_zram, OID_AUTO, reserve_avail, CTLFLAG_RD, &zram_pool.reserve_n, 0,
    "Reserve chunks (4 pages each) on hand");
SYSCTL_U64(_kern_zram, OID_AUTO, reserve_used, CTLFLAG_RD, &zram_pool.reserve_used, 0,
    "Spans built from the reserve because the page allocator refused, since load");
SYSCTL_INT(_kern_zram, OID_AUTO, hdr_reserve_avail, CTLFLAG_RD, &zram_pool.hdr_n, 0,
    "Spare span headers on hand");
SYSCTL_U64(_kern_zram, OID_AUTO, hdr_reserve_used, CTLFLAG_RD,
    &zram_pool.hdr_reserve_used, 0,
    "Span headers taken from the spare list because UMA refused, since load");

/* "<bucket upper bound>:<pages> ..." for COMP pages, then RAW:<pages>. */
static int
zram_hist_sysctl(SYSCTL_HANDLER_ARGS)
{
	uint64_t h[ZRAM_HIST_LEN];
	struct sbuf sb;
	int i, error;

	for (i = 0; i < ZRAM_HIST_LEN; i++)
		h[i] = atomic_load_64(&zram_hist[i]);
	sbuf_new_for_sysctl(&sb, NULL, 256, req);
	for (i = 0; i < ZRAM_HIST_LEN - 1; i++)
		sbuf_printf(&sb, "%d:%ju ", (i + 1) * ZRAM_HIST_BUCKET, (uintmax_t)h[i]);
	sbuf_printf(&sb, "RAW:%ju", (uintmax_t)h[ZRAM_HIST_LEN - 1]);
	error = sbuf_finish(&sb);
	sbuf_delete(&sb);
	return (error);
}
SYSCTL_PROC(_kern_zram, OID_AUTO, hist,
    CTLTYPE_STRING | CTLFLAG_RD | CTLFLAG_MPSAFE, NULL, 0, zram_hist_sysctl, "A",
    "Compressed-size histogram of held pages (256-byte buckets, then RAW)");

static int64_t zram_mem_limit_tun = -1;

static void
zram_refill(void *arg __unused, int pending __unused)
{
	zpool_refill(&zram_pool);
}

static void
zram_ctx_free(void)
{
	int i;

	for (i = 0; i < zram_nctx; i++) {
		if (zram_ctxs[i].cctx != NULL)
			ZSTD_freeCCtx(zram_ctxs[i].cctx);
		if (zram_ctxs[i].dctx != NULL)
			ZSTD_freeDCtx(zram_ctxs[i].dctx);
		mtx_destroy(&zram_ctxs[i].mtx);
	}
	free(zram_ctxs, M_ZRAM);
	zram_ctxs = NULL;
	zram_nctx = 0;
}

static int
zram_modevent(module_t mod __unused, int type, void *data __unused)
{
	uint32_t i;

	switch (type) {
	case MOD_LOAD:
		/* One codec context per CPU, all allocated up front. */
		zram_nctx = mp_ncpus;
		zram_ctxs = malloc(sizeof(struct zram_ctx) * zram_nctx, M_ZRAM,
		    M_WAITOK | M_ZERO);
		for (i = 0; i < (uint32_t)zram_nctx; i++) {
			mtx_init(&zram_ctxs[i].mtx, "zram ctx", NULL, MTX_DEF);
			zram_ctxs[i].cctx = ZSTD_createCCtx();
			zram_ctxs[i].dctx = ZSTD_createDCtx();
			if (zram_ctxs[i].cctx == NULL || zram_ctxs[i].dctx == NULL) {
				zram_nctx = i + 1;	/* free only what was set up */
				zram_ctx_free();
				return (ENOMEM);
			}
		}
		for (i = 0; i < ZRAM_NSLOTLOCKS; i++)
			mtx_init((struct mtx *)&zram_slot_mtx[i], "zram slot", NULL, MTX_DEF);
		mtx_init(&zram_pool_mtx, "zram pool", NULL, MTX_DEF);
		zram_slots = malloc(sizeof(struct zram_slot) * ZRAM_NSLOTS, M_ZRAM,
		    M_WAITOK | M_ZERO);
		TASK_INIT(&zram_refill_task, 0, zram_refill, NULL);
		if (zpool_init(&zram_pool, zram_reserve_chunks, &zram_pool_mtx) != 0)
			printf("atrium_zram: reserve short (%d of %d chunks)\n",
			    zram_pool.reserve_n, zram_reserve_chunks);
		/* Default limit: 25 % of RAM, unless a tunable set one. */
		TUNABLE_QUAD_FETCH("kern.zram.mem_limit", &zram_mem_limit_tun);
		zram_pool.limit = zram_mem_limit_tun >= 0 ?
		    (uint64_t)zram_mem_limit_tun : ptoa((uint64_t)physmem) / 4;
		/* /dev/zram0 — a compressed RAM block device (page-sized sectors). */
		zram_disk = disk_alloc();
		zram_disk->d_strategy = zram_strategy;
		zram_disk->d_name = "zram";
		zram_disk->d_unit = 0;
		zram_disk->d_sectorsize = ZRAM_PAGE;
		zram_disk->d_mediasize = (off_t)ZRAM_NSLOTS * ZRAM_PAGE;
		zram_disk->d_maxsize = ZRAM_PAGE * 16;
		zram_disk->d_flags = DISKFLAG_CANDELETE;	/* swapon -E trims into us */
		zram_disk->d_delmaxsize = (off_t)ZRAM_NSLOTS * ZRAM_PAGE;
		disk_create(zram_disk, DISK_VERSION);
		printf("atrium_zram: /dev/zram0 ready (%d pages = %juMB, packed pool, "
		    "limit %juMB, reserve %dKB)\n",
		    ZRAM_NSLOTS, (uintmax_t)((off_t)ZRAM_NSLOTS * ZRAM_PAGE >> 20),
		    (uintmax_t)(zram_pool.limit >> 20), zram_pool.reserve_n * ZP_CHUNK / 1024);
		return (0);
	case MOD_UNLOAD:
		disk_destroy(zram_disk);
		taskqueue_drain(taskqueue_thread, &zram_refill_task);
		for (i = 0; i < ZRAM_NSLOTS; i++)
			zram_free_slot(&zram_slots[i]);
		free(zram_slots, M_ZRAM);
		zpool_fini(&zram_pool);
		zram_ctx_free();
		for (i = 0; i < ZRAM_NSLOTLOCKS; i++)
			mtx_destroy((struct mtx *)&zram_slot_mtx[i]);
		mtx_destroy(&zram_pool_mtx);
		return (0);
	default:
		return (EOPNOTSUPP);
	}
}

static moduledata_t zram_mod = { "atrium_zram", zram_modevent, NULL };
DECLARE_MODULE(atrium_zram, zram_mod, SI_SUB_DRIVERS, SI_ORDER_MIDDLE);
