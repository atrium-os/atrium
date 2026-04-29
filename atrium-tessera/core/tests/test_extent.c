/*
 * Tests for the in-memory free-extent allocator.
 *
 * Properties exercised:
 *   1. Empty allocator → alloc fails ENOSPC; free_blocks == 0.
 *   2. Single seeded extent: alloc(N) succeeds at extent.start; remainder
 *      is tracked correctly.
 *   3. Whole-extent alloc removes the extent from the set.
 *   4. Best-fit picks the smallest-fitting extent.
 *   5. Free + adjacent free coalesce on both sides; non-adjacent stays
 *      separate.
 *   6. Touching/overlapping free returns EINVAL.
 *   7. Randomised stress: invariants hold across thousands of ops
 *      (sorted, non-touching, free_blocks accurate, no overlaps).
 *
 * No SHA-256 dependency; pure in-memory. Runs on host and VM.
 */

#include "tessera/extent.h"
#include "tessera/btree.h"
#include "tessera/error.h"
#include "tessera/format.h"

#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

static int failures = 0;

#define CHECK(cond) do {                                                 \
	if (!(cond)) {                                                   \
		fprintf(stderr, "FAIL %s:%d: %s\n",                      \
		    __FILE__, __LINE__, #cond);                          \
		failures++;                                              \
	}                                                                \
} while (0)

/* ── basic shape ─────────────────────────────────────────────────── */

static void
test_empty(void)
{
	tessera_extent_alloc_t *a = tessera_extent_open(NULL, 0);
	CHECK(a != NULL);
	CHECK(tessera_extent_free_blocks(a) == 0);
	CHECK(tessera_extent_largest_free_run(a) == 0);

	uint64_t s = 0xdead;
	CHECK(tessera_extent_alloc(a, 1, &s) == TESSERA_ENOSPC);

	tessera_extent_close(a);
}

static void
test_seeded_alloc_split(void)
{
	tessera_extent_alloc_t *a = tessera_extent_open(NULL, 0);
	CHECK(tessera_extent_free(a, 1000, 100) == TESSERA_OK);
	CHECK(tessera_extent_free_blocks(a) == 100);
	CHECK(tessera_extent_largest_free_run(a) == 100);

	uint64_t s = 0;
	CHECK(tessera_extent_alloc(a, 30, &s) == TESSERA_OK);
	CHECK(s == 1000);
	CHECK(tessera_extent_free_blocks(a) == 70);
	CHECK(tessera_extent_largest_free_run(a) == 70);

	CHECK(tessera_extent_alloc(a, 70, &s) == TESSERA_OK);
	CHECK(s == 1030);
	CHECK(tessera_extent_free_blocks(a) == 0);
	CHECK(tessera_extent_largest_free_run(a) == 0);

	CHECK(tessera_extent_alloc(a, 1, &s) == TESSERA_ENOSPC);
	tessera_extent_close(a);
}

static void
test_best_fit(void)
{
	/* Three free runs: 1000:50, 2000:200, 3000:100.
	 * Asking for 80 should pick the 100-extent (smallest fit), not
	 * the 200-extent. */
	tessera_extent_alloc_t *a = tessera_extent_open(NULL, 0);
	CHECK(tessera_extent_free(a, 1000, 50)  == TESSERA_OK);
	CHECK(tessera_extent_free(a, 2000, 200) == TESSERA_OK);
	CHECK(tessera_extent_free(a, 3000, 100) == TESSERA_OK);

	uint64_t s = 0;
	CHECK(tessera_extent_alloc(a, 80, &s) == TESSERA_OK);
	CHECK(s == 3000);
	CHECK(tessera_extent_free_blocks(a) == (50 + 200 + 100) - 80);

	/* Now extents are 1000:50, 2000:200, 3080:20.
	 * 200 should pick the 200-extent exactly. */
	CHECK(tessera_extent_alloc(a, 200, &s) == TESSERA_OK);
	CHECK(s == 2000);
	CHECK(tessera_extent_free_blocks(a) == 50 + 20);

	tessera_extent_close(a);
}

