/*
 * tessera-core: free-extent allocator.
 *
 * In-memory representation: a sorted dynamic array of (start_sector,
 * length_sectors), backed at rest by a tessera B+tree (tree_kind = 2,
 * key = 8 B start_sector, value = 8 B length_sectors). open() loads
 * the tree into the array; flush() COW-publishes the array as a fresh
 * tree and returns its new root sector.
 *
 * Invariants on the in-memory array:
 *   - extents are strictly increasing in start_sector,
 *   - no two adjacent extents are touching (extents[i].start +
 *     extents[i].length < extents[i+1].start).
 * Both invariants are enforced by tessera_extent_free()'s coalesce
 * pass; allocation only ever shrinks extents from one end, so it
 * cannot create touching pairs.
 *
 * Allocation policy: best-fit. Walks the array picking the smallest
 * extent ≥ requested length. Anti-fragmentation rotation (start the
 * scan at a rotating cursor) is reserved for v2 — best-fit alone is
 * sufficient for the workloads tessera targets (Git-style append-only
 * packs concentrate allocations into a sliding zone tail).
 *
 * Complexity: alloc O(N), free O(N) (due to insertion-sort + coalesce).
 * For v1 N = number of free extents, expected O(packs in flight) ≈
 * tens to low thousands. The on-disk B+tree drops free() to
 * O(log N); the in-memory side will follow once we wire COW updates
 * to the persistent tree.
 */

#include "tessera/extent.h"
#include "tessera/btree.h"
#include "tessera/error.h"
#include "tessera_compat.h"

#define EXTENT_TREE_KIND  2u
#define EXTENT_KEY_SIZE   8u    /* start_sector */
#define EXTENT_VALUE_SIZE 8u    /* length_sectors */

struct tessera_extent_alloc {
	tessera_free_extent_t *extents;
	size_t  count;
	size_t  capacity;
	uint64_t free_blocks;     /* sum of all extent lengths (cached) */
	tessera_block_io_t  io;
	int                 has_io;     /* 1 iff io was provided to open() */
};

static int
ensure_capacity(tessera_extent_alloc_t *a, size_t need)
{
	if (a->capacity >= need) return 0;
	size_t newcap = a->capacity ? a->capacity : 16;
	while (newcap < need) newcap *= 2;
	tessera_free_extent_t *p = tessera_realloc(a->extents,
	    newcap * sizeof *a->extents);
	if (p == NULL) return -1;
	a->extents = p;
	a->capacity = newcap;
	return 0;
}

/* Binary search: returns the index of the first extent whose start_sector
 * is >= `s`, or count if none. */
static size_t
lower_bound(const tessera_extent_alloc_t *a, uint64_t s)
{
	size_t lo = 0, hi = a->count;
	while (lo < hi) {
		size_t mid = lo + (hi - lo) / 2;
		if (a->extents[mid].start_sector < s) lo = mid + 1;
		else                                  hi = mid;
	}
	return lo;
}

/* ── public API ──────────────────────────────────────────────────── */

tessera_extent_alloc_t *
tessera_extent_open(const tessera_block_io_t *io, uint64_t free_root_sector)
{
	tessera_extent_alloc_t *a = tessera_zalloc(sizeof *a);
	if (a == NULL) return NULL;
	if (io != NULL) {
		a->io = *io;
		a->has_io = 1;
	}

	/* No backing tree yet — empty allocator, ready for mkfs-time
	 * seeding via tessera_extent_free(). */
	if (io == NULL || free_root_sector == 0) return a;

	/* Walk the existing free-extent B+tree and replay each entry
	 * through the in-memory free() path. The tree's stored entries
	 * are by construction non-overlapping and non-touching, so each
	 * free() takes the simple-insert branch — coalesce never fires. */
	tessera_btree_t *t = tessera_btree_open(io, free_root_sector,
	    EXTENT_TREE_KIND, EXTENT_KEY_SIZE, EXTENT_VALUE_SIZE);
	if (t == NULL) goto fail;

	tessera_btree_cursor_t *c = tessera_btree_seek_first(t);
	if (c != NULL) {
		uint64_t key, val;
		while (tessera_btree_cursor_get(c, &key, &val) == TESSERA_OK) {
			if (tessera_extent_free(a, key, val) != TESSERA_OK) {
				tessera_btree_cursor_free(c);
				tessera_btree_close(t);
				goto fail;
			}
			if (tessera_btree_cursor_next(c) != TESSERA_OK) break;
		}
		tessera_btree_cursor_free(c);
	}
	tessera_btree_close(t);
	return a;

fail:
	tessera_free(a->extents);
	tessera_free(a);
	return NULL;
}

int
tessera_extent_flush(tessera_extent_alloc_t *a, uint64_t *out_new_root)
{
	return tessera_extent_flush_via(a, NULL, out_new_root);
}

