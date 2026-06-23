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
#include <sys/sysctl.h>
#include <geom/geom_disk.h>

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

static MALLOC_DEFINE(M_ZRAM, "zram", "atrium zram store");

/* One stored page. SAME = a same-filled page (incl. all-zero) kept as a tag, no
 * buffer; COMP = zstd-compressed; RAW = incompressible, stored verbatim. */
enum zram_state { ZRAM_EMPTY = 0, ZRAM_SAME, ZRAM_COMP, ZRAM_RAW };
struct zram_slot {
	enum zram_state	state;
	uint32_t	len;	/* COMP/RAW byte length */
	uint8_t		fill;	/* SAME fill byte */
	void		*buf;	/* COMP/RAW buffer, NULL for SAME/EMPTY */
};

static struct zram_slot	*zram_slots;
static ZSTD_CCtx	*zram_cctx;	/* pre-allocated — no malloc on the hot path */
static ZSTD_DCtx	*zram_dctx;
static struct mtx	 zram_mtx;	/* serializes the shared contexts (per-CPU = refinement) */
static uint64_t		 zram_phys_bytes;	/* total physical bytes the store holds */
static struct disk	*zram_disk;	/* /dev/zram0 — the swap block device */

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

static void
zram_free_slot(struct zram_slot *s)
{
	if (s->buf != NULL) {
		zram_phys_bytes -= s->len;
		free(s->buf, M_ZRAM);
		s->buf = NULL;
	}
	s->state = ZRAM_EMPTY;
	s->len = 0;
}

/* Store `page` at slot `idx`. Same-filled -> tag (0 bytes); else compress; if it
 * doesn't shrink, store raw. Returns 0, or ENOMEM. Caller holds nothing. */
static int
zram_store(uint32_t idx, const void *page)
{
	static char comp[ZRAM_PAGE + 512]; /* >= ZSTD_compressBound(4096)=4174; guarded by mtx */
	struct zram_slot *s = &zram_slots[idx];
	uint8_t fill;
	size_t clen;
	int err = 0;

	mtx_lock(&zram_mtx);
	zram_free_slot(s);

	if (zram_same_filled(page, &fill)) {
		s->state = ZRAM_SAME;
		s->fill = fill;
		goto out; /* 0 physical bytes — the zero/same-page win */
	}
	clen = ZSTD_compressCCtx(zram_cctx, comp, sizeof(comp), page, ZRAM_PAGE, ZRAM_LEVEL);
	if (!ZSTD_isError(clen) && clen < ZRAM_PAGE) {
		s->buf = malloc(clen, M_ZRAM, M_NOWAIT);
		if (s->buf == NULL) { err = ENOMEM; goto out; }
		memcpy(s->buf, comp, clen);
		s->state = ZRAM_COMP;
		s->len = clen;
	} else {
		/* incompressible (or codec error) → store verbatim. */
		s->buf = malloc(ZRAM_PAGE, M_ZRAM, M_NOWAIT);
		if (s->buf == NULL) { err = ENOMEM; goto out; }
		memcpy(s->buf, page, ZRAM_PAGE);
		s->state = ZRAM_RAW;
		s->len = ZRAM_PAGE;
	}
	zram_phys_bytes += s->len;
out:
	mtx_unlock(&zram_mtx);
	return (err);
}