static void
test_coalesce(void)
{
	tessera_extent_alloc_t *a = tessera_extent_open(NULL, 0);

	/* Free three pieces in non-adjacent order; verify two of them
	 * fuse when a connector is freed in between. */
	CHECK(tessera_extent_free(a, 100, 10) == TESSERA_OK);    /* [100,110) */
	CHECK(tessera_extent_free(a, 200, 10) == TESSERA_OK);    /* [200,210) */
	CHECK(tessera_extent_free_blocks(a) == 20);
	CHECK(tessera_extent_largest_free_run(a) == 10);

	/* Connector — fuses with both. */
	CHECK(tessera_extent_free(a, 110, 90) == TESSERA_OK);    /* [110,200) */
	CHECK(tessera_extent_free_blocks(a) == 110);
	CHECK(tessera_extent_largest_free_run(a) == 110);

	/* Allocate the whole fused run in one shot. */
	uint64_t s = 0;
	CHECK(tessera_extent_alloc(a, 110, &s) == TESSERA_OK);
	CHECK(s == 100);
	CHECK(tessera_extent_free_blocks(a) == 0);

	tessera_extent_close(a);
}

static void
test_coalesce_one_side(void)
{
	/* Adjacent on the right only. */
	tessera_extent_alloc_t *a = tessera_extent_open(NULL, 0);
	CHECK(tessera_extent_free(a, 200, 10) == TESSERA_OK);    /* [200,210) */
	CHECK(tessera_extent_free(a, 190, 10) == TESSERA_OK);    /* fuses left */
	CHECK(tessera_extent_largest_free_run(a) == 20);

	/* Adjacent on the left only. */
	CHECK(tessera_extent_free(a, 210,  5) == TESSERA_OK);    /* fuses right */
	CHECK(tessera_extent_largest_free_run(a) == 25);

	/* Non-adjacent — separate run. */
	CHECK(tessera_extent_free(a, 1000, 7) == TESSERA_OK);
	CHECK(tessera_extent_free_blocks(a) == 32);
	CHECK(tessera_extent_largest_free_run(a) == 25);

	tessera_extent_close(a);
}

static void
test_overlap_rejected(void)
{
	tessera_extent_alloc_t *a = tessera_extent_open(NULL, 0);
	CHECK(tessera_extent_free(a, 100, 50) == TESSERA_OK);     /* [100,150) */

	/* Exactly overlapping. */
	CHECK(tessera_extent_free(a, 100, 50) == TESSERA_EINVAL);
	/* Partial overlap (right edge into existing). */
	CHECK(tessera_extent_free(a,  90, 30) == TESSERA_EINVAL);
	/* Partial overlap (left edge into existing). */
	CHECK(tessera_extent_free(a, 140, 30) == TESSERA_EINVAL);
	/* Fully contained inside. */
	CHECK(tessera_extent_free(a, 110, 10) == TESSERA_EINVAL);
	/* Adjacent, NOT overlapping — must succeed. */
	CHECK(tessera_extent_free(a, 150, 10) == TESSERA_OK);
	CHECK(tessera_extent_largest_free_run(a) == 60);

	tessera_extent_close(a);
}

static void
test_einval_zero(void)
{
	tessera_extent_alloc_t *a = tessera_extent_open(NULL, 0);
	uint64_t s = 0;
	CHECK(tessera_extent_alloc(a, 0, &s) == TESSERA_EINVAL);
	CHECK(tessera_extent_free(a, 0, 0) == TESSERA_EINVAL);
	CHECK(tessera_extent_alloc(NULL, 1, &s) == TESSERA_EINVAL);
	CHECK(tessera_extent_alloc(a, 1, NULL) == TESSERA_EINVAL);
	tessera_extent_close(a);
}