int
tessera_extent_flush_via(tessera_extent_alloc_t *a,
                         const tessera_block_io_t *alt_io,
                         uint64_t *out_new_root)
{
	if (a == NULL || out_new_root == NULL) return TESSERA_EINVAL;
	const tessera_block_io_t *use_io;
	if (alt_io != NULL) {
		use_io = alt_io;
	} else {
		if (!a->has_io) return TESSERA_EINVAL;
		use_io = &a->io;
	}

	uint64_t root = 0;
	tessera_btree_t *t = tessera_btree_create(use_io, EXTENT_TREE_KIND,
	    EXTENT_KEY_SIZE, EXTENT_VALUE_SIZE, &root);
	if (t == NULL) return TESSERA_ENOSPC;

	for (size_t i = 0; i < a->count; i++) {
		uint64_t k = a->extents[i].start_sector;
		uint64_t v = a->extents[i].length_sectors;
		int r = tessera_btree_put(t, &k, &v, &root);
		if (r != TESSERA_OK) {
			tessera_btree_close(t);
			return r;
		}
	}
	tessera_btree_close(t);
	*out_new_root = root;
	return TESSERA_OK;
}

void
tessera_extent_close(tessera_extent_alloc_t *a)
{
	if (a == NULL) return;
	tessera_free(a->extents);
	tessera_free(a);
}

uint64_t
tessera_extent_free_blocks(const tessera_extent_alloc_t *a)
{
	return a == NULL ? 0 : a->free_blocks;
}

uint64_t
tessera_extent_largest_free_run(const tessera_extent_alloc_t *a)
{
	if (a == NULL) return 0;
	uint64_t best = 0;
	for (size_t i = 0; i < a->count; i++)
		if (a->extents[i].length_sectors > best)
			best = a->extents[i].length_sectors;
	return best;
}

int
tessera_extent_alloc(tessera_extent_alloc_t *a, uint64_t n_sectors,
                     uint64_t *out_start)
{
	if (a == NULL || out_start == NULL) return TESSERA_EINVAL;
	if (n_sectors == 0) return TESSERA_EINVAL;

	/* Best-fit: smallest extent with length >= n_sectors. */
	size_t best_idx = (size_t)-1;
	uint64_t best_len = ~(uint64_t)0;
	for (size_t i = 0; i < a->count; i++) {
		uint64_t L = a->extents[i].length_sectors;
		if (L >= n_sectors && L < best_len) {
			best_len = L;
			best_idx = i;
			if (L == n_sectors) break;     /* perfect fit */
		}
	}
	if (best_idx == (size_t)-1) return TESSERA_ENOSPC;

	tessera_free_extent_t *e = &a->extents[best_idx];
	*out_start = e->start_sector;
	if (e->length_sectors == n_sectors) {
		/* Whole extent consumed — remove it. */
		memmove(e, e + 1,
		    (a->count - best_idx - 1) * sizeof *e);
		a->count--;
	} else {
		e->start_sector   += n_sectors;
		e->length_sectors -= n_sectors;
	}
	a->free_blocks -= n_sectors;
	return TESSERA_OK;
}

int
tessera_extent_free(tessera_extent_alloc_t *a, uint64_t start,
                    uint64_t n_sectors)
{
	if (a == NULL) return TESSERA_EINVAL;
	if (n_sectors == 0) return TESSERA_EINVAL;

	const uint64_t end = start + n_sectors;       /* exclusive */
	const size_t idx = lower_bound(a, start);     /* first extent >= start */

	/* Overlap detection: must not touch any *allocated* range — i.e.
	 * the freed range must lie strictly between the previous and next
	 * free extents (or coalesce). */
	if (idx > 0) {
		const tessera_free_extent_t *prev = &a->extents[idx - 1];
		uint64_t prev_end = prev->start_sector + prev->length_sectors;
		if (prev_end > start) return TESSERA_EINVAL; /* overlaps prev */
	}
	if (idx < a->count) {
		const tessera_free_extent_t *next = &a->extents[idx];
		if (end > next->start_sector) return TESSERA_EINVAL; /* overlaps next */
	}

	/* Merge with predecessor? */
	int merged_prev = 0;
	if (idx > 0) {
		tessera_free_extent_t *prev = &a->extents[idx - 1];
		if (prev->start_sector + prev->length_sectors == start) {
			prev->length_sectors += n_sectors;
			merged_prev = 1;
		}
	}
	/* Merge with successor? */
	int merged_next = 0;
	if (idx < a->count) {
		tessera_free_extent_t *next = &a->extents[idx];
		if (end == next->start_sector) {
			if (merged_prev) {
				/* Merge prev + next; remove next. */
				a->extents[idx - 1].length_sectors +=
				    next->length_sectors;
				memmove(next, next + 1,
				    (a->count - idx - 1) * sizeof *next);
				a->count--;
			} else {
				next->start_sector   = start;
				next->length_sectors += n_sectors;
			}
			merged_next = 1;
		}
	}

	if (!merged_prev && !merged_next) {
		/* Insert a new extent at idx. */
		if (ensure_capacity(a, a->count + 1) != 0)
			return TESSERA_ENOMEM;
		memmove(&a->extents[idx + 1], &a->extents[idx],
		    (a->count - idx) * sizeof *a->extents);
		a->extents[idx].start_sector   = start;
		a->extents[idx].length_sectors = n_sectors;
		a->count++;
	}
	a->free_blocks += n_sectors;
	return TESSERA_OK;
}
