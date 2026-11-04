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
#include <sys/bitstring.h>
#include <sys/callout.h>
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
#include <geom/geom.h>
#include <geom/geom_disk.h>
#include <vm/uma.h>
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
/*
 * Device size and slot table (atrium-zram-v2.md §4.1, increment 4). The size
 * is kern.zram.disksize (bytes, default = physical RAM), read at load. The
 * table is a directory of fixed-size chunks, each allocated on the first
 * write into its range, so an idle or lightly used device wires almost
 * nothing.
 */
#define	ZRAM_CHUNK_SHIFT	12
#define	ZRAM_CHUNK_SLOTS	(1u << ZRAM_CHUNK_SHIFT)	/* 4096 slots = 64 KiB */
#define	ZRAM_MIN_SLOTS		(16u << (20 - 12))		/* 16 MiB device */
#define	ZRAM_MAX_SLOTS		(1u << 28)			/* 1 TiB device */

MALLOC_DEFINE(M_ZRAM, "zram", "atrium zram store");

/* One stored page. SAME = a same-filled page (incl. all-zero) kept as a tag, no
 * buffer; COMP = zstd-compressed; RAW = incompressible (or compressed to more
 * than ZP_MAXOBJ, where packing saves nothing), stored verbatim. COMP and RAW
 * live in the packed pool (zram_pool.c), not in per-page malloc buffers. */
enum zram_state { ZRAM_EMPTY = 0, ZRAM_SAME, ZRAM_COMP, ZRAM_RAW, ZRAM_WB };
struct zram_slot {			/* 16 bytes (v1: 24) */
	union {
		struct zspan *span;	/* COMP/RAW: pool span */
		uint64_t blk;		/* WB: backing-store block */
	};
	uint16_t	idx;	/* object index in span */
	uint16_t	len;	/* COMP/RAW/WB byte length (<= ZRAM_PAGE; WB < PAGE = zstd) */
	uint8_t		state;	/* enum zram_state */
	uint8_t		fill;	/* SAME: fill byte; COMP/RAW: ZRAM_F_* flags */
	uint16_t	gen;	/* bumped whenever the contents change */
};
#define	ZRAM_F_IDLE	0x01	/* not read since the last idle scan */
_Static_assert(sizeof(struct zram_slot) == 16, "zram_slot is 16 bytes");

static uint32_t		 zram_nslots;	/* device size in pages */
static uint32_t		 zram_ndir;	/* chunks in the directory */
static struct zram_slot	**zram_dir;	/* chunk pointers, NULL until first write */
static struct zram_slot	*zram_spare_chunk; /* for when malloc(M_NOWAIT) says no */
static uint64_t		 zram_table_bytes;	/* gauge: chunk memory */
static uint64_t		 zram_chunk_spare_used;	/* event */
static uint64_t		 zram_fail_table;	/* event: no memory for a chunk */
static uint64_t		 zram_disksize_tun;
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
static uint64_t	zram_pages_wb;			/* gauge: pages on the backing store */
static uint64_t	zram_wb_pages, zram_wb_through, zram_wb_reads;	/* events, pages */
static uint64_t	zram_wb_fail, zram_wb_scans;			/* events */

/*
 * Backing store (atrium-zram-v2.md §4.5, increment 5). Optional: a GEOM
 * provider named by kern.zram.backing_dev. Only /dev/zram0 is swapped on;
 * zram decides what goes to disk:
 *
 *   - RAW pages (nothing gained by keeping them in RAM), soon after they land;
 *   - idle pages: each scan marks COMP slots idle, a read clears the mark, and
 *     a slot still idle at the next scan (kern.zram.idle_secs) is written back;
 *   - over 90 % of mem_limit, any COMP slot, until the pool is under 80 %;
 *   - a store the pool refuses (at the limit, or no memory) is written
 *     through to disk directly from the swap-out bio instead of failing.
 *
 * Writeback runs on its own taskqueue thread and does synchronous I/O. A slot
 * keeps its RAM copy until its disk write is done, and is switched to WB only
 * if its generation is unchanged. Reads of WB slots are asynchronous child
 * bios, completed (and decompressed) in the GEOM up path. One object per 4 KiB
 * block: the disk holds pages, not packed compressed bytes.
 */