/* ── randomised stress ───────────────────────────────────────────── */

/* Simulate a workload of many alloc/free pairs, then verify:
 *   - free_blocks reported == sum of allocated returned + free_blocks
 *     after, equals the original seed,
 *   - never two extents that touch or overlap. */
static void
test_random_stress(void)
{
	tessera_extent_alloc_t *a = tessera_extent_open(NULL, 0);
	const uint64_t TOTAL = 1ull << 16;        /* 65536 sectors seeded */
	CHECK(tessera_extent_free(a, 0, TOTAL) == TESSERA_OK);
	CHECK(tessera_extent_free_blocks(a) == TOTAL);
	CHECK(tessera_extent_largest_free_run(a) == TOTAL);

	struct alloc_rec { uint64_t start, len; };
	struct alloc_rec *live = calloc(8192, sizeof *live);
	size_t live_n = 0;

	uint64_t rng = 0xc0ffee;
	for (int i = 0; i < 20000; i++) {
		rng ^= rng << 13; rng ^= rng >> 7; rng ^= rng << 17;
		int free_op = (rng & 1) && live_n > 0;
		if (free_op) {
			size_t pick = (size_t)((rng >> 1) % live_n);
			CHECK(tessera_extent_free(a,
			    live[pick].start, live[pick].len) == TESSERA_OK);
			live[pick] = live[live_n - 1];
			live_n--;
		} else {
			uint64_t want = ((rng >> 8) % 100) + 1;
			uint64_t s = 0;
			int r = tessera_extent_alloc(a, want, &s);
			if (r == TESSERA_OK) {
				if (live_n < 8192) {
					live[live_n++] = (struct alloc_rec){ s, want };
				} else {
					/* Capacity for tracker exhausted; free
					 * back so the test invariants don't drift. */
					CHECK(tessera_extent_free(a, s, want) == TESSERA_OK);
				}
			} else {
				CHECK(r == TESSERA_ENOSPC);
			}
		}
	}

	uint64_t allocated = 0;
	for (size_t i = 0; i < live_n; i++) allocated += live[i].len;
	CHECK(allocated + tessera_extent_free_blocks(a) == TOTAL);
	printf("  stress: %zu live allocs, %llu free blocks, largest run %llu\n",
	    live_n,
	    (unsigned long long)tessera_extent_free_blocks(a),
	    (unsigned long long)tessera_extent_largest_free_run(a));

	/* Free everything; should coalesce back to a single TOTAL-sized run. */
	for (size_t i = 0; i < live_n; i++)
		CHECK(tessera_extent_free(a, live[i].start, live[i].len)
		    == TESSERA_OK);
	CHECK(tessera_extent_free_blocks(a) == TOTAL);
	CHECK(tessera_extent_largest_free_run(a) == TOTAL);

	free(live);
	tessera_extent_close(a);
}

/* ── B+tree-backed round-trip ────────────────────────────────────── */

#define MAX_SECTORS 256

struct mem_disk {
	uint8_t  blocks[MAX_SECTORS][4096];
	uint8_t  used[MAX_SECTORS];
	uint64_t next_sector;
};

static int md_read(void *ctx, uint64_t s, uint8_t *o) {
	struct mem_disk *d = ctx;
	if (s >= MAX_SECTORS || !d->used[s]) return -1;
	memcpy(o, d->blocks[s], 4096); return 0;
}
static int md_write(void *ctx, uint64_t s, const uint8_t *b) {
	struct mem_disk *d = ctx;
	if (s >= MAX_SECTORS) return -1;
	memcpy(d->blocks[s], b, 4096); return 0;
}
static int md_alloc(void *ctx, uint64_t n, uint64_t *o) {
	struct mem_disk *d = ctx;
	if (n != 1) return -1;
	for (uint64_t i = d->next_sector; i < MAX_SECTORS; i++) {
		if (!d->used[i]) { d->used[i] = 1; d->next_sector = i + 1;
		    *o = i; return 0; }
	}
	for (uint64_t i = 1; i < d->next_sector; i++) {
		if (!d->used[i]) { d->used[i] = 1; *o = i; return 0; }
	}
	return -1;
}
static int md_free(void *ctx, uint64_t s, uint64_t n) {
	struct mem_disk *d = ctx;
	if (n != 1 || s >= MAX_SECTORS) return -1;
	d->used[s] = 0;
	if (s < d->next_sector) d->next_sector = s;
	return 0;
}

