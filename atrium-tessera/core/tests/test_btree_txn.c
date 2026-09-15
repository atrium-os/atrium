/* Host unit test: a B+tree mutation that FAILS must not free any node the
 * tree's surviving root still reaches.
 *
 * ★ The bug this pins down. COW frees the superseded node as soon as its
 * replacement is written, but the mutation can still fail further up — the
 * parent's allocation, a root split — and the caller then keeps the OLD root,
 * which references every node freed so far. The kmod's epoch sweep recycles
 * same-flush frees immediately when the metadata reserve is exhausted, so
 * those still-live nodes were handed to other trees: on the dev VM, "sector
 * N holds a inode node but was reached as snapshot", pinscan aborting every
 * pass, and a corrupt volume.
 *
 * The allocator here does what the sweep does: it hands out freed sectors
 * FIRST, immediately. And it can be told to fail after K more allocations.
 * For every K, every mutation kind is made to fail part-way; then a
 * successful mutation on a SECOND tree (which grabs whatever was freed)
 * overwrites any sector the failed op wrongly released. The first tree must
 * still read back exactly, and no freed sector may be one its root reaches. */
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include "tessera/btree.h"
#include "tessera/error.h"
#include "tessera/format.h"

#define NSEC   20000
static uint8_t (*disk)[4096];
static uint64_t bump = 1;
static uint64_t freelist[NSEC];
static uint32_t nfree;
static long     alloc_budget = -1;     /* <0 = unlimited */
static int      failures;
static int      scenarios_failed_op;   /* dead-arm guard: ops that actually failed */

static int rd(void *c, uint64_t s, uint8_t *b) { (void)c; if (s >= NSEC) return -1; memcpy(b, disk[s], 4096); return 0; }
static int wr(void *c, uint64_t s, const uint8_t *b) { (void)c; if (s >= NSEC) return -1; memcpy(disk[s], b, 4096); return 0; }
static int al(void *c, uint64_t n, uint64_t *out)
{
	(void)c;
	if (n != 1) return -1;
	if (alloc_budget == 0) return -1;
	if (alloc_budget > 0) alloc_budget--;
	if (nfree > 0) { *out = freelist[--nfree]; return 0; }   /* reuse first */
	if (bump >= NSEC) return -1;
	*out = bump++;
	return 0;
}
static int fr(void *c, uint64_t s, uint64_t n) { (void)c; (void)n; freelist[nfree++] = s; return 0; }

#define CHECK(cond, ...) do { if (!(cond)) { printf("  FAIL: " __VA_ARGS__); printf("\n"); failures++; } } while (0)

/* reachable set of a tree */
static uint8_t reach[NSEC];
static int mark(void *ctx, uint64_t s) { (void)ctx; if (s < NSEC) reach[s] = 1; return 0; }

static void mkkey(uint32_t v, uint8_t k[8]) { for (int i = 0; i < 8; i++) k[i] = (uint8_t)((uint64_t)v >> ((7 - i) * 8)); }
static void mkval(uint32_t v, uint32_t salt, uint8_t val[32]) { for (int i = 0; i < 32; i++) val[i] = (uint8_t)(v * 31u + salt + (uint32_t)i); }

static int verify(tessera_btree_t *t, uint32_t nkeys, uint32_t salt, const char *what)
{
	uint8_t k[8], v[32], want[32];
	for (uint32_t i = 0; i < nkeys; i++) {
		mkkey(i * 2, k); mkval(i * 2, salt, want);
		if (tessera_btree_get(t, k, v) != TESSERA_OK || memcmp(v, want, 32) != 0) {
			printf("  FAIL: %s: key %u unreadable or wrong after a failed mutation\n", what, i * 2);
			failures++;
			return 0;
		}
	}
	return 1;
}

/* One scenario: build tree A (nkeys even keys), fail `op` after K allocs,
 * then let tree B consume the free list, then verify A. */
