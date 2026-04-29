/*
 * Tests for the COW B+tree primitives.
 *
 * Backed by an in-memory "disk" (a slab + simple bump+free-list
 * allocator) so we can exercise read_block / write_block / alloc /
 * free without any actual file I/O. The B+tree code never sees the
 * difference between this and a real device.
 *
 * Cases:
 *   1. Empty tree → get returns ENOENT.
 *   2. Single put → single get returns same value.
 *   3. Sequential puts of N >> leaf_fanout — forces leaf splits and
 *      eventually internal-node growth. All entries retrievable.
 *   4. Random-order puts — same retrievability invariant.
 *   5. Update existing key — get returns new value, structure unchanged.
 *   6. Cursor walks all keys in ascending order.
 *   7. seek_at(K) lands on K (or first key > K).
 *   8. Delete one key — get returns ENOENT, others still findable.
 *   9. Delete every key — tree collapses to an empty leaf root,
 *      and a fresh put afterward works.
 */

#include "tessera/btree.h"
#include "tessera/error.h"

#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#define BLOCK_SIZE 4096

static int failures = 0;
#define CHECK(cond) do {                                                 \
	if (!(cond)) {                                                   \
		fprintf(stderr, "FAIL %s:%d: %s\n",                      \
		    __FILE__, __LINE__, #cond);                          \
		failures++;                                              \
	}                                                                \
} while (0)

/* ── memory-backed block I/O ─────────────────────────────────────── */

#define MAX_SECTORS 4096        /* 16 MiB of "disk" */

struct mem_disk {
	uint8_t  blocks[MAX_SECTORS][BLOCK_SIZE];
	uint8_t  used[MAX_SECTORS];
	uint64_t next_sector;       /* simple bump allocator */
	uint64_t in_flight;         /* allocated but not yet freed */
	uint64_t alloc_count;
	uint64_t free_count;
};

static int
mem_read(void *ctx, uint64_t s, uint8_t *out)
{
	struct mem_disk *d = ctx;
	if (s >= MAX_SECTORS) return -1;
	if (!d->used[s]) return -1;
	memcpy(out, d->blocks[s], BLOCK_SIZE);
	return 0;
}

static int
mem_write(void *ctx, uint64_t s, const uint8_t *buf)
{
	struct mem_disk *d = ctx;
	if (s >= MAX_SECTORS) return -1;
	memcpy(d->blocks[s], buf, BLOCK_SIZE);
	return 0;
}

static int
mem_alloc(void *ctx, uint64_t n, uint64_t *out)
{
	struct mem_disk *d = ctx;
	if (n != 1) return -1;
	for (uint64_t i = d->next_sector; i < MAX_SECTORS; i++) {
		if (!d->used[i]) {
			d->used[i] = 1;
			d->next_sector = i + 1;
			d->in_flight++;
			d->alloc_count++;
			*out = i;
			return 0;
		}
	}
	for (uint64_t i = 0; i < d->next_sector; i++) {
		if (!d->used[i]) {
			d->used[i] = 1;
			d->in_flight++;
			d->alloc_count++;
			*out = i;
			return 0;
		}
	}
	return -1;
}

static int
mem_free(void *ctx, uint64_t s, uint64_t n)
{
	struct mem_disk *d = ctx;
	if (n != 1 || s >= MAX_SECTORS || !d->used[s]) return -1;
	d->used[s] = 0;
	d->in_flight--;
	d->free_count++;
	if (s < d->next_sector) d->next_sector = s;
	return 0;
}

static struct mem_disk *
mk_disk(void)
{
	struct mem_disk *d = calloc(1, sizeof *d);
	d->next_sector = 1;             /* leave sector 0 unused */
	return d;
}

static tessera_block_io_t
mk_io(struct mem_disk *d)
{
	tessera_block_io_t io = {
		.read_block  = mem_read,
		.write_block = mem_write,
		.alloc       = mem_alloc,
		.free        = mem_free,
		.ctx         = d,
	};
	return io;
}

/* ── test helpers ────────────────────────────────────────────────── */

/* Encode an integer into a big-endian key so memcmp ordering ==
 * numeric ordering. */
static void
be32_key(uint32_t k, uint8_t out[4])
{
	out[0] = (k >> 24) & 0xff;
	out[1] = (k >> 16) & 0xff;
	out[2] = (k >>  8) & 0xff;
	out[3] = (k      ) & 0xff;
}

/* ── tests ───────────────────────────────────────────────────────── */

