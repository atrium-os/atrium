/*
 * Tests for the volume layer (mkfs + open).
 *
 *   1. format → open → fields match the requested layout.
 *   2. open without prior format → ECORRUPT.
 *   3. dual-superblock recovery: corrupt SB-A, SB-B picks up.
 *   4. dual-superblock recovery: corrupt SB-B, SB-A picks up.
 *   5. dual corruption → ECORRUPT.
 *   6. extent root reachable: open the free-extent tree at the root
 *      published in the SB and verify it advertises the expected
 *      pack-zone size.
 *   7. inode + pack-registry roots reachable: open with the published
 *      key/value sizes; cursor walks to empty.
 */

#include "tessera/volume.h"
#include "tessera/btree.h"
#include "tessera/codec.h"
#include "tessera/error.h"
#include "tessera/extent.h"
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

/* ── memory-backed disk ──────────────────────────────────────────── */

#define MAX_SECTORS 4096

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
	memcpy(d->blocks[s], b, 4096); d->used[s] = 1; return 0;
}
static int md_alloc(void *ctx, uint64_t n, uint64_t *o) {
	struct mem_disk *d = ctx;
	if (n != 1) return -1;
	for (uint64_t i = d->next_sector; i < MAX_SECTORS; i++) {
		if (!d->used[i]) { d->used[i] = 1; d->next_sector = i + 1;
		    *o = i; return 0; }
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

static tessera_block_io_t
mk_io(struct mem_disk *d)
{
	tessera_block_io_t io = {
		.read_block  = md_read,  .write_block = md_write,
		.alloc       = md_alloc, .free        = md_free,
		.ctx         = d,
	};
	return io;
}

/* ── tests ───────────────────────────────────────────────────────── */

static void
test_format_open_round_trip(void)
{
	struct mem_disk *d = calloc(1, sizeof *d);
	d->next_sector = 1;
	tessera_block_io_t io = mk_io(d);

	tessera_format_opts_t opts;
	memset(&opts, 0, sizeof opts);
	opts.total_sectors = 2048;
	opts.journal_sectors = 64;
	for (int i = 0; i < 16; i++) opts.volume_uuid[i] = (uint8_t)(0x10 + i);

	CHECK(tessera_volume_format(&io, &opts) == TESSERA_OK);

	tessera_volume_t *v = NULL;
	CHECK(tessera_volume_open(&io, &v) == TESSERA_OK);
	CHECK(v != NULL);
	CHECK(tessera_volume_total_sectors(v) == 2048);
	CHECK(tessera_volume_generation(v)    == 1);
	CHECK(tessera_volume_journal_start(v) == 4);
	CHECK(tessera_volume_journal_length(v) == 64);

	const uint8_t *u = tessera_volume_uuid(v);
	for (int i = 0; i < 16; i++) CHECK(u[i] == (uint8_t)(0x10 + i));

	const uint64_t expect_pack_start = 4 + 64 + TESSERA_METADATA_ZONE_SECTORS;
	CHECK(tessera_volume_inode_root(v) >= 4 + 64);
	CHECK(tessera_volume_pack_registry_root(v) >= 4 + 64);
	CHECK(tessera_volume_free_extent_root(v) >= 4 + 64);
	CHECK(tessera_volume_inode_root(v) < expect_pack_start);
	CHECK(tessera_volume_pack_registry_root(v) < expect_pack_start);
	CHECK(tessera_volume_free_extent_root(v) < expect_pack_start);

	tessera_volume_close(v);
	free(d);
}

static void
test_open_without_format(void)
{
	struct mem_disk *d = calloc(1, sizeof *d);
	tessera_block_io_t io = mk_io(d);
	tessera_volume_t *v = NULL;
	CHECK(tessera_volume_open(&io, &v) == TESSERA_ECORRUPT);
	CHECK(v == NULL);
	free(d);
}

static void
test_sb_a_corruption(void)
{
	struct mem_disk *d = calloc(1, sizeof *d);
	d->next_sector = 1;
	tessera_block_io_t io = mk_io(d);
	tessera_format_opts_t opts;
	memset(&opts, 0, sizeof opts);
	opts.total_sectors = 1024;
	opts.journal_sectors = 32;
	CHECK(tessera_volume_format(&io, &opts) == TESSERA_OK);

	/* Damage SB-A; SB-B should cover. */
	d->blocks[0][20] ^= 0xff;

	tessera_volume_t *v = NULL;
	CHECK(tessera_volume_open(&io, &v) == TESSERA_OK);
	CHECK(tessera_volume_total_sectors(v) == 1024);
	tessera_volume_close(v);
	free(d);
}

static void
test_sb_b_corruption(void)
{
	struct mem_disk *d = calloc(1, sizeof *d);
	d->next_sector = 1;
	tessera_block_io_t io = mk_io(d);
	tessera_format_opts_t opts;
	memset(&opts, 0, sizeof opts);
	opts.total_sectors = 1024;
	opts.journal_sectors = 32;
	tessera_volume_format(&io, &opts);

	d->blocks[1][30] ^= 0x01;
	tessera_volume_t *v = NULL;
	CHECK(tessera_volume_open(&io, &v) == TESSERA_OK);
	tessera_volume_close(v);
	free(d);
}

static void
test_dual_corruption(void)
{
	struct mem_disk *d = calloc(1, sizeof *d);
	d->next_sector = 1;
	tessera_block_io_t io = mk_io(d);
	tessera_format_opts_t opts;
	memset(&opts, 0, sizeof opts);
	opts.total_sectors = 1024;
	opts.journal_sectors = 32;
	tessera_volume_format(&io, &opts);

	d->blocks[0][20] ^= 0xff;
	d->blocks[1][30] ^= 0x01;
	tessera_volume_t *v = NULL;
	CHECK(tessera_volume_open(&io, &v) == TESSERA_ECORRUPT);
	free(d);
}

static void
test_free_extent_tree_reachable(void)
{
	struct mem_disk *d = calloc(1, sizeof *d);
	d->next_sector = 1;
	tessera_block_io_t io = mk_io(d);
	tessera_format_opts_t opts;
	memset(&opts, 0, sizeof opts);
	opts.total_sectors = 4000;
	opts.journal_sectors = 64;
	tessera_volume_format(&io, &opts);

	tessera_volume_t *v = NULL;
	tessera_volume_open(&io, &v);

	/* Open the published free-extent tree and add up the lengths. */
	tessera_extent_alloc_t *ea = tessera_extent_open(&io,
	    tessera_volume_free_extent_root(v));
	CHECK(ea != NULL);

	const uint64_t metadata_end = 4 + 64 + TESSERA_METADATA_ZONE_SECTORS;
	const uint64_t expect_free  = 4000 - metadata_end;
	CHECK(tessera_extent_free_blocks(ea) == expect_free);
	CHECK(tessera_extent_largest_free_run(ea) == expect_free);

	tessera_extent_close(ea);
	tessera_volume_close(v);
	free(d);
}

static void
test_inode_and_pack_roots_reachable(void)
{
	struct mem_disk *d = calloc(1, sizeof *d);
	d->next_sector = 1;
	tessera_block_io_t io = mk_io(d);
	tessera_format_opts_t opts;
	memset(&opts, 0, sizeof opts);
	opts.total_sectors = 1024;
	opts.journal_sectors = 32;
	tessera_volume_format(&io, &opts);

	tessera_volume_t *v = NULL;
	tessera_volume_open(&io, &v);

	/* mkfs seeds inode 2 (root dir); the inode tree is non-empty. */
	tessera_btree_t *inode_tree = tessera_btree_open(&io,
	    tessera_volume_inode_root(v),
	    /*kind*/ 0, /*key*/ 4, /*value*/ TESSERA_INODE_RECORD_SIZE);
	CHECK(inode_tree != NULL);
	tessera_btree_cursor_t *c = tessera_btree_seek_first(inode_tree);
	CHECK(c != NULL);
	uint8_t k[4]; uint8_t val[TESSERA_INODE_RECORD_SIZE];
	CHECK(tessera_btree_cursor_get(c, k, val) == TESSERA_OK);
	/* First entry is inode 2 (TESSERA_INODE_ROOT_DIR), big-endian key. */
	CHECK(k[0] == 0 && k[1] == 0 && k[2] == 0 && k[3] == 2);
	CHECK(tessera_btree_cursor_next(c) == TESSERA_ENOENT);
	tessera_btree_cursor_free(c);
	tessera_btree_close(inode_tree);

	tessera_btree_t *pack_tree = tessera_btree_open(&io,
	    tessera_volume_pack_registry_root(v),
	    /*kind*/ 1, /*key*/ 16, /*value*/ TESSERA_REGISTRY_ENTRY_SIZE);
	CHECK(pack_tree != NULL);
	tessera_btree_cursor_t *c2 = tessera_btree_seek_first(pack_tree);
	CHECK(c2 != NULL);
	uint8_t pk[16]; uint8_t pv[TESSERA_REGISTRY_ENTRY_SIZE];
	CHECK(tessera_btree_cursor_get(c2, pk, pv) == TESSERA_ENOENT);
	tessera_btree_cursor_free(c2);
	tessera_btree_close(pack_tree);

	tessera_volume_close(v);
	free(d);
}

int
main(void)
{
	printf("test_volume: format + open + dual-SB recovery + tree reachability\n");
	test_format_open_round_trip();
	test_open_without_format();
	test_sb_a_corruption();
	test_sb_b_corruption();
	test_dual_corruption();
	test_free_extent_tree_reachable();
	test_inode_and_pack_roots_reachable();
	if (failures > 0) {
		fprintf(stderr, "%d failure(s)\n", failures);
		return 1;
	}
	printf("ok\n");
	return 0;
}
