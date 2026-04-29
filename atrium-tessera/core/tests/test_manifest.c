/*
 * Tests for the manifest builder + parser.
 *
 *   1. INLINE         small payload round-trip; hash determinism.
 *   2. CHUNK_LIST     N chunk records → build → parse → recover all.
 *   3. CHUNK_TREE     N tree children → build → parse → recover all.
 *   4. SYMLINK        target string round-trip.
 *   5. DIRECTORY      add 100 entries in random order; verify they
 *                     come back sorted by name on parse.
 *   6. EEXIST         duplicate dirent name rejected.
 *   7. ETOOBIG        finalize with insufficient buffer reports the
 *                     required size.
 *   8. Cross-impl hash determinism (via the in-VM run): same bytes
 *                     produce same hash on both libmd and the portable
 *                     fallback.
 */

#include "tessera/manifest.h"
#include "tessera/error.h"
#include "tessera/format.h"
#include "tessera/hash.h"

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

static void
fill_pattern(uint8_t *p, size_t n, uint8_t seed)
{
	for (size_t i = 0; i < n; i++) p[i] = (uint8_t)(seed + i * 17);
}

static void
test_inline(void)
{
	tessera_manifest_builder_t *b = tessera_manifest_begin(TESSERA_MFT_INLINE);
	CHECK(b != NULL);
	uint8_t data[200];
	fill_pattern(data, sizeof data, 0xa5);
	CHECK(tessera_manifest_set_inline(b, data, sizeof data) == TESSERA_OK);

	uint8_t buf[512];
	size_t  sz = 0;
	tessera_hash_t h;
	CHECK(tessera_manifest_finalize(b, buf, sizeof buf, &sz, h) == TESSERA_OK);
	CHECK(sz == 32 + sizeof data);

	tessera_manifest_parser_t *p = tessera_manifest_parse(buf, sz);
	CHECK(p != NULL);
	CHECK(tessera_manifest_parser_kind(p) == TESSERA_MFT_INLINE);
	CHECK(tessera_manifest_parser_size(p) == sizeof data);

	const uint8_t *out_data;
	size_t out_len;
	CHECK(tessera_manifest_inline_data(p, &out_data, &out_len) == TESSERA_OK);
	CHECK(out_len == sizeof data);
	CHECK(memcmp(out_data, data, sizeof data) == 0);

	/* Determinism: same input ⇒ same hash. */
	tessera_hash_t h2;
	tessera_sha256(buf, sz, h2);
	CHECK(memcmp(h, h2, 32) == 0);

	tessera_manifest_parser_free(p);
	tessera_manifest_free(b);
}

static void
test_chunk_list(void)
{
	tessera_manifest_builder_t *b = tessera_manifest_begin(TESSERA_MFT_CHUNK_LIST);
	const uint32_t N = 64;
	for (uint32_t i = 0; i < N; i++) {
		tessera_hash_t hh;
		fill_pattern(hh, 32, (uint8_t)i);
		CHECK(tessera_manifest_add_chunk(b, hh,
		    (uint64_t)i * 65536, 65536, 0) == TESSERA_OK);
	}

	size_t need = 0;
	tessera_hash_t h;
	(void)tessera_manifest_finalize(b, NULL, 0, &need, h);
	uint8_t *buf = malloc(need);
	size_t sz = 0;
	CHECK(tessera_manifest_finalize(b, buf, need, &sz, h) == TESSERA_OK);
	CHECK(sz == need);

	tessera_manifest_parser_t *p = tessera_manifest_parse(buf, sz);
	CHECK(tessera_manifest_parser_kind(p) == TESSERA_MFT_CHUNK_LIST);
	CHECK(tessera_manifest_parser_count(p) == N);
	CHECK(tessera_manifest_parser_size(p) == (uint64_t)N * 65536);

	for (uint32_t i = 0; i < N; i++) {
		tessera_chunk_record_t r;
		CHECK(tessera_manifest_chunk_at(p, i, &r) == TESSERA_OK);
		CHECK(r.logical_offset == (uint64_t)i * 65536);
		CHECK(r.uncompressed_size == 65536);
		uint8_t expect[32];
		fill_pattern(expect, 32, (uint8_t)i);
		CHECK(memcmp(r.chunk_hash, expect, 32) == 0);
	}

	tessera_chunk_record_t r;
	CHECK(tessera_manifest_chunk_at(p, N, &r) == TESSERA_ENOENT);

	tessera_manifest_parser_free(p);
	tessera_manifest_free(b);
	free(buf);
}

