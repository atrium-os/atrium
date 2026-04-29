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
	CHECK(sizeof(tessera_record_header_t)     == 32);
	CHECK(sizeof(tessera_pack_header_t)       == 4096);
	CHECK(sizeof(tessera_pack_index_entry_t)  == 48);
	CHECK(sizeof(tessera_blob_descriptor_t)   == 16);
	CHECK(sizeof(tessera_pack_footer_t)       == 4096);
	CHECK(sizeof(tessera_manifest_header_t)   == 32);
	CHECK(sizeof(tessera_chunk_record_t)      == 48);
	CHECK(sizeof(tessera_tree_record_t)       == 40);
	CHECK(sizeof(tessera_inode_record_t)      == 144);
	CHECK(sizeof(tessera_btree_node_header_t) == 32);
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
test_stubs_return_enotimpl(void)
{
	uint8_t buf[4096];
	tessera_superblock_t sb;
	memset(&sb, 0, sizeof(sb));
	CHECK(tessera_encode_superblock(&sb, buf) == TESSERA_ENOTIMPL);
	CHECK(tessera_decode_superblock(buf, &sb) == TESSERA_ENOTIMPL);

	size_t bounds[16];
	size_t n = 0;
	CHECK(tessera_cdc_split(buf, sizeof(buf), &tessera_cdc_default_params,
	                         bounds, 16, &n) == TESSERA_ENOTIMPL);
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
	test_stubs_return_enotimpl();
	test_strerror_nonnull();

	if (failures > 0) {
		fprintf(stderr, "%d failure(s)\n", failures);
		return 1;
	}
	printf("ok — all checks passed\n");
	return 0;
}