/* Load slot `idx` into `page` (exactly ZRAM_PAGE bytes). 0, or EIO/ENOENT. */
static int
zram_load(uint32_t idx, void *page)
{
	struct zram_slot *s = &zram_slots[idx];
	size_t r;
	int err = 0;

	mtx_lock(&zram_mtx);
	switch (s->state) {
	case ZRAM_SAME:
		memset(page, s->fill, ZRAM_PAGE);
		break;
	case ZRAM_RAW:
		memcpy(page, s->buf, ZRAM_PAGE);
		break;
	case ZRAM_COMP:
		r = ZSTD_decompressDCtx(zram_dctx, page, ZRAM_PAGE, s->buf, s->len);
		if (ZSTD_isError(r) || r != ZRAM_PAGE)
			err = EIO;
		break;
	default:
		/* never-written block reads as zeros (standard block-device / swap). */
		memset(page, 0, ZRAM_PAGE);
	}
	mtx_unlock(&zram_mtx);
	return (err);
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
		    zram_load(idx, data + off) : zram_store(idx, data + off);
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
	char *p, *back, res[160];
	int i, fails = 0;
	uint64_t before;

	p = malloc(ZRAM_PAGE, M_ZRAM, M_WAITOK);
	back = malloc(ZRAM_PAGE, M_ZRAM, M_WAITOK);
	before = zram_phys_bytes;

	/* slot 0: all-zero -> SAME, 0 physical bytes. */
	memset(p, 0, ZRAM_PAGE);
	zram_store(0, p);
	if (zram_load(0, back) != 0 || memcmp(p, back, ZRAM_PAGE) != 0) fails++;

	/* slot 1: half-zero + low-entropy -> COMP. */
	memset(p, 0, ZRAM_PAGE);
	for (i = ZRAM_PAGE / 2; i < ZRAM_PAGE; i++) p[i] = (char)(i & 0x1f);
	zram_store(1, p);
	if (zram_load(1, back) != 0 || memcmp(p, back, ZRAM_PAGE) != 0) fails++;

	/* slot 2: random -> incompressible -> RAW fallback. */
	arc4random_buf(p, ZRAM_PAGE);
	zram_store(2, p);
	if (zram_load(2, back) != 0 || memcmp(p, back, ZRAM_PAGE) != 0) fails++;

	snprintf(res, sizeof(res),
	    "%s | 3 pages (zero/compressible/random) = 12288 logical -> %ju physical bytes "
	    "[slot0=%s slot1=%s(%u) slot2=%s(%u)]",
	    fails ? "FAIL" : "OK round-trips", (uintmax_t)(zram_phys_bytes - before),
	    "SAME", zram_slots[1].state == ZRAM_COMP ? "COMP" : "?", zram_slots[1].len,
	    zram_slots[2].state == ZRAM_RAW ? "RAW" : "?", zram_slots[2].len);

	/* leave the store clean for repeat reads. */
	zram_free_slot(&zram_slots[0]);
	zram_free_slot(&zram_slots[1]);
	zram_free_slot(&zram_slots[2]);
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

static int
zram_modevent(module_t mod __unused, int type, void *data __unused)
{
	uint32_t i;

	switch (type) {
	case MOD_LOAD:
		zram_cctx = ZSTD_createCCtx();
		zram_dctx = ZSTD_createDCtx();
		if (zram_cctx == NULL || zram_dctx == NULL)
			return (ENOMEM);
		zram_slots = malloc(sizeof(struct zram_slot) * ZRAM_NSLOTS, M_ZRAM,
		    M_WAITOK | M_ZERO);
		mtx_init(&zram_mtx, "zram", NULL, MTX_DEF);
		/* /dev/zram0 — a compressed RAM block device (page-sized sectors). */
		zram_disk = disk_alloc();
		zram_disk->d_strategy = zram_strategy;
		zram_disk->d_name = "zram";
		zram_disk->d_unit = 0;
		zram_disk->d_sectorsize = ZRAM_PAGE;
		zram_disk->d_mediasize = (off_t)ZRAM_NSLOTS * ZRAM_PAGE;
		zram_disk->d_maxsize = ZRAM_PAGE * 16;
		disk_create(zram_disk, DISK_VERSION);
		printf("atrium_zram: /dev/zram0 ready (%d pages = %juMB, pre-allocated zstd)\n",
		    ZRAM_NSLOTS, (uintmax_t)((off_t)ZRAM_NSLOTS * ZRAM_PAGE >> 20));
		return (0);
	case MOD_UNLOAD:
		disk_destroy(zram_disk);
		for (i = 0; i < ZRAM_NSLOTS; i++)
			zram_free_slot(&zram_slots[i]);
		free(zram_slots, M_ZRAM);
		ZSTD_freeCCtx(zram_cctx);
		ZSTD_freeDCtx(zram_dctx);
		mtx_destroy(&zram_mtx);
		return (0);
	default:
		return (EOPNOTSUPP);
	}
}

static moduledata_t zram_mod = { "atrium_zram", zram_modevent, NULL };
DECLARE_MODULE(atrium_zram, zram_mod, SI_SUB_DRIVERS, SI_ORDER_MIDDLE);