static void
test_empty_get(void)
{
	struct mem_disk *d = mk_disk();
	tessera_block_io_t io = mk_io(d);
	uint64_t root;
	tessera_btree_t *t = tessera_btree_create(&io, /*tree_kind*/ 0,
	    /*key*/ 4, /*value*/ 8, &root);
	CHECK(t != NULL);

	uint8_t k[4]; be32_key(42, k);
	uint8_t v[8] = {0};
	CHECK(tessera_btree_get(t, k, v) == TESSERA_ENOENT);

	tessera_btree_close(t);
	free(d);
}

static void
test_one_put_get(void)
{
	struct mem_disk *d = mk_disk();
	tessera_block_io_t io = mk_io(d);
	uint64_t root;
	tessera_btree_t *t = tessera_btree_create(&io, 0, 4, 8, &root);

	uint8_t k[4]; be32_key(123, k);
	uint8_t in[8]  = "abcdefgh";
	uint8_t out[8] = {0};
	CHECK(tessera_btree_put(t, k, in, &root) == TESSERA_OK);
	CHECK(tessera_btree_get(t, k, out) == TESSERA_OK);
	CHECK(memcmp(in, out, 8) == 0);

	/* Missing key still ENOENT. */
	be32_key(999, k);
	CHECK(tessera_btree_get(t, k, out) == TESSERA_ENOENT);

	tessera_btree_close(t);
	free(d);
}

static void
test_many_sequential(void)
{
	struct mem_disk *d = mk_disk();
	tessera_block_io_t io = mk_io(d);
	uint64_t root;
	tessera_btree_t *t = tessera_btree_create(&io, 0, 4, 8, &root);

	const uint32_t N = 2000;
	for (uint32_t i = 0; i < N; i++) {
		uint8_t k[4]; be32_key(i, k);
		uint8_t v[8];
		uint64_t vv = (uint64_t)i * 31u + 7u;
		memcpy(v, &vv, 8);
		CHECK(tessera_btree_put(t, k, v, &root) == TESSERA_OK);
	}
	for (uint32_t i = 0; i < N; i++) {
		uint8_t k[4]; be32_key(i, k);
		uint8_t v[8] = {0};
		CHECK(tessera_btree_get(t, k, v) == TESSERA_OK);
		uint64_t want = (uint64_t)i * 31u + 7u;
		uint64_t got;
		memcpy(&got, v, 8);
		if (got != want) {
			fprintf(stderr, "FAIL i=%u got=%llu want=%llu\n",
			    i, (unsigned long long)got,
			    (unsigned long long)want);
			failures++;
			break;
		}
	}
	printf("  sequential N=%u : disk in_flight=%llu (alloc=%llu free=%llu)\n",
	    N,
	    (unsigned long long)d->in_flight,
	    (unsigned long long)d->alloc_count,
	    (unsigned long long)d->free_count);

	tessera_btree_close(t);
	free(d);
}

static void
test_random_order(void)
{
	struct mem_disk *d = mk_disk();
	tessera_block_io_t io = mk_io(d);
	uint64_t root;
	tessera_btree_t *t = tessera_btree_create(&io, 0, 4, 8, &root);

	const uint32_t N = 1500;
	uint32_t *keys = malloc(N * sizeof *keys);
	for (uint32_t i = 0; i < N; i++) keys[i] = i;
	/* xorshift shuffle */
	uint64_t s = 0xfeedface;
	for (uint32_t i = N - 1; i > 0; i--) {
		s ^= s << 13; s ^= s >> 7; s ^= s << 17;
		uint32_t j = (uint32_t)(s % (i + 1));
		uint32_t tmp = keys[i]; keys[i] = keys[j]; keys[j] = tmp;
	}
	for (uint32_t i = 0; i < N; i++) {
		uint8_t k[4]; be32_key(keys[i], k);
		uint64_t v = keys[i];
		CHECK(tessera_btree_put(t, k, &v, &root) == TESSERA_OK);
	}
	for (uint32_t i = 0; i < N; i++) {
		uint8_t k[4]; be32_key(i, k);
		uint64_t v = 0;
		CHECK(tessera_btree_get(t, k, &v) == TESSERA_OK);
		CHECK(v == i);
	}
	free(keys);
	tessera_btree_close(t);
	free(d);
}

