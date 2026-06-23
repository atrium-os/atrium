/*-
 * SPDX-License-Identifier: BSD-2-Clause
 *
 * atrium-zram — compressed-RAM swap codec core (atrium-memory-pressure.md Phase 3).
 *
 * The kernel half of the zram-equivalent the cost model (gpusim compress.rs)
 * specified: a non-destructive, cooperation-free reclaim tier that compresses cold
 * anon pages in RAM rather than swapping to flash or killing apps. This module is
 * the *codec core* — the reusable per-page compress/decompress over the kernel's
 * in-tree zstd (ZSTD_compress/ZSTD_decompress are global symbols when ZSTDIO is
 * compiled in, which it is) — verified by a self-test, with NO swap-path risk. The
 * compressed page store + the block device + swapon are the next increments.
 */

#include <sys/param.h>
#include <sys/systm.h>
#include <sys/kernel.h>
#include <sys/malloc.h>
#include <sys/module.h>
#include <sys/sysctl.h>

/*
 * In-kernel zstd one-shot API (resolved at load against the kernel's global
 * ZSTD_* symbols). Declared here to avoid pulling the contrib/zstd headers into a
 * module build.
 */
size_t ZSTD_compress(void *dst, size_t dstCap, const void *src, size_t srcSize, int level);
size_t ZSTD_decompress(void *dst, size_t dstCap, const void *src, size_t srcSize);
size_t ZSTD_compressBound(size_t srcSize);
unsigned ZSTD_isError(size_t code);

#define	ZRAM_PAGE	4096
#define	ZRAM_LEVEL	3	/* low zstd level — fast, zram-class */

static MALLOC_DEFINE(M_ZRAM, "zram", "atrium zram codec");

/*
 * Compress one page. Returns the compressed length, or 0 if it errored or did not
 * shrink (the caller then stores the page uncompressed — a real zram always keeps
 * an uncompressed fallback for incompressible pages).
 *
 * NOTE: ZSTD_compress() allocates a context internally each call. The swap path
 * (called under memory pressure) must instead use a PRE-ALLOCATED per-CPU
 * ZSTD_CCtx (ZSTD_createCCtx at init, ZSTD_compressCCtx per page) — allocating
 * during reclaim is exactly what you cannot do. The codec core proves the
 * round-trip; the pager wires the pre-allocated contexts.
 */
static size_t
zram_compress_page(const void *page, void *dst, size_t dstcap)
{
	size_t r = ZSTD_compress(dst, dstcap, page, ZRAM_PAGE, ZRAM_LEVEL);

	if (ZSTD_isError(r) || r >= ZRAM_PAGE)
		return (0);
	return (r);
}

/* Decompress one page back to exactly ZRAM_PAGE bytes; 0 on success, EIO on error. */
static int
zram_decompress_page(const void *src, size_t srclen, void *page)
{
	size_t r = ZSTD_decompress(page, ZRAM_PAGE, src, srclen);

	return ((ZSTD_isError(r) || r != ZRAM_PAGE) ? EIO : 0);
}

/* Self-test: compress + decompress a realistically-compressible page, verify the
 * round-trip, report the ratio. Read kern.zram.selftest. */
static int
zram_selftest(SYSCTL_HANDLER_ARGS)
{
	char *orig, *comp, *back, res[96];
	size_t clen, bound, rx;
	int i;

	orig = malloc(ZRAM_PAGE, M_ZRAM, M_WAITOK | M_ZERO);
	back = malloc(ZRAM_PAGE, M_ZRAM, M_WAITOK);
	bound = ZSTD_compressBound(ZRAM_PAGE);
	comp = malloc(bound, M_ZRAM, M_WAITOK);

	/* half zeros + half a repeating low-entropy pattern — like real cold anon. */
	for (i = ZRAM_PAGE / 2; i < ZRAM_PAGE; i++)
		orig[i] = (char)(i & 0x1f);

	clen = zram_compress_page(orig, comp, bound);
	if (clen == 0)
		snprintf(res, sizeof(res), "FAIL: compress (error/incompressible)");
	else if (zram_decompress_page(comp, clen, back) != 0)
		snprintf(res, sizeof(res), "FAIL: decompress");
	else if (memcmp(orig, back, ZRAM_PAGE) != 0)
		snprintf(res, sizeof(res), "FAIL: round-trip mismatch");
	else {
		rx = (size_t)ZRAM_PAGE * 100 / clen; /* ratio ×100 */
		snprintf(res, sizeof(res), "OK: 4096 -> %zu bytes, ratio %zu.%02zux",
		    clen, rx / 100, rx % 100);
	}
	free(orig, M_ZRAM);
	free(comp, M_ZRAM);
	free(back, M_ZRAM);
	return (sysctl_handle_string(oidp, res, sizeof(res), req));
}

static SYSCTL_NODE(_kern, OID_AUTO, zram, CTLFLAG_RD | CTLFLAG_MPSAFE, NULL,
    "atrium compressed-RAM swap codec");
SYSCTL_PROC(_kern_zram, OID_AUTO, selftest,
    CTLTYPE_STRING | CTLFLAG_RD | CTLFLAG_MPSAFE, NULL, 0,
    zram_selftest, "A", "Compress+decompress a test page, verify round-trip");

static int
zram_modevent(module_t mod __unused, int type, void *data __unused)
{
	switch (type) {
	case MOD_LOAD:
		printf("atrium_zram: zstd codec core loaded (ZSTD_compressBound(4096)=%zu)\n",
		    ZSTD_compressBound(ZRAM_PAGE));
		return (0);
	case MOD_UNLOAD:
		return (0);
	default:
		return (EOPNOTSUPP);
	}
}

static moduledata_t zram_mod = { "atrium_zram", zram_modevent, NULL };
DECLARE_MODULE(atrium_zram, zram_mod, SI_SUB_DRIVERS, SI_ORDER_MIDDLE);
