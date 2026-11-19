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
#include <unistd.h>

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

/* bloom_hash_count comes off disk. A pack claiming 2^32-1 made every
 * lookup of a present blob loop 2^32 times; the reader caps probes at 64.
 * alarm() turns a regression into a failure instead of a hang. */
static void
test_bloom_probe_cap(void)
{
	const uint8_t pack_id[16] = { 'P','A','C','K','B','L','O','O','M','C','A','P','T','E','S','T' };
	tessera_pack_builder_t *pb = tessera_pack_begin(0, pack_id, 0);
	uint8_t bytes[100];
	memset(bytes, 0x77, sizeof bytes);
	tessera_hash_t h;
	tessera_sha256(bytes, sizeof bytes, h);
	CHECK(tessera_pack_add_blob(pb, h, bytes, sizeof bytes, 0) == TESSERA_OK);
	size_t need = 0;
	(void)tessera_pack_finalize(pb, NULL, 0, &need);
	uint8_t *pack = malloc(need); size_t sz = 0;
	CHECK(tessera_pack_finalize(pb, pack, need, &sz) == TESSERA_OK);

	tessera_pack_header_t hh;
	CHECK(tessera_decode_pack_header(pack, &hh) == TESSERA_OK);
	hh.bloom_hash_count = UINT32_MAX;
	CHECK(tessera_encode_pack_header(&hh, pack) == TESSERA_OK);
	/* Every bit set, so no probe can end the loop early. */
	memset(pack + TESSERA_SECTOR_SIZE * (1 + hh.index_blocks), 0xff,
	    hh.bloom_bytes);

	tessera_pack_reader_t *pr = tessera_pack_open(pack, sz);
	CHECK(pr != NULL);
	if (pr != NULL) {
		alarm(10);
		CHECK(tessera_pack_bloom_might_contain(pr, h) == 1);
		alarm(0);
		tessera_pack_close(pr);
	}
	free(pack);
	tessera_pack_free(pb);
}

/* Partial-read API (the kernel's path): geometry from the header sector
 * alone, and a search of a separately read index, must agree with the
 * whole-pack reader and refuse entries that point outside the data. */