static void
test_update_in_place(void)
{
	struct mem_disk *d = mk_disk();
	tessera_block_io_t io = mk_io(d);
	uint64_t root;
	tessera_btree_t *t = tessera_btree_create(&io, 0, 4, 8, &root);

	uint8_t k[4]; be32_key(7, k);
	uint64_t v1 = 100, v2 = 200, vget;
	CHECK(tessera_btree_put(t, k, &v1, &root) == TESSERA_OK);
	CHECK(tessera_btree_put(t, k, &v2, &root) == TESSERA_OK);
	CHECK(tessera_btree_get(t, k, &vget) == TESSERA_OK);
	CHECK(vget == 200);
	tessera_btree_close(t);
	free(d);
}

static void
test_cursor_walk(void)
{
	struct mem_disk *d = mk_disk();
	tessera_block_io_t io = mk_io(d);
	uint64_t root;
	tessera_btree_t *t = tessera_btree_create(&io, 0, 4, 8, &root);

	const uint32_t N = 800;
	for (uint32_t i = 0; i < N; i++) {
		uint8_t k[4]; be32_key(i * 3, k);
		uint64_t v = i;
		CHECK(tessera_btree_put(t, k, &v, &root) == TESSERA_OK);
	}

	tessera_btree_cursor_t *c = tessera_btree_seek_first(t);
	CHECK(c != NULL);
	uint32_t expect = 0;
	int got_count = 0;
	for (;;) {
		uint8_t kbuf[4]; uint64_t vbuf;
		if (tessera_btree_cursor_get(c, kbuf, &vbuf) != TESSERA_OK)
			break;
		uint32_t key = ((uint32_t)kbuf[0] << 24) |
		               ((uint32_t)kbuf[1] << 16) |
		               ((uint32_t)kbuf[2] <<  8) |
		               ((uint32_t)kbuf[3]);
		CHECK(key == expect * 3);
		CHECK(vbuf == expect);
		expect++; got_count++;
		if (tessera_btree_cursor_next(c) != TESSERA_OK) break;
	}
	CHECK(got_count == (int)N);
	tessera_btree_cursor_free(c);

	/* seek_at(arbitrary key) should land on next-or-equal. */
	uint8_t k[4]; be32_key(50 * 3, k);
	c = tessera_btree_seek_at(t, k);
	CHECK(c != NULL);
	uint8_t kbuf[4]; uint64_t vbuf;
	CHECK(tessera_btree_cursor_get(c, kbuf, &vbuf) == TESSERA_OK);
	CHECK(vbuf == 50);
	tessera_btree_cursor_free(c);

	tessera_btree_close(t);
	free(d);
}

static void
test_delete_then_collapse(void)
{
	struct mem_disk *d = mk_disk();
	tessera_block_io_t io = mk_io(d);
	uint64_t root;
	tessera_btree_t *t = tessera_btree_create(&io, 0, 4, 8, &root);

	const uint32_t N = 500;
	for (uint32_t i = 0; i < N; i++) {
		uint8_t k[4]; be32_key(i, k);
		uint64_t v = i;
		CHECK(tessera_btree_put(t, k, &v, &root) == TESSERA_OK);
	}
	/* Delete every odd key. */
	for (uint32_t i = 1; i < N; i += 2) {
		uint8_t k[4]; be32_key(i, k);
		CHECK(tessera_btree_delete(t, k, &root) == TESSERA_OK);
	}
	for (uint32_t i = 0; i < N; i++) {
		uint8_t k[4]; be32_key(i, k);
		uint64_t v = 0;
		int r = tessera_btree_get(t, k, &v);
		if (i & 1) {
			CHECK(r == TESSERA_ENOENT);
		} else {
			CHECK(r == TESSERA_OK);
			CHECK(v == i);
		}
	}
	/* Delete all remaining. */
	for (uint32_t i = 0; i < N; i += 2) {
		uint8_t k[4]; be32_key(i, k);
		CHECK(tessera_btree_delete(t, k, &root) == TESSERA_OK);
	}
	/* Tree empty — fresh put still works. */
	uint8_t k[4]; be32_key(99, k);
	uint64_t v = 9999, vget = 0;
	CHECK(tessera_btree_put(t, k, &v, &root) == TESSERA_OK);
	CHECK(tessera_btree_get(t, k, &vget) == TESSERA_OK);
	CHECK(vget == 9999);

	tessera_btree_close(t);
	free(d);
}

int
main(void)
{
	printf("test_btree: COW B+tree get/put/delete + cursor\n");
	test_empty_get();
	test_one_put_get();
	test_many_sequential();
	test_random_order();
	test_update_in_place();
	test_cursor_walk();
	test_delete_then_collapse();
	if (failures > 0) {
		fprintf(stderr, "%d failure(s)\n", failures);
		return 1;
	}
	printf("ok\n");
	return 0;
}
