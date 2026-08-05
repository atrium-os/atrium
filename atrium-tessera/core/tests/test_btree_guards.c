/*
 * Tests for load_node's on-disk sanity guards.
 *
 * These cover the checks a reader applies to a node header BEFORE trusting it
 * enough to index into the block. Each one exists because of a real failure:
 *
 *   entry_count bound   An inode leaf with entry_count=4000000 made every
 *                       reader walk off the end of the 4 KiB block.
 *                       tessera-fsck died with SIGSEGV (exit 139); in the
 *                       kernel the same overread is a panic. Neither magic
 *                       nor CRC catches it — the CRC covers the header, so a
 *                       header claiming four million entries is perfectly
 *                       self-consistent. The insert path had always bounded
 *                       this; the read path never did.
 *   geometry match      A node whose key/value sizes differ from the tree's
 *                       is indexed with the wrong stride, reading real bytes
 *                       at meaningless offsets.
 *   tree_kind match     #115: a root sector recycled into another tree. The
 *                       quota root pointed at a blob-index node for weeks.
 *
 * The corruption is applied the way it actually occurs: the header is decoded,
 * a field is changed, and the header is RE-ENCODED so its CRC is valid again.
 * Flipping raw bytes instead would be rejected as a bad CRC before the check
 * under test ever ran — the test would pass while proving nothing.
 */

#include "tessera/btree.h"
#include "tessera/codec.h"
#include "tessera/error.h"
#include "tessera/format.h"

#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#define BLOCK_SIZE  4096
#define HEADER_SIZE 64u
#define KEY_SIZE    4u
#define VAL_SIZE    16u
#define LEAF_FANOUT ((BLOCK_SIZE - HEADER_SIZE) / (KEY_SIZE + VAL_SIZE))

static int failures = 0;
#define CHECK(cond) do {                                                 \
	if (!(cond)) {                                                   \
		fprintf(stderr, "FAIL %s:%d: %s\n",                      \
		    __FILE__, __LINE__, #cond);                          \
		failures++;                                              \
	}                                                                \
} while (0)

/* ── memory-backed block I/O (same shape as test_btree.c) ────────── */

#define MAX_SECTORS 512

struct mem_disk {
	uint8_t  blocks[MAX_SECTORS][BLOCK_SIZE];
	uint8_t  used[MAX_SECTORS];
	uint64_t next_sector;
};

static int
mem_read(void *ctx, uint64_t s, uint8_t *out)
{
	struct mem_disk *d = ctx;
	if (s >= MAX_SECTORS || !d->used[s]) return -1;
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
		if (!d->used[i]) { d->used[i] = 1; d->next_sector = i + 1;
		                   *out = i; return 0; }
	}
	return -1;
}

static int
mem_free(void *ctx, uint64_t s, uint64_t n)
{
	struct mem_disk *d = ctx;
	if (n != 1 || s >= MAX_SECTORS || !d->used[s]) return -1;
	d->used[s] = 0;
	if (s < d->next_sector) d->next_sector = s;
	return 0;
}

static struct mem_disk *disk;
static tessera_block_io_t io;

static void
io_init(void)
{
	free(disk);
	disk = calloc(1, sizeof *disk);
	disk->next_sector = 1;
	io.ctx = disk;
	io.read_block = mem_read;
	io.write_block = mem_write;
	io.alloc = mem_alloc;
	io.free = mem_free;
}

/* Build a small tree and return its root sector. */
static uint64_t
build_tree(uint8_t tree_kind, uint32_t nkeys)
{
	/* create(), not open(at 0): create allocates and writes the initial
	 * empty root node. Opening a tree at sector 0 leaves it with no root
	 * and the first put fails. */
	uint64_t root = 0;
	tessera_btree_t *t = tessera_btree_create(&io, tree_kind,
	    KEY_SIZE, VAL_SIZE, &root);
	if (t == NULL) { fprintf(stderr, "FAIL: btree_create\n"); failures++; return 0; }
	uint8_t val[VAL_SIZE];
	for (uint32_t i = 1; i <= nkeys; i++) {
		uint8_t key[KEY_SIZE];
		key[0] = (uint8_t)(i >> 24); key[1] = (uint8_t)(i >> 16);
		key[2] = (uint8_t)(i >> 8);  key[3] = (uint8_t)i;
		memset(val, (int)i, sizeof val);
		if (tessera_btree_put(t, key, val, &root) != TESSERA_OK) {
			fprintf(stderr, "FAIL: put %u\n", i); failures++; break;
		}
	}
	tessera_btree_close(t);
	return root;
}

/* Decode the node at `sector`, mutate its header, re-encode (fixing the CRC),
 * write it back. Returns 0 on success. */
typedef void (*hdr_mut_fn)(tessera_btree_node_header_t *);

static int
mutate_header(uint64_t sector, hdr_mut_fn fn)
{
	uint8_t blk[BLOCK_SIZE];
	if (mem_read(disk, sector, blk) != 0) return -1;
	tessera_btree_node_header_t h;
	if (tessera_decode_btree_node_header(blk, &h) != TESSERA_OK) return -1;
	fn(&h);
	if (tessera_encode_btree_node_header(&h, blk) != TESSERA_OK) return -1;
	return mem_write(disk, sector, blk);
}

/* A get() that must NOT crash and must NOT succeed. */
static int
get_key1(uint64_t root, uint8_t tree_kind)
{
	tessera_btree_t *t = tessera_btree_open(&io, root, tree_kind,
	    KEY_SIZE, VAL_SIZE);
	if (t == NULL) return TESSERA_ECORRUPT;
	uint8_t key[KEY_SIZE] = { 0, 0, 0, 1 };
	uint8_t out[VAL_SIZE];
	int rc = tessera_btree_get(t, key, out);
	tessera_btree_close(t);
	return rc;
}