static void
test_partial_read_api(void)
{
	const uint8_t pack_id[16] = { 'P','A','C','K','P','A','R','T','I','A','L','R','E','A','D','S' };
	tessera_pack_builder_t *pb = tessera_pack_begin(0, pack_id, 0);
	enum { N = 40 };
	uint8_t *bytes[N]; uint32_t lens[N]; tessera_hash_t hs[N];
	for (uint32_t i = 0; i < N; i++) {
		gen_blob(i, &bytes[i], &lens[i], hs[i]);
		CHECK(tessera_pack_add_blob(pb, hs[i], bytes[i], lens[i], 0) == TESSERA_OK);
	}
	size_t need = 0;
	(void)tessera_pack_finalize(pb, NULL, 0, &need);
	uint8_t *pack = malloc(need); size_t sz = 0;
	CHECK(tessera_pack_finalize(pb, pack, need, &sz) == TESSERA_OK);
	tessera_pack_reader_t *pr = tessera_pack_open(pack, sz);
	CHECK(pr != NULL);

	tessera_pack_geom_t g;
	CHECK(tessera_pack_geom_from_header(pack, sz, &g) == TESSERA_OK);
	CHECK(g.blob_count == N);
	/* More bytes on disk than the pack claims is fine (sector padding);
	 * fewer is not. */
	CHECK(tessera_pack_geom_from_header(pack, sz + 8192, &g) == TESSERA_OK);
	CHECK(tessera_pack_geom_from_header(pack, sz - 1, &g) == TESSERA_ECORRUPT);
	const uint8_t *idx = pack + g.index_off;
	const size_t idx_len = (size_t)(g.bloom_off - g.index_off);
	for (uint32_t i = 0; i < N && pr != NULL; i++) {
		uint64_t off = 0; uint32_t n = 0;
		const uint8_t *want = NULL; uint32_t wn = 0;
		CHECK(tessera_pack_index_find(idx, idx_len, &g, hs[i], &off, &n) == TESSERA_OK);
		CHECK(tessera_pack_lookup(pr, hs[i], &want, &wn) == TESSERA_OK);
		CHECK(n == wn && n == lens[i]);
		CHECK(pack + off + sizeof(tessera_blob_descriptor_t) == want);
	}
	tessera_hash_t absent;
	memset(absent, 0x5a, sizeof absent);
	uint64_t off; uint32_t n;
	CHECK(tessera_pack_index_find(idx, idx_len, &g, absent, &off, &n) == TESSERA_ENOENT);
	/* An index shorter than blob_count entries is refused, not overread. */
	CHECK(tessera_pack_index_find(idx, 48, &g, hs[N - 1], &off, &n) == TESSERA_ECORRUPT);

	/* An entry whose data_offset sits near 2^64: offset + size wraps, the
	 * old kernel check passed it, and the read left the pack. */
	tessera_pack_index_entry_t *e = (tessera_pack_index_entry_t *)
	    (pack + g.index_off);
	e->data_offset = UINT64_MAX - 8;
	CHECK(tessera_pack_index_find(idx, idx_len, &g, e->blob_hash, &off, &n) == TESSERA_ECORRUPT);
	e->data_offset = g.data_len - 4;
	CHECK(tessera_pack_index_find(idx, idx_len, &g, e->blob_hash, &off, &n) == TESSERA_ECORRUPT);
	/* Exact boundary: descriptor plus data ending at the region's end is
	 * fine, one byte more is not (sweeps catch this, fuzzing did not). */
	const uint64_t dsz = sizeof(tessera_blob_descriptor_t);
	for (uint32_t k = 0; k < 64; k++) {
		e->data_offset = g.data_len - dsz - k;
		e->data_size = k;
		CHECK(tessera_pack_index_find(idx, idx_len, &g, e->blob_hash, &off, &n) == TESSERA_OK);
		CHECK(off == g.data_off + g.data_len - dsz - k && n == k);
		e->data_size = k + 1;
		CHECK(tessera_pack_index_find(idx, idx_len, &g, e->blob_hash, &off, &n) == TESSERA_ECORRUPT);
	}

	/* A header whose data region runs past its own size. */
	tessera_pack_header_t hh;
	CHECK(tessera_decode_pack_header(pack, &hh) == TESSERA_OK);
	hh.data_length = hh.total_pack_bytes;
	CHECK(tessera_encode_pack_header(&hh, pack) == TESSERA_OK);
	CHECK(tessera_pack_geom_from_header(pack, sz, &g) == TESSERA_ECORRUPT);

	tessera_pack_close(pr);
	for (uint32_t i = 0; i < N; i++) free(bytes[i]);
	free(pack);
	tessera_pack_free(pb);
}

/* The scrub's compaction: dropping some blobs keeps the pack id, kind and
 * creator and every other blob byte for byte; the dropped blobs are gone
 * from both index and data; nothing to drop is ENOENT. */
