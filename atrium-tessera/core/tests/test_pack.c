/*
 * Tests for the pack-file builder + reader.
 *
 *   1. round-trip: write N blobs → open → lookup each → bytes match.
 *   2. bloom: bloom_might_contain reports 1 for present, mostly 0 for
 *      randomly-generated absent hashes (FPR < 5 %).
 *   3. lookup absent → ENOENT.
 *   4. duplicate blob in builder → EEXIST on finalize.
 *   5. corruption: flip a byte in the data area → open fails (footer
 *      CRC mismatch) OR the open succeeds but reader self-checks fail.
 *      We assert the open path catches it.
 */

#include "tessera/pack.h"
#include "tessera/codec.h"
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
gen_blob(uint32_t i, uint8_t **bytes, uint32_t *len, tessera_hash_t hash)
{
	uint32_t L = 100u + (i * 13u) % 4096u;
	uint8_t *b = malloc(L);
	uint64_t s = 0xc0ffee + i;
	for (uint32_t j = 0; j < L; j++) {
		s ^= s << 13; s ^= s >> 7; s ^= s << 17;
		b[j] = (uint8_t)s;
	}
	tessera_sha256(b, L, hash);
	*bytes = b; *len = L;
}

static void
test_round_trip(void)
{
	const uint8_t pack_id[16] = { 'P','A','C','K','0','0','0','R','T','E','S','T','1','2','3','4' };
	tessera_pack_builder_t *pb = tessera_pack_begin(1, pack_id, 42);
	CHECK(pb != NULL);

	const uint32_t N = 200;
	uint8_t **blob_bytes = calloc(N, sizeof *blob_bytes);
	uint32_t *blob_lens  = calloc(N, sizeof *blob_lens);
	tessera_hash_t *hashes = calloc(N, sizeof *hashes);

	for (uint32_t i = 0; i < N; i++) {
		gen_blob(i, &blob_bytes[i], &blob_lens[i], hashes[i]);
		CHECK(tessera_pack_add_blob(pb, hashes[i],
		    blob_bytes[i], blob_lens[i],
		    TESSERA_BLOB_FLAG_CHUNK) == TESSERA_OK);
	}

	size_t need = 0;
	(void)tessera_pack_finalize(pb, NULL, 0, &need);
	uint8_t *pack = malloc(need);
	size_t sz = 0;
	CHECK(tessera_pack_finalize(pb, pack, need, &sz) == TESSERA_OK);
	CHECK(sz == need);

	tessera_pack_reader_t *pr = tessera_pack_open(pack, sz);
	CHECK(pr != NULL);
	CHECK(tessera_pack_blob_count(pr) == N);

	for (uint32_t i = 0; i < N; i++) {
		const uint8_t *out;
		uint32_t       outlen;
		CHECK(tessera_pack_bloom_might_contain(pr, hashes[i]) == 1);
		CHECK(tessera_pack_lookup(pr, hashes[i], &out, &outlen)
		    == TESSERA_OK);
		CHECK(outlen == blob_lens[i]);
		CHECK(memcmp(out, blob_bytes[i], outlen) == 0);
	}

	/* Bloom false-positive rate on random absent hashes. With 10 bits
	 * per blob and ~7 hashes the expected FPR is ~1 %. We assert it
	 * stays below 5 % over 1 000 trials so the test isn't flaky. */
	uint32_t bloom_hits = 0;
	for (uint32_t i = 0; i < 1000; i++) {
		uint8_t buf[33];
		buf[0] = 'X';
		for (int k = 0; k < 32; k++) buf[k+1] = (uint8_t)(i * 31 + k);
		tessera_hash_t h;
		tessera_sha256(buf, 33, h);

		const uint8_t *bytes; uint32_t len;
		int absent = tessera_pack_lookup(pr, h, &bytes, &len);
		CHECK(absent == TESSERA_ENOENT);
		if (tessera_pack_bloom_might_contain(pr, h)) bloom_hits++;
	}
	printf("  bloom FPR over 1000 absent hashes: %u/1000 = %.2f%%\n",
	    bloom_hits, bloom_hits / 10.0);
	CHECK(bloom_hits < 50);

	tessera_pack_close(pr);
	tessera_pack_free(pb);
	for (uint32_t i = 0; i < N; i++) free(blob_bytes[i]);
	free(blob_bytes); free(blob_lens); free(hashes); free(pack);
}

static void
test_duplicate_rejected(void)
{
	const uint8_t pack_id[16] = { 'P','A','C','K','D','U','P','L','I','C','A','T','E','I','D','X' };
	tessera_pack_builder_t *pb = tessera_pack_begin(0, pack_id, 1);
	uint8_t bytes[16];
	memset(bytes, 0xab, sizeof bytes);
	tessera_hash_t h;
	tessera_sha256(bytes, sizeof bytes, h);
	CHECK(tessera_pack_add_blob(pb, h, bytes, sizeof bytes, 0) == TESSERA_OK);
	CHECK(tessera_pack_add_blob(pb, h, bytes, sizeof bytes, 0) == TESSERA_OK);

	uint8_t buf[64 * 1024]; size_t sz = 0;
	CHECK(tessera_pack_finalize(pb, buf, sizeof buf, &sz) == TESSERA_EEXIST);
	tessera_pack_free(pb);
}

static void
test_corruption_detected(void)
{
	const uint8_t pack_id[16] = { 'P','A','C','K','C','O','R','R','U','P','T','I','D','X','Y','Z' };
	tessera_pack_builder_t *pb = tessera_pack_begin(0, pack_id, 0);
	uint8_t bytes[100];
	memset(bytes, 0x55, sizeof bytes);
	tessera_hash_t h;
	tessera_sha256(bytes, sizeof bytes, h);
	CHECK(tessera_pack_add_blob(pb, h, bytes, sizeof bytes, 0) == TESSERA_OK);

	size_t need = 0;
	(void)tessera_pack_finalize(pb, NULL, 0, &need);
	uint8_t *pack = malloc(need); size_t sz = 0;
	CHECK(tessera_pack_finalize(pb, pack, need, &sz) == TESSERA_OK);

	/* Sanity: opens cleanly. */
	tessera_pack_reader_t *pr = tessera_pack_open(pack, sz);
	CHECK(pr != NULL);
	tessera_pack_close(pr);

	/* Flip a byte inside the data area. The pack-wide CRC must catch
	 * it on the next open. */
	tessera_pack_header_t hh;
	(void)tessera_decode_pack_header(pack, &hh);
	pack[hh.data_offset + sizeof(tessera_blob_descriptor_t) + 10] ^= 0xff;
	pr = tessera_pack_open(pack, sz);
	CHECK(pr == NULL);

	free(pack);
	tessera_pack_free(pb);
}

int
main(void)
{
	printf("test_pack: builder + reader round-trip + bloom FPR + corruption detection\n");
	test_round_trip();
	test_duplicate_rejected();
	test_corruption_detected();
	if (failures > 0) {
		fprintf(stderr, "%d failure(s)\n", failures);
		return 1;
	}
	printf("ok\n");
	return 0;
}