static struct g_geom	*zram_bk_gp;
static struct g_consumer *zram_bk_cp;	/* NULL: no backing store */
static volatile int	 zram_bk_gone;	/* provider went away */
static char		 zram_bk_name[64];
static bitstr_t		*zram_bk_map;	/* 1 = block in use */
static int		 zram_bk_nblk, zram_bk_hint;
static uint64_t		 zram_bk_used;	/* gauge, blocks */
static struct mtx	 zram_bk_mtx;
static struct taskqueue	*zram_wb_tq;
static struct task	 zram_wb_task;
static struct callout	 zram_wb_callout;
static volatile int	 zram_wb_idle_due;
static int		 zram_idle_secs = 300;
static int		 zram_wb_ticks;
static uma_zone_t	 zram_io_zone;

static void	zram_bk_free(uint64_t blk);

static inline bool
zram_bk_usable(void)
{
	return (zram_bk_cp != NULL && !zram_bk_gone);
}

static inline void
zram_wb_kick(void)
{
	if (zram_bk_usable())
		taskqueue_enqueue(zram_wb_tq, &zram_wb_task);
}

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
	case ZRAM_WB:
		ZRAM_ADD(zram_pages_wb, dir);
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
zram_drop_obj(struct zram_slot *s)
{
	ZRAM_ADD(zram_phys_bytes, -(int64_t)s->len);
	mtx_lock(&zram_pool_mtx);
	zpool_free(&zram_pool, s->span, s->idx);
	mtx_unlock(&zram_pool_mtx);
	s->span = NULL;
}

static void
zram_free_slot(struct zram_slot *s)
{
	zram_count(s, -1);
	switch (s->state) {
	case ZRAM_COMP:
	case ZRAM_RAW:
		zram_drop_obj(s);
		break;
	case ZRAM_WB:
		zram_bk_free(s->blk);
		break;
	default:
		break;
	}
	s->span = NULL;
	s->state = ZRAM_EMPTY;
	s->len = 0;
	s->fill = 0;
	s->gen++;
}

/*
 * Slot `idx`, or NULL when its chunk has never been written. With `create`,
 * allocate the chunk: M_NOWAIT (this is the swap-out path), then the spare
 * chunk, which the refill task replaces. Chunks live until unload.
 */