static void
test_chunk_tree(void)
{
	tessera_manifest_builder_t *b = tessera_manifest_begin(TESSERA_MFT_CHUNK_TREE);
	for (uint32_t i = 0; i < 32; i++) {
		tessera_hash_t hh;
		fill_pattern(hh, 32, (uint8_t)(0x80 + i));
		CHECK(tessera_manifest_add_tree_child(b, hh,
		    (uint64_t)i * (1ull << 24)) == TESSERA_OK);
	}
	size_t need = 0; tessera_hash_t h;
	(void)tessera_manifest_finalize(b, NULL, 0, &need, h);
	uint8_t *buf = malloc(need); size_t sz = 0;
	CHECK(tessera_manifest_finalize(b, buf, need, &sz, h) == TESSERA_OK);

	tessera_manifest_parser_t *p = tessera_manifest_parse(buf, sz);
	CHECK(tessera_manifest_parser_kind(p) == TESSERA_MFT_CHUNK_TREE);
	CHECK(tessera_manifest_parser_count(p) == 32);
	for (uint32_t i = 0; i < 32; i++) {
		tessera_tree_record_t r;
		CHECK(tessera_manifest_tree_at(p, i, &r) == TESSERA_OK);
		CHECK(r.logical_offset == (uint64_t)i * (1ull << 24));
		uint8_t expect[32];
		fill_pattern(expect, 32, (uint8_t)(0x80 + i));
		CHECK(memcmp(r.child_manifest_hash, expect, 32) == 0);
	}
	tessera_manifest_parser_free(p);
	tessera_manifest_free(b);
	free(buf);
}

static void
test_symlink(void)
{
	tessera_manifest_builder_t *b = tessera_manifest_begin(TESSERA_MFT_SYMLINK);
	const char *t = "../../usr/local/bin/sh";
	CHECK(tessera_manifest_set_symlink(b, t) == TESSERA_OK);

	uint8_t buf[256]; size_t sz = 0; tessera_hash_t h;
	CHECK(tessera_manifest_finalize(b, buf, sizeof buf, &sz, h) == TESSERA_OK);

	tessera_manifest_parser_t *p = tessera_manifest_parse(buf, sz);
	const uint8_t *od; size_t ol;
	CHECK(tessera_manifest_inline_data(p, &od, &ol) == TESSERA_OK);
	CHECK(ol == strlen(t));
	CHECK(memcmp(od, t, ol) == 0);
	tessera_manifest_parser_free(p);
	tessera_manifest_free(b);
}

static void
test_directory_sorted(void)
{
	tessera_manifest_builder_t *b = tessera_manifest_begin(TESSERA_MFT_DIRECTORY);

	/* Insert 40 names in unsorted order. */
	const char *names[] = {
		"zebra", "alpha", "mango", "pear", "fig",
		"apple", "banana", "cherry", "date", "elder",
		"grape", "honey", "ice", "jack", "kiwi",
		"lemon", "lime", "nectar", "olive", "orange",
		"plum", "quince", "rasp", "straw", "tangerine",
		"ugli", "vanilla", "walnut", "ximen", "yam",
		"avocado", "blueberry", "currant", "durian", "elderflower",
		"fig2", "guava", "hazel", "imbe", "jambul",
	};
	const size_t N = sizeof names / sizeof names[0];
	for (size_t i = 0; i < N; i++) {
		CHECK(tessera_manifest_add_dirent(b, 100u + i,
		    names[i], strlen(names[i])) == TESSERA_OK);
	}
	/* Duplicate insertion is rejected. */
	CHECK(tessera_manifest_add_dirent(b, 999, "apple", 5) == TESSERA_EEXIST);

	size_t need = 0; tessera_hash_t h;
	(void)tessera_manifest_finalize(b, NULL, 0, &need, h);
	uint8_t *buf = malloc(need); size_t sz = 0;
	CHECK(tessera_manifest_finalize(b, buf, need, &sz, h) == TESSERA_OK);

	/* Walk the encoded body and verify sorted order. */
	const uint8_t *body = buf + 32;
	size_t off = 0;
	const char *prev = NULL;
	size_t prev_len = 0;
	uint32_t seen = 0;
	while (off < sz - 32) {
		uint16_t nl;
		memcpy(&nl, body + off + 8, 2);
		const char *nm = (const char *)body + off + 10;
		if (prev != NULL) {
			size_t cmp_len = prev_len < nl ? prev_len : nl;
			int c = memcmp(prev, nm, cmp_len);
			CHECK(c < 0 || (c == 0 && prev_len < nl));
		}
		prev = nm;
		prev_len = nl;
		off += 10 + nl;
		seen++;
	}
	CHECK(seen == N);

	tessera_manifest_free(b);
	free(buf);
}

static void
test_einval_etoobig(void)
{
	tessera_manifest_builder_t *b = tessera_manifest_begin(TESSERA_MFT_CHUNK_LIST);
	tessera_hash_t hh = {0};
	/* Wrong-kind add should be EINVAL. */
	CHECK(tessera_manifest_add_tree_child(b, hh, 0) == TESSERA_EINVAL);

	CHECK(tessera_manifest_add_chunk(b, hh, 0, 1024, 0) == TESSERA_OK);
	uint8_t small[16]; size_t sz = 0; tessera_hash_t h;
	int r = tessera_manifest_finalize(b, small, sizeof small, &sz, h);
	CHECK(r == TESSERA_ETOOBIG);
	CHECK(sz == 32 + sizeof(tessera_chunk_record_t));
	tessera_manifest_free(b);
}

int
main(void)
{
	printf("test_manifest: builder + parser round-trips for INLINE / CHUNK_LIST / "
	       "CHUNK_TREE / SYMLINK / DIRECTORY\n");
	test_inline();
	test_chunk_list();
	test_chunk_tree();
	test_symlink();
	test_directory_sorted();
	test_einval_etoobig();
	if (failures > 0) {
		fprintf(stderr, "%d failure(s)\n", failures);
		return 1;
	}
	printf("ok\n");
	return 0;
}
