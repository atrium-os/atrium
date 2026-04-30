/*
 * Phase-0 smoke test — verifies the library headers compile, the
 * struct sizes match the spec (via static_asserts in format.h), and
 * that all stub functions return TESSERA_ENOTIMPL when called.
 *
 * Subsequent phases replace these placeholders with real algorithm
 * tests. The smoke test stays as a tripwire for regressions.
 */

#include "tessera/tessera.h"

#include <assert.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

static int failures = 0;

#define CHECK(cond) do {                                              \
	if (!(cond)) {                                                \
		fprintf(stderr, "FAIL %s:%d: %s\n",                  \
		    __FILE__, __LINE__, #cond);                       \
		failures++;                                           \
	}                                                             \
} while (0)

static void
test_struct_sizes(void)
{
	/* The static_asserts in format.h would have failed compilation
	 * if these were wrong; this is just runtime sanity checking. */
	CHECK(sizeof(tessera_superblock_t)        == 4096);
	CHECK(sizeof(tessera_journal_header_t)    == 4096);
	CHECK(sizeof(tessera_record_header_t)     == 64);
	CHECK(sizeof(tessera_pack_header_t)       == 4096);
	CHECK(sizeof(tessera_pack_index_entry_t)  == 48);
	CHECK(sizeof(tessera_blob_descriptor_t)   == 16);
	CHECK(sizeof(tessera_pack_footer_t)       == 4096);
	CHECK(sizeof(tessera_manifest_header_t)   == 32);
	CHECK(sizeof(tessera_chunk_record_t)      == 48);
	CHECK(sizeof(tessera_tree_record_t)       == 40);
	CHECK(sizeof(tessera_inode_record_t)      == 144);
	CHECK(sizeof(tessera_btree_node_header_t) == 64);
	CHECK(sizeof(tessera_registry_entry_t)    == 64);
	CHECK(sizeof(tessera_free_extent_t)       == 16);
}

static void
test_hash_basic(void)
{
	/* SHA-256 is implemented (not stubbed) since it dispatches to libmd. */
	tessera_hash_t h;
	tessera_sha256((const uint8_t *)"abc", 3, h);
	/* SHA-256("abc") = ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad */
	CHECK(h[0] == 0xba && h[1] == 0x78 && h[2] == 0x16);
	CHECK(h[29] == 0x00 && h[30] == 0x15 && h[31] == 0xad);

	tessera_hash_t z = {0};
	CHECK(tessera_hash_is_null(z));
	CHECK(!tessera_hash_is_null(h));
	CHECK(tessera_hash_equal(h, h));
	CHECK(!tessera_hash_equal(h, z));
}

static void
test_phase1_primitives(void)
{
	/* Phase 1: codec round-trip on superblock (the broadest CRC-bearing
	 * struct). Detailed coverage lives in test_codec/test_crc/test_cdc;
	 * this is just a tripwire to ensure the in-VM build of the same
	 * primitives behaves identically. */
	uint8_t buf[4096];
	tessera_superblock_t sb, sb2;
	memset(&sb, 0, sizeof(sb));
	memset(&sb2, 0, sizeof(sb2));
	memcpy(sb.magic, TESSERA_MAGIC_SUPERBLOCK, 8);
	sb.version_major = 1;
	sb.sector_size = TESSERA_SECTOR_SIZE;
	sb.total_sectors = 1024;

	CHECK(tessera_encode_superblock(&sb, buf) == TESSERA_OK);
	CHECK(tessera_decode_superblock(buf, &sb2) == TESSERA_OK);
	CHECK(sb2.version_major == 1);
	CHECK(sb2.total_sectors == 1024);

	/* Corruption is detected. */
	buf[20] ^= 0x10;
	CHECK(tessera_decode_superblock(buf, &sb2) == TESSERA_ECORRUPT);

	/* CDC: 1 MiB pseudo-random buffer splits into multiple chunks. */
	uint8_t *big = malloc(1u << 20);
	uint64_t s = 1;
	for (size_t i = 0; i < (1u << 20); i++) {
		s ^= s << 13; s ^= s >> 7; s ^= s << 17;
		big[i] = (uint8_t)s;
	}
	size_t bounds[64];
	size_t n = 0;
	CHECK(tessera_cdc_split(big, 1u << 20, &tessera_cdc_default_params,
	                         bounds, 64, &n) == TESSERA_OK);
	CHECK(n >= 2);
	CHECK(bounds[n - 1] == (1u << 20));
	free(big);
}

static void
test_strerror_nonnull(void)
{
	for (int e = 0; e >= -25; e--) {
		const char *s = tessera_strerror((tessera_errno_t)e);
		CHECK(s != NULL);
	}
}

int
main(void)
{
	printf("tessera-core phase-0 smoke test\n");

	test_struct_sizes();
	test_hash_basic();
	test_phase1_primitives();
	test_strerror_nonnull();

	if (failures > 0) {
		fprintf(stderr, "%d failure(s)\n", failures);
		return 1;
	}
	printf("ok — all checks passed\n");
	return 0;
}