static void
test_compact(void)
{
	const uint8_t pack_id[16] = { 'P','A','C','K','C','O','M','P','A','C','T','T','E','S','T','1' };
	tessera_pack_builder_t *pb = tessera_pack_begin(2, pack_id, 77);
	enum { N = 30 };
	uint8_t *bytes[N]; uint32_t lens[N]; tessera_hash_t hs[N];
	for (uint32_t i = 0; i < N; i++) {
		gen_blob(i + 1000, &bytes[i], &lens[i], hs[i]);
		CHECK(tessera_pack_add_blob(pb, hs[i], bytes[i], lens[i], i & 1) == TESSERA_OK);
	}
	size_t need = 0;
	(void)tessera_pack_finalize(pb, NULL, 0, &need);
	uint8_t *pack = malloc(need); size_t sz = 0;
	CHECK(tessera_pack_finalize(pb, pack, need, &sz) == TESSERA_OK);

	tessera_hash_t drop[N / 3 + 1];
	uint32_t nd = 0;
	for (uint32_t i = 0; i < N; i += 3)
		memcpy(drop[nd++], hs[i], sizeof(tessera_hash_t));
	uint8_t *out = malloc(sz); size_t osz = 0;
	uint32_t kept = 0, dropped = 0;
	CHECK(tessera_pack_compact(pack, sz, drop, nd, NULL, out, &osz, &kept, &dropped) == TESSERA_OK);
	CHECK(dropped == nd && kept == N - nd);
	CHECK(osz > 0 && osz < sz);
	tessera_pack_reader_t *pr = tessera_pack_open(out, osz);
	CHECK(pr != NULL);
	if (pr != NULL) {
		CHECK(memcmp(tessera_pack_pack_id(pr), pack_id, 16) == 0);
		CHECK(tessera_pack_blob_count(pr) == kept);
		for (uint32_t i = 0; i < N; i++) {
			const uint8_t *b = NULL; uint32_t bn = 0;
			int r = tessera_pack_lookup(pr, hs[i], &b, &bn);
			if (i % 3 == 0) {
				CHECK(r == TESSERA_ENOENT);
			} else {
				CHECK(r == TESSERA_OK && bn == lens[i] &&
				    memcmp(b, bytes[i], bn) == 0);
			}
		}
		tessera_pack_close(pr);
	}
	tessera_pack_header_t hh;
	CHECK(tessera_decode_pack_header(out, &hh) == TESSERA_OK);
	CHECK(hh.pack_kind == 2 && hh.creator_tx_id == 77);
	/* No dropped blob's bytes survive anywhere in the output. */
	for (uint32_t i = 0; i < N; i += 3) {
		int found = 0;
		for (size_t o = 0; o + 64 <= osz && !found; o++)
			found = memcmp(out + o, bytes[i], 64) == 0;
		CHECK(!found);
	}

	tessera_hash_t absent;
	memset(absent, 0x5a, sizeof absent);
	CHECK(tessera_pack_compact(pack, sz, &absent, 1, NULL, out, &osz, &kept, &dropped) == TESSERA_ENOENT);
	CHECK(tessera_pack_compact(pack, sz, NULL, 0, NULL, out, &osz, NULL, NULL) == TESSERA_ENOENT);
	/* A new id goes into the header. */
	const uint8_t nid[16] = { 'N','E','W','I','D',0,1,2,3,4,5,6,7,8,9,10 };
	CHECK(tessera_pack_compact(pack, sz, drop, nd, nid, out, &osz, &kept, &dropped) == TESSERA_OK);
	pr = tessera_pack_open(out, osz);
	CHECK(pr != NULL && memcmp(tessera_pack_pack_id(pr), nid, 16) == 0);
	if (pr != NULL)
		tessera_pack_close(pr);
	/* Dropping everything still gives a valid, empty pack. */
	CHECK(tessera_pack_compact(pack, sz, hs, N, NULL, out, &osz, &kept, &dropped) == TESSERA_OK);
	CHECK(kept == 0 && dropped == N);
	pr = tessera_pack_open(out, osz);
	CHECK(pr != NULL && tessera_pack_blob_count(pr) == 0);
	if (pr != NULL)
		tessera_pack_close(pr);

	for (uint32_t i = 0; i < N; i++) free(bytes[i]);
	free(pack); free(out);
	tessera_pack_free(pb);
}

int
main(void)
{
	printf("test_pack: builder + reader round-trip + bloom FPR + corruption detection\n");
	test_round_trip();
	test_duplicate_rejected();
	test_corruption_detected();
	test_bloom_probe_cap();
	test_partial_read_api();
	test_compact();
	if (failures > 0) {
		fprintf(stderr, "%d failure(s)\n", failures);
		return 1;
	}
	printf("ok\n");
	return 0;
}