static struct zram_slot *
zram_slot(uint32_t idx, bool create)
{
	volatile uintptr_t *dp = (volatile uintptr_t *)&zram_dir[idx >> ZRAM_CHUNK_SHIFT];
	struct zram_slot *ch;
	const size_t bytes = sizeof(struct zram_slot) * ZRAM_CHUNK_SLOTS;

	ch = (struct zram_slot *)atomic_load_acq_ptr(dp);
	if (ch == NULL && create) {
		ch = malloc(bytes, M_ZRAM, M_NOWAIT | M_ZERO);
		if (ch == NULL) {
			ch = (struct zram_slot *)atomic_swap_ptr(
			    (volatile uintptr_t *)&zram_spare_chunk, 0);
			if (ch != NULL) {
				ZRAM_ADD(zram_chunk_spare_used, 1);
				taskqueue_enqueue(taskqueue_thread, &zram_refill_task);
			}
		}
		if (ch == NULL) {
			ZRAM_ADD(zram_fail_table, 1);
			return (NULL);
		}
		if (atomic_cmpset_rel_ptr(dp, 0, (uintptr_t)ch)) {
			ZRAM_ADD(zram_table_bytes, bytes);
		} else {
			/* Another writer installed this chunk first. */
			free(ch, M_ZRAM);
			ch = (struct zram_slot *)atomic_load_acq_ptr(dp);
		}
	}
	return (ch == NULL ? NULL : &ch[idx & (ZRAM_CHUNK_SLOTS - 1)]);
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
	/* Near or at the limit: start moving pages to disk. */
	if (zram_pool.limit != 0 &&
	    zram_pool.pool_bytes + ZP_CHUNK > zram_pool.limit / 10 * 9)
		zram_wb_kick();
	if (err != 0) {
		s->span = NULL;
		ZRAM_ADD(*(err == ENOSPC ? &zram_fail_limit : &zram_fail_nomem), 1);
		goto out;
	}
	if (clen == ZRAM_PAGE) {
		memcpy(zpool_ptr(s->span, s->idx, ZRAM_PAGE), page, ZRAM_PAGE);
		s->state = ZRAM_RAW;
		zram_wb_kick();	/* RAW belongs on disk */
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

/*
 * Load slot `s` into `page` (exactly ZRAM_PAGE bytes). 0 or EIO; or, for a
 * written-back slot, EJUSTRETURN with its block and length in *blkp / *lenp,
 * for the caller to read asynchronously.
 */
static int
zram_load(struct zram_slot *s, void *page, uint64_t *blkp, uint16_t *lenp)
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
		s->fill &= ~ZRAM_F_IDLE;
		break;
	case ZRAM_WB:
		*blkp = s->blk;
		*lenp = s->len;
		err = EJUSTRETURN;
		break;
	case ZRAM_COMP:
		s->fill &= ~ZRAM_F_IDLE;
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
	if (end > zram_nslots)
		end = zram_nslots;
	for (idx = first; idx < end; idx++) {
		struct zram_slot *sl = zram_slot(idx, false);
		struct mtx *lk;

		if (sl == NULL) {	/* chunk never written: skip the rest of it */
			idx |= ZRAM_CHUNK_SLOTS - 1;
			continue;
		}
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

/* ---- backing store: block allocator -------------------------------------- */

static int
zram_bk_alloc(uint64_t *blkp)
{
	int b;

	mtx_lock(&zram_bk_mtx);
	bit_ffc_at(zram_bk_map, zram_bk_hint, zram_bk_nblk, &b);
	if (b < 0)
		bit_ffc(zram_bk_map, zram_bk_nblk, &b);
	if (b >= 0) {
		bit_set(zram_bk_map, b);
		zram_bk_hint = b + 1 < zram_bk_nblk ? b + 1 : 0;
		zram_bk_used++;
	}
	mtx_unlock(&zram_bk_mtx);
	if (b < 0)
		return (ENOSPC);
	*blkp = (uint64_t)b;
	return (0);
}

static void
zram_bk_free(uint64_t blk)
{
	mtx_lock(&zram_bk_mtx);
	KASSERT(bit_test(zram_bk_map, (int)blk), ("zram: backing block %ju not in use",
	    (uintmax_t)blk));
	bit_clear(zram_bk_map, (int)blk);
	zram_bk_used--;
	mtx_unlock(&zram_bk_mtx);
}

/* ---- async I/O against the backing store, from the strategy path -------- */

/*
 * The strategy bio is completed when its last page is: bio_children counts
 * the outstanding parts (1 for the strategy loop itself, plus one per child
 * bio), and the first error wins.
 */
static void
zram_parent_drop(struct bio *bp, int err)
{
	if (err != 0)
		atomic_cmpset_int((volatile u_int *)&bp->bio_error, 0, err);
	if (atomic_fetchadd_int((volatile u_int *)&bp->bio_children, -1) != 1)
		return;
	bp->bio_children = 0;
	err = bp->bio_error;
	bp->bio_error = 0;
	bp->bio_resid = (err != 0) ? bp->bio_length : 0;
	biofinish(bp, NULL, err);
}

struct zram_io {
	struct bio		*parent;
	struct zram_slot	*slot;	/* write-through: the slot to roll back */
	char			*dst;	/* read: where the page goes */
	uint64_t		 blk;
	uint16_t		 len;
	uint16_t		 gen;
	char			 buf[ZRAM_PAGE];	/* read of a compressed block */
};

static void
zram_io_done(struct bio *cbp)
{
	struct zram_io *io = cbp->bio_caller1;
	struct zram_ctx *c;
	struct mtx *lk;
	size_t r;
	int err = cbp->bio_error;

	if (cbp->bio_cmd == BIO_READ) {
		if (err == 0 && io->len < ZRAM_PAGE) {
			c = zram_ctx_get();
			r = ZSTD_decompressDCtx(c->dctx, io->dst, ZRAM_PAGE, io->buf,
			    io->len);
			mtx_unlock(&c->mtx);
			if (ZSTD_isError(r) || r != ZRAM_PAGE)
				err = EIO;
		}
		if (err != 0)
			ZRAM_ADD(zram_fail_io, 1);
	} else if (err != 0) {
		/* Write-through failed: the slot must not claim the block. */
		lk = zram_slot_lock(io->slot);
		mtx_lock(lk);
		if (io->slot->state == ZRAM_WB && io->slot->gen == io->gen &&
		    io->slot->blk == io->blk)
			zram_free_slot(io->slot);
		mtx_unlock(lk);
		ZRAM_ADD(zram_wb_fail, 1);
	}
	zram_parent_drop(io->parent, err);
	g_destroy_bio(cbp);
	uma_zfree(zram_io_zone, io);
}

static int
zram_io_start(struct bio *parent, int cmd, uint64_t blk, char *data,
    struct zram_io *io)
{
	struct bio *cbp;

	cbp = g_new_bio();
	if (cbp == NULL)
		return (ENOMEM);
	io->parent = parent;
	io->blk = blk;
	cbp->bio_cmd = cmd;
	cbp->bio_offset = (off_t)blk * ZRAM_PAGE;
	cbp->bio_length = ZRAM_PAGE;
	cbp->bio_data = data;
	cbp->bio_done = zram_io_done;
	cbp->bio_caller1 = io;
	atomic_add_int((volatile u_int *)&parent->bio_children, 1);
	g_io_request(cbp, zram_bk_cp);
	return (0);
}

/* Read a written-back page. */
static int
zram_bk_read(struct bio *parent, char *dst, uint64_t blk, uint16_t len)
{
	struct zram_io *io;
	int err;

	if (!zram_bk_usable())
		return (EIO);
	io = uma_zalloc(zram_io_zone, M_NOWAIT | M_USE_RESERVE);
	if (io == NULL)
		return (ENOMEM);
	io->dst = dst;
	io->len = len;
	io->slot = NULL;
	err = zram_io_start(parent, BIO_READ, blk, len < ZRAM_PAGE ? io->buf : dst, io);
	if (err != 0)
		uma_zfree(zram_io_zone, io);
	else
		ZRAM_ADD(zram_wb_reads, 1);
	return (err);
}

/* The pool refused this page: write it straight to disk, uncompressed. */
static int
zram_bk_write_through(struct bio *parent, struct zram_slot *s, char *page)
{
	struct zram_io *io;
	struct mtx *lk = zram_slot_lock(s);
	uint64_t blk;
	int err;

	if (!zram_bk_usable())
		return (ENOSPC);
	io = uma_zalloc(zram_io_zone, M_NOWAIT | M_USE_RESERVE);
	if (io == NULL)
		return (ENOMEM);
	if ((err = zram_bk_alloc(&blk)) != 0) {
		uma_zfree(zram_io_zone, io);
		return (err);
	}
	mtx_lock(lk);
	zram_free_slot(s);
	s->state = ZRAM_WB;
	s->blk = blk;
	s->len = ZRAM_PAGE;
	zram_count(s, +1);
	io->slot = s;
	io->gen = s->gen;
	io->len = ZRAM_PAGE;
	mtx_unlock(lk);
	/* The swap pager will not read this block before the write completes. */
	err = zram_io_start(parent, BIO_WRITE, blk, page, io);
	if (err != 0) {
		mtx_lock(lk);
		if (s->state == ZRAM_WB && s->gen == io->gen)
			zram_free_slot(s);
		mtx_unlock(lk);
		uma_zfree(zram_io_zone, io);
	} else
		ZRAM_ADD(zram_wb_through, 1);
	return (err);
}

/* ---- writeback task -------------------------------------------------------- */

#define	ZRAM_WB_BATCH	256		/* pages per batch (1 MiB) */
#define	ZRAM_WB_RUN	32		/* pages per write bio (128 KiB) */
#define	ZRAM_WB_TICK	5		/* seconds between RAW / pressure checks */

struct zram_wb_rec {
	struct zram_slot	*slot;
	uint64_t		 blk;
	uint16_t		 gen;
	uint16_t		 len;
	uint8_t			 state;
	int			 ok;
};
static struct zram_wb_rec *zram_wb_recs;
static char		*zram_wb_buf;

static bool
zram_wb_pressure(void)
{
	uint64_t lim = zram_pool.limit;

	return (lim != 0 && zram_pool.pool_bytes > lim / 10 * 8);
}

static int
zram_wb_io(int cmd, uint64_t blk, char *data, int npages)
{
	struct bio *bp;
	int err;

	bp = g_alloc_bio();
	bp->bio_cmd = cmd;
	bp->bio_offset = (off_t)blk * ZRAM_PAGE;
	bp->bio_length = (off_t)npages * ZRAM_PAGE;
	bp->bio_data = data;
	bp->bio_done = NULL;
	g_io_request(bp, zram_bk_cp);
	err = biowait(bp, "zramwb");
	g_destroy_bio(bp);
	return (err);
}

/* Write the batch, then switch each still-unchanged slot to WB. */
static void
zram_wb_flush(int n)
{
	struct zram_wb_rec *r;
	struct mtx *lk;
	int i, j, run, err;

	for (i = 0; i < n; i++) {
		zram_wb_recs[i].ok = 0;
		if (zram_bk_alloc(&zram_wb_recs[i].blk) != 0)
			break;
	}
	n = i;		/* disk full: the rest stay in RAM */
	for (i = 0; i < n; i = j) {
		/* A run of consecutive blocks goes out as one write. */
		for (j = i + 1; j < n && j - i < ZRAM_WB_RUN &&
		    zram_wb_recs[j].blk == zram_wb_recs[j - 1].blk + 1; j++)
			;
		run = j - i;
		err = zram_wb_io(BIO_WRITE, zram_wb_recs[i].blk,
		    zram_wb_buf + (size_t)i * ZRAM_PAGE, run);
		if (err != 0)
			ZRAM_ADD(zram_wb_fail, run);
		for (; run > 0; run--)
			zram_wb_recs[j - run].ok = (err == 0);
	}
	for (i = 0; i < n; i++) {
		r = &zram_wb_recs[i];
		if (r->ok) {
			lk = zram_slot_lock(r->slot);
			mtx_lock(lk);
			if (r->slot->gen == r->gen && r->slot->state == r->state) {
				zram_count(r->slot, -1);
				zram_drop_obj(r->slot);
				r->slot->state = ZRAM_WB;
				r->slot->blk = r->blk;
				r->slot->fill = 0;
				zram_count(r->slot, +1);
				r->ok = 2;	/* block now owned by the slot */
			}
			mtx_unlock(lk);
		}
		if (r->ok != 2)
			zram_bk_free(r->blk);
		else
			ZRAM_ADD(zram_wb_pages, 1);
	}
}

static void
zram_wb_run(void *arg __unused, int pending __unused)
{
	struct zram_slot *ch, *s;
	struct mtx *lk;
	uint32_t ci, j;
	bool idle, take;
	uint8_t st;
	int n = 0;

	if (!zram_bk_usable())
		return;
	idle = atomic_readandclear_int(&zram_wb_idle_due) != 0;
	if (!idle && !zram_wb_pressure() && atomic_load_64(&zram_pages_raw) == 0)
		return;
	ZRAM_ADD(zram_wb_scans, 1);
	for (ci = 0; ci < zram_ndir; ci++) {
		ch = (struct zram_slot *)atomic_load_acq_ptr(
		    (volatile uintptr_t *)&zram_dir[ci]);
		if (ch == NULL)
			continue;
		for (j = 0; j < ZRAM_CHUNK_SLOTS; j++) {
			s = &ch[j];
			st = s->state;		/* unlocked peek */
			if (st != ZRAM_COMP && st != ZRAM_RAW)
				continue;
			if (st == ZRAM_COMP && !idle && !zram_wb_pressure())
				continue;
			lk = zram_slot_lock(s);
			mtx_lock(lk);
			st = s->state;
			take = false;
			if (st == ZRAM_RAW)
				take = true;
			else if (st == ZRAM_COMP) {
				if (zram_wb_pressure() ||
				    (idle && (s->fill & ZRAM_F_IDLE) != 0))
					take = true;
				else if (idle)
					s->fill |= ZRAM_F_IDLE;
			}
			if (take) {
				memcpy(zram_wb_buf + (size_t)n * ZRAM_PAGE,
				    zpool_ptr(s->span, s->idx, zpool_objsize(&zram_pool,
				    s->span)), s->len);
				zram_wb_recs[n].slot = s;
				zram_wb_recs[n].gen = s->gen;
				zram_wb_recs[n].len = s->len;
				zram_wb_recs[n].state = st;
				n++;
			}
			mtx_unlock(lk);
			if (n == ZRAM_WB_BATCH) {
				zram_wb_flush(n);
				n = 0;
				if (!zram_bk_usable())
					return;
			}
		}
	}
	if (n > 0)
		zram_wb_flush(n);
}

static void
zram_wb_tick(void *arg __unused)
{
	zram_wb_ticks += ZRAM_WB_TICK;
	if (zram_wb_ticks >= zram_idle_secs) {
		zram_wb_ticks = 0;
		atomic_store_int(&zram_wb_idle_due, 1);
	}
	taskqueue_enqueue(zram_wb_tq, &zram_wb_task);
	callout_schedule(&zram_wb_callout, ZRAM_WB_TICK * hz);
}

/* ---- attaching the backing provider ------------------------------------- */

static void
zram_bk_orphan(struct g_consumer *cp __unused)
{
	g_topology_assert();
	zram_bk_gone = 1;	/* reads of WB slots now fail; detached at unload */
}

static struct g_class zram_bk_class = {
	.name = "ZRAM_BACKING",
	.version = G_VERSION,
	.orphan = zram_bk_orphan,
};

static int
zram_bk_attach(const char *name)
{
	struct g_provider *pp;
	struct g_consumer *cp;
	off_t nblk;
	int error;

	g_topology_lock();
	if (zram_bk_cp != NULL) {
		g_topology_unlock();
		return (EBUSY);
	}
	pp = g_provider_by_name(name);
	if (pp == NULL) {
		g_topology_unlock();
		return (ENOENT);
	}
	nblk = pp->mediasize / ZRAM_PAGE;
	if (nblk < ZRAM_WB_BATCH || nblk > INT_MAX || pp->sectorsize > ZRAM_PAGE) {
		g_topology_unlock();
		return (EINVAL);
	}
	if (zram_bk_gp == NULL)
		zram_bk_gp = g_new_geomf(&zram_bk_class, "zram_backing");
	cp = g_new_consumer(zram_bk_gp);
	error = g_attach(cp, pp);
	if (error == 0) {
		error = g_access(cp, 1, 1, 1);	/* exclusive: nobody else writes it */
		if (error != 0)
			g_detach(cp);
	}
	if (error != 0) {
		g_destroy_consumer(cp);
		g_topology_unlock();
		return (error);
	}
	g_topology_unlock();

	zram_bk_map = bit_alloc((int)nblk, M_ZRAM, M_WAITOK);
	zram_bk_nblk = (int)nblk;
	zram_bk_hint = 0;
	zram_wb_buf = malloc((size_t)ZRAM_WB_BATCH * ZRAM_PAGE, M_ZRAM, M_WAITOK);
	zram_wb_recs = malloc(sizeof(*zram_wb_recs) * ZRAM_WB_BATCH, M_ZRAM,
	    M_WAITOK | M_ZERO);
	strlcpy(zram_bk_name, name, sizeof(zram_bk_name));
	zram_bk_gone = 0;
	atomic_store_rel_ptr((volatile uintptr_t *)&zram_bk_cp, (uintptr_t)cp);
	callout_reset(&zram_wb_callout, ZRAM_WB_TICK * hz, zram_wb_tick, NULL);
	printf("atrium_zram: backing store %s, %juMB\n", name,
	    (uintmax_t)(nblk * ZRAM_PAGE >> 20));
	return (0);
}

/* Unload only: every WB slot has been freed and no I/O is in flight. */
static void
zram_bk_detach(void)
{
	struct g_consumer *cp = zram_bk_cp;

	if (cp == NULL)
		return;
	zram_bk_cp = NULL;
	g_topology_lock();
	g_access(cp, -1, -1, -1);
	g_detach(cp);
	g_destroy_consumer(cp);
	g_topology_unlock();
	free(zram_bk_map, M_ZRAM);
	free(zram_wb_buf, M_ZRAM);
	free(zram_wb_recs, M_ZRAM);
	zram_bk_map = NULL;
}

static int
zram_backing_sysctl(SYSCTL_HANDLER_ARGS)
{
	char name[sizeof(zram_bk_name)];
	const char *p;
	int error;

	strlcpy(name, zram_bk_name, sizeof(name));
	error = sysctl_handle_string(oidp, name, sizeof(name), req);
	if (error != 0 || req->newptr == NULL)
		return (error);
	p = name;
	if (strncmp(p, "/dev/", 5) == 0)
		p += 5;
	return (zram_bk_attach(p));
}

/*
 * Block-device strategy: map each page of a BIO to a store slot. Swap (and dd
 * bs=4096) do page-aligned I/O; reject anything else. This is /dev/zram0's hot
 * path: store on write, load on read. Pages on the backing store are read with
 * child bios, and a store the pool refuses is written through to disk, so the
 * bio may complete after this returns (zram_parent_drop).
 */
static void
zram_strategy(struct bio *bp)
{
	struct zram_slot *sl;
	uint8_t *data;
	uint64_t blk;
	uint16_t len;
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
	bp->bio_children = 1;	/* this loop; child bios add to it */
	bp->bio_error = 0;
	data = bp->bio_data;
	for (off = 0; off < bp->bio_length; off += ZRAM_PAGE) {
		idx = (uint32_t)((bp->bio_offset + off) / ZRAM_PAGE);
		if (idx >= zram_nslots) { err = EINVAL; break; }
		sl = zram_slot(idx, bp->bio_cmd == BIO_WRITE);
		if (bp->bio_cmd == BIO_READ) {
			if (sl == NULL) {	/* never written: reads as zeros */
				memset(data + off, 0, ZRAM_PAGE);
				ZRAM_ADD(zram_reads, 1);
				continue;
			}
			err = zram_load(sl, data + off, &blk, &len);
			if (err == EJUSTRETURN)
				err = zram_bk_read(bp, data + off, blk, len);
		} else {
			if (sl == NULL) {
				err = ENOMEM;	/* no memory for the table chunk */
				break;
			}
			err = zram_store(sl, data + off);
			if (err != 0 && zram_bk_usable())
				err = zram_bk_write_through(bp, sl, data + off);
		}
		if (err != 0)
			break;
	}
	zram_parent_drop(bp, err);
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
	if (zram_load(&t[0], back, NULL, NULL) != 0 || memcmp(p, back, ZRAM_PAGE) != 0) fails++;

	/* slot 1: half-zero + low-entropy -> COMP. */
	memset(p, 0, ZRAM_PAGE);
	for (i = ZRAM_PAGE / 2; i < ZRAM_PAGE; i++) p[i] = (char)(i & 0x1f);
	zram_store(&t[1], p);
	if (zram_load(&t[1], back, NULL, NULL) != 0 || memcmp(p, back, ZRAM_PAGE) != 0) fails++;

	/* slot 2: random -> incompressible -> RAW fallback. */
	arc4random_buf(p, ZRAM_PAGE);
	zram_store(&t[2], p);
	if (zram_load(&t[2], back, NULL, NULL) != 0 || memcmp(p, back, ZRAM_PAGE) != 0) fails++;

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
	    atomic_load_64(&zram_pages_raw) + atomic_load_64(&zram_pages_wb)) *
	    ZRAM_PAGE;
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
/* Backing store and writeback (increment 5). */
SYSCTL_PROC(_kern_zram, OID_AUTO, backing_dev,
    CTLTYPE_STRING | CTLFLAG_RW | CTLFLAG_MPSAFE, NULL, 0, zram_backing_sysctl, "A",
    "GEOM provider to write cold and incompressible pages to (set once)");
SYSCTL_INT(_kern_zram, OID_AUTO, backing_blocks, CTLFLAG_RD, &zram_bk_nblk, 0,
    "Backing store size in 4 KiB blocks");
SYSCTL_U64(_kern_zram, OID_AUTO, backing_used, CTLFLAG_RD, &zram_bk_used, 0,
    "Backing store blocks in use");
SYSCTL_INT(_kern_zram, OID_AUTO, idle_secs, CTLFLAG_RWTUN, &zram_idle_secs, 0,
    "A page not read for this long (one to two periods) is written back");
SYSCTL_U64(_kern_zram, OID_AUTO, pages_wb, CTLFLAG_RD, &zram_pages_wb, 0,
    "Pages held on the backing store");
SYSCTL_U64(_kern_zram, OID_AUTO, wb_pages, CTLFLAG_RD, &zram_wb_pages, 0,
    "Pages moved from RAM to the backing store by writeback, since load");
SYSCTL_U64(_kern_zram, OID_AUTO, wb_through, CTLFLAG_RD, &zram_wb_through, 0,
    "Swap-outs the pool refused that were written straight to disk, since load");
SYSCTL_U64(_kern_zram, OID_AUTO, wb_reads, CTLFLAG_RD, &zram_wb_reads, 0,
    "Swap-ins served from the backing store, since load");
SYSCTL_U64(_kern_zram, OID_AUTO, wb_fail, CTLFLAG_RD, &zram_wb_fail, 0,
    "Backing store writes that failed, since load");
SYSCTL_U64(_kern_zram, OID_AUTO, wb_scans, CTLFLAG_RD, &zram_wb_scans, 0,
    "Writeback passes over the slot table, since load");
SYSCTL_U64(_kern_zram, OID_AUTO, disksize, CTLFLAG_RD, &zram_disksize_tun, 0,
    "Device size in bytes (tunable, read at load; default physical RAM)");
SYSCTL_U64(_kern_zram, OID_AUTO, table_bytes, CTLFLAG_RD, &zram_table_bytes, 0,
    "Slot-table memory allocated so far (chunks are allocated on first write)");
SYSCTL_U64(_kern_zram, OID_AUTO, table_spare_used, CTLFLAG_RD,
    &zram_chunk_spare_used, 0,
    "Table chunks taken from the spare because malloc refused, since load");
SYSCTL_U64(_kern_zram, OID_AUTO, fail_table, CTLFLAG_RD, &zram_fail_table, 0,
    "Writes failed for lack of memory for a table chunk, since load");
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
	struct zram_slot *ch;

	zpool_refill(&zram_pool);
	if (atomic_load_ptr(&zram_spare_chunk) == NULL) {
		ch = malloc(sizeof(struct zram_slot) * ZRAM_CHUNK_SLOTS, M_ZRAM,
		    M_WAITOK | M_ZERO);
		if (!atomic_cmpset_ptr((volatile uintptr_t *)&zram_spare_chunk, 0,
		    (uintptr_t)ch))
			free(ch, M_ZRAM);
	}
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
		mtx_init(&zram_bk_mtx, "zram backing", NULL, MTX_DEF);
		zram_io_zone = uma_zcreate("zram_io", sizeof(struct zram_io), NULL,
		    NULL, NULL, NULL, UMA_ALIGN_PTR, 0);
		uma_zone_reserve(zram_io_zone, 64);	/* short-lived: refills itself */
		uma_prealloc(zram_io_zone, 64);
		zram_wb_tq = taskqueue_create("zram_wb", M_WAITOK,
		    taskqueue_thread_enqueue, &zram_wb_tq);
		taskqueue_start_threads(&zram_wb_tq, 1, PVM, "zram writeback");
		TASK_INIT(&zram_wb_task, 0, zram_wb_run, NULL);
		callout_init(&zram_wb_callout, 1);
		g_modevent(NULL, MOD_LOAD, &zram_bk_class);
		/* Device size: kern.zram.disksize, default physical RAM. */
		zram_disksize_tun = ptoa((uint64_t)physmem);
		TUNABLE_UINT64_FETCH("kern.zram.disksize", &zram_disksize_tun);
		zram_nslots = (uint32_t)MIN(MAX(zram_disksize_tun / ZRAM_PAGE,
		    ZRAM_MIN_SLOTS), ZRAM_MAX_SLOTS);
		zram_nslots = roundup2(zram_nslots, ZRAM_CHUNK_SLOTS);
		zram_disksize_tun = (uint64_t)zram_nslots * ZRAM_PAGE;
		zram_ndir = zram_nslots >> ZRAM_CHUNK_SHIFT;
		zram_dir = malloc(sizeof(*zram_dir) * zram_ndir, M_ZRAM,
		    M_WAITOK | M_ZERO);
		zram_spare_chunk = malloc(sizeof(struct zram_slot) * ZRAM_CHUNK_SLOTS,
		    M_ZRAM, M_WAITOK | M_ZERO);
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
		zram_disk->d_mediasize = (off_t)zram_nslots * ZRAM_PAGE;
		zram_disk->d_maxsize = ZRAM_PAGE * 16;
		zram_disk->d_flags = DISKFLAG_CANDELETE;	/* swapon -E trims into us */
		zram_disk->d_delmaxsize = (off_t)zram_nslots * ZRAM_PAGE;
		disk_create(zram_disk, DISK_VERSION);
		/* Backing store named in loader.conf / kenv, if any. */
		{
			char bk[sizeof(zram_bk_name)];

			if (TUNABLE_STR_FETCH("kern.zram.backing_dev", bk, sizeof(bk)) &&
			    bk[0] != '\0' && zram_bk_attach(strncmp(bk, "/dev/", 5) == 0 ?
			    bk + 5 : bk) != 0)
				printf("atrium_zram: backing store %s not attached\n", bk);
		}
		printf("atrium_zram: /dev/zram0 ready (%u pages = %juMB, lazy table, "
		    "limit %juMB, reserve %dKB)\n",
		    zram_nslots, (uintmax_t)((off_t)zram_nslots * ZRAM_PAGE >> 20),
		    (uintmax_t)(zram_pool.limit >> 20), zram_pool.reserve_n * ZP_CHUNK / 1024);
		return (0);
	case MOD_UNLOAD:
		disk_destroy(zram_disk);
		callout_drain(&zram_wb_callout);
		taskqueue_drain(zram_wb_tq, &zram_wb_task);
		taskqueue_free(zram_wb_tq);
		taskqueue_drain(taskqueue_thread, &zram_refill_task);
		for (i = 0; i < zram_ndir; i++) {
			uint32_t j;

			if (zram_dir[i] == NULL)
				continue;
			for (j = 0; j < ZRAM_CHUNK_SLOTS; j++)
				zram_free_slot(&zram_dir[i][j]);
			free(zram_dir[i], M_ZRAM);
		}
		free(zram_dir, M_ZRAM);
		free(zram_spare_chunk, M_ZRAM);
		zram_bk_detach();
		g_topology_lock();
		if (zram_bk_gp != NULL)
			g_destroy_geom(zram_bk_gp);
		zram_bk_gp = NULL;
		g_topology_unlock();
		g_modevent(NULL, MOD_UNLOAD, &zram_bk_class);
		uma_zdestroy(zram_io_zone);
		mtx_destroy(&zram_bk_mtx);
		zram_dir = NULL;
		zram_spare_chunk = NULL;
		zram_table_bytes = 0;
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