static void scenario(const char *op, uint32_t nkeys, long K)
{
	memset(disk, 0, (size_t)NSEC * 4096); bump = 1; nfree = 0; alloc_budget = -1;
	tessera_block_io_t io = { .ctx = NULL, .read_block = rd, .write_block = wr, .alloc = al, .free = fr };
	uint64_t ra = 0, rb = 0;
	tessera_btree_t *A = tessera_btree_create(&io, 0, 8, 32, &ra);
	tessera_btree_t *B = tessera_btree_create(&io, 0, 8, 32, &rb);
	uint8_t k[8], v[32];
	for (uint32_t i = 0; i < nkeys; i++) {
		mkkey(i * 2, k); mkval(i * 2, 7, v);
		if (tessera_btree_put(A, k, v, &ra) != TESSERA_OK) { printf("  setup put failed\n"); failures++; return; }
	}
	nfree = 0;   /* setup frees are irrelevant; start the op with a dry free list */

	/* Run the mutation sequence one op at a time; only the op under test
	 * gets the allocation budget, and the root / reachable set / free
	 * list position are captured immediately before it. */
	uint32_t nops = strcmp(op, "batch") == 0 ? 1 : nkeys;
	int rc = TESSERA_OK;
	uint64_t out = 0, root_before = 0;
	uint32_t nfree_before = 0;
	for (uint32_t i = 0; i < nops; i++) {
		if (strcmp(op, "delete") == 0 && i % 3 != 0) continue;
		memset(reach, 0, sizeof reach);
		(void)tessera_btree_walk_nodes(A, mark, NULL);
		root_before = tessera_btree_root(A);
		nfree_before = nfree;
		alloc_budget = K;
		if (strcmp(op, "put-split") == 0) {
			mkkey(i * 2 + 1, k); mkval(i * 2 + 1, 7, v);
			rc = tessera_btree_put(A, k, v, &out);
		} else if (strcmp(op, "batch") == 0) {
			uint8_t *keys = calloc(nkeys, 8), *vals = calloc(nkeys, 32);
			for (uint32_t j = 0; j < nkeys; j++) { mkkey(j * 2 + 1, keys + j * 8); mkval(j * 2 + 1, 7, vals + j * 32); }
			rc = tessera_btree_put_sorted_batch(A, keys, vals, nkeys, &out);
			free(keys); free(vals);
		} else {
			mkkey(i * 2, k);
			rc = tessera_btree_delete(A, k, &out);
		}
		alloc_budget = -1;
		if (rc != TESSERA_OK) break;
	}
	if (rc == TESSERA_OK) {   /* never failed at this budget */
		tessera_btree_close(A); tessera_btree_close(B);
		return;
	}
	scenarios_failed_op++;

	/* (1) the failed op kept its root and freed nothing that root reaches. */
	CHECK(tessera_btree_root(A) == root_before, "%s K=%ld: root moved on failure", op, K);
	for (uint32_t i = nfree_before; i < nfree; i++)
		CHECK(!reach[freelist[i]], "%s K=%ld: freed sector %ju is still reachable from the surviving root",
		    op, K, (uintmax_t)freelist[i]);

	/* (2) end to end: another tree grabs and overwrites every freed sector,
	 * then A must still read back exactly (the deletes and odd keys never
	 * landed, so the original even keys are the expected content). */
	for (uint32_t i = 0; i < 400; i++) {
		mkkey(i, k); mkval(i, 99, v);
		(void)tessera_btree_put(B, k, v, &rb);
	}
	if (strcmp(op, "delete") != 0)
		{ char _w[48]; snprintf(_w, sizeof _w, "%s K=%ld", op, K); (void)verify(A, nkeys, 7, _w); }
	else {
		/* A failed delete sequence may have completed some deletes before
		 * the failing one; check the tree is walkable and every key it
		 * reports is intact. */
		uint8_t want[32];
		for (uint32_t i = 0; i < nkeys; i++) {
			mkkey(i * 2, k); mkval(i * 2, 7, want);
			int g = tessera_btree_get(A, k, v);
			CHECK(g == TESSERA_OK || g == TESSERA_ENOENT, "delete K=%ld: key %u read error %d", K, i * 2, g);
			if (g == TESSERA_OK)
				CHECK(memcmp(v, want, 32) == 0, "delete K=%ld: key %u corrupted", K, i * 2);
		}
	}
	tessera_btree_close(A); tessera_btree_close(B);
}

int main(void)
{
	disk = calloc(NSEC, 4096);
	if (disk == NULL) return 2;
	printf("test_btree_txn: failed mutations must not free live nodes\n");
	const char *ops[] = { "put-split", "batch", "delete" };
	int scenarios = 0;
	for (int o = 0; o < 3; o++) {
		for (long K = 0; K <= 40; K++) {
			int f0 = failures;
			scenario(ops[o], 2000, K);
			scenarios++;
			if (failures > f0 && failures - f0 > 20) break;
		}
	}
	printf("  %d scenarios, %d with a mid-op allocation failure, %d failure(s)\n",
	    scenarios, scenarios_failed_op, failures);
	if (scenarios_failed_op == 0) {
		printf("  FAIL: no mutation ever failed — the test exercised nothing\n");
		return 1;
	}
	free(disk);
	return failures ? 1 : 0;
}