/* ── the mutations ───────────────────────────────────────────────── */

static void mut_count_huge(tessera_btree_node_header_t *h) { h->entry_count = 4000000u; }
static void mut_count_over(tessera_btree_node_header_t *h) { h->entry_count = LEAF_FANOUT + 1u; }
static void mut_count_exact(tessera_btree_node_header_t *h) { h->entry_count = LEAF_FANOUT; }
static void mut_keysize(tessera_btree_node_header_t *h)    { h->key_size = KEY_SIZE + 4u; }
static void mut_valsize(tessera_btree_node_header_t *h)    { h->value_size = VAL_SIZE * 2u; }
static void mut_zero_geom(tessera_btree_node_header_t *h)  { h->key_size = 0; h->value_size = 0; }
static void mut_kind(tessera_btree_node_header_t *h)       { h->tree_kind = TESSERA_BTREE_KIND_BLOB_INDEX; }

static void
test_entry_count_bound(void)
{
	/* The exact failure that segfaulted tessera-fsck. */
	io_init();
	uint64_t root = build_tree(TESSERA_BTREE_KIND_INODE, 8);
	CHECK(get_key1(root, TESSERA_BTREE_KIND_INODE) == TESSERA_OK);
	CHECK(mutate_header(root, mut_count_huge) == 0);
	CHECK(get_key1(root, TESSERA_BTREE_KIND_INODE) == TESSERA_ECORRUPT);

	/* One past the fanout must be refused too — the interesting boundary
	 * is not "absurdly large", it is "one more than fits". */
	io_init();
	root = build_tree(TESSERA_BTREE_KIND_INODE, 8);
	CHECK(mutate_header(root, mut_count_over) == 0);
	CHECK(get_key1(root, TESSERA_BTREE_KIND_INODE) == TESSERA_ECORRUPT);
}

static void
test_entry_count_exact_fanout_ok(void)
{
	/*
	 * A FULL node must still load. Guards that are off by one turn a
	 * bounds check into a corruption report on healthy volumes, which is
	 * worse than the hole they close.
	 *
	 * The lookup result is deliberately not asserted: the header now
	 * claims more entries than were written, so the search reads zeroed
	 * slots and may or may not find key 1. What matters is that the node
	 * was ACCEPTED — i.e. the answer is not ECORRUPT.
	 */
	io_init();
	uint64_t root = build_tree(TESSERA_BTREE_KIND_INODE, 8);
	CHECK(mutate_header(root, mut_count_exact) == 0);
	CHECK(get_key1(root, TESSERA_BTREE_KIND_INODE) != TESSERA_ECORRUPT);
}

static void
test_geometry(void)
{
	io_init();
	uint64_t root = build_tree(TESSERA_BTREE_KIND_INODE, 4);
	CHECK(mutate_header(root, mut_keysize) == 0);
	CHECK(get_key1(root, TESSERA_BTREE_KIND_INODE) == TESSERA_ECORRUPT);

	io_init();
	root = build_tree(TESSERA_BTREE_KIND_INODE, 4);
	CHECK(mutate_header(root, mut_valsize) == 0);
	CHECK(get_key1(root, TESSERA_BTREE_KIND_INODE) == TESSERA_ECORRUPT);

	/* Zero means "written before these fields existed" and must still
	 * load — the guard is gated on nonzero for exactly this reason. */
	io_init();
	root = build_tree(TESSERA_BTREE_KIND_INODE, 4);
	CHECK(mutate_header(root, mut_zero_geom) == 0);
	CHECK(get_key1(root, TESSERA_BTREE_KIND_INODE) == TESSERA_OK);
}

static void
test_tree_kind(void)
{
	/* #115: the stale-root signature. A quota root pointing at a
	 * blob-index node produced exactly this. */
	io_init();
	uint64_t root = build_tree(TESSERA_BTREE_KIND_QUOTA, 4);
	CHECK(get_key1(root, TESSERA_BTREE_KIND_QUOTA) == TESSERA_OK);
	CHECK(mutate_header(root, mut_kind) == 0);
	CHECK(get_key1(root, TESSERA_BTREE_KIND_QUOTA) == TESSERA_ECORRUPT);
}

static void
test_healthy_tree_unaffected(void)
{
	/*
	 * The whole guard set must be invisible to a correct volume. This is
	 * the check that would have caught a too-strict geometry rule before
	 * it reached the dev root.
	 */
	io_init();
	uint64_t root = build_tree(TESSERA_BTREE_KIND_INODE, 200);  /* forces splits */
	tessera_btree_t *t = tessera_btree_open(&io, root,
	    TESSERA_BTREE_KIND_INODE, KEY_SIZE, VAL_SIZE);
	CHECK(t != NULL);
	int found = 0;
	for (uint32_t i = 1; i <= 200; i++) {
		uint8_t key[KEY_SIZE], out[VAL_SIZE];
		key[0] = (uint8_t)(i >> 24); key[1] = (uint8_t)(i >> 16);
		key[2] = (uint8_t)(i >> 8);  key[3] = (uint8_t)i;
		if (tessera_btree_get(t, key, out) == TESSERA_OK) found++;
	}
	tessera_btree_close(t);
	CHECK(found == 200);
}

int
main(void)
{
	test_entry_count_bound();
	test_entry_count_exact_fanout_ok();
	test_geometry();
	test_tree_kind();
	test_healthy_tree_unaffected();

	if (failures != 0) {
		fprintf(stderr, "%d failure(s)\n", failures);
		return 1;
	}
	printf("btree guards ok (fanout %u)\n", (unsigned)LEAF_FANOUT);
	return 0;
}