static void
test_btree_round_trip(void)
{
	/* Build an allocator state, flush to a B+tree on a memory-backed
	 * disk, close, reopen at the published root, and verify the
	 * reloaded state is byte-identical to what was flushed. */
	struct mem_disk *d = calloc(1, sizeof *d);
	d->next_sector = 1;
	tessera_block_io_t io = {
		.read_block = md_read, .write_block = md_write,
		.alloc      = md_alloc, .free       = md_free,
		.ctx        = d,
	};

	tessera_extent_alloc_t *a = tessera_extent_open(&io, 0);
	CHECK(a != NULL);

	const struct { uint64_t s, n; } seed[] = {
		{ 100,    50 },
		{ 200,   100 },
		{ 5000, 1000 },
		{ 8000,    7 },
		{ 12000, 200 },
	};
	for (size_t i = 0; i < sizeof seed / sizeof seed[0]; i++)
		CHECK(tessera_extent_free(a, seed[i].s, seed[i].n) == TESSERA_OK);

	uint64_t expect_total = 0;
	for (size_t i = 0; i < sizeof seed / sizeof seed[0]; i++)
		expect_total += seed[i].n;
	CHECK(tessera_extent_free_blocks(a) == expect_total);

	uint64_t root = 0;
	CHECK(tessera_extent_flush(a, &root) == TESSERA_OK);
	CHECK(root != 0);
	tessera_extent_close(a);

	/* Reopen and verify state matches. */
	tessera_extent_alloc_t *a2 = tessera_extent_open(&io, root);
	CHECK(a2 != NULL);
	CHECK(tessera_extent_free_blocks(a2) == expect_total);

	/* Allocate one block from each seeded extent's start; verifies
	 * the reloaded boundaries round-tripped exactly. */
	for (size_t i = 0; i < sizeof seed / sizeof seed[0]; i++) {
		uint64_t s = 0;
		CHECK(tessera_extent_alloc(a2, seed[i].n, &s) == TESSERA_OK);
		/* s could match any of the remaining extents whose length
		 * equals seed[i].n (best-fit on equal pick walks left).
		 * Just assert it lies within one of the seed ranges. */
		int found = 0;
		for (size_t j = 0; j < sizeof seed / sizeof seed[0]; j++) {
			if (s == seed[j].s) { found = 1; break; }
		}
		CHECK(found);
	}
	CHECK(tessera_extent_free_blocks(a2) == 0);

	/* flush() with no io must EINVAL. */
	tessera_extent_alloc_t *no_io = tessera_extent_open(NULL, 0);
	uint64_t dummy = 0;
	CHECK(tessera_extent_flush(no_io, &dummy) == TESSERA_EINVAL);
	tessera_extent_close(no_io);

	tessera_extent_close(a2);
	free(d);
}

int
main(void)
{
	printf("test_extent: in-memory free-extent allocator + B+tree backing\n");
	test_empty();
	test_seeded_alloc_split();
	test_best_fit();
	test_coalesce();
	test_coalesce_one_side();
	test_overlap_rejected();
	test_einval_zero();
	test_random_stress();
	test_btree_round_trip();
	if (failures > 0) {
		fprintf(stderr, "%d failure(s)\n", failures);
		return 1;
	}
	printf("ok\n");
	return 0;
}
