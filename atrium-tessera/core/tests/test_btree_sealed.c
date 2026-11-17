/*
 * Sealed B+tree (docs/spec/tessera-encryption.md §5.3) on the OpenSSL
 * backend, both suites:
 *
 *   1. A random workload of puts, sorted batches and deletes, checked
 *      against a model after every step: every get, a full cursor walk,
 *      and walk_nodes. Three levels deep.
 *   2. Nothing on the "disk" is plaintext: no stored key appears in any
 *      sector.
 *   3. Any one byte flipped in any live node is refused (EAUTH and
 *      FAIL_AUTH) by a full walk, and the tree reads again once restored.
 *   4. An older copy of a live node, written back at the same sector, is
 *      refused against its parent's tag (or the root tag).
 *   5. Opening with a wrong root tag, the wrong tree kind, other keys, or
 *      without sealing at all is refused.
 *   6. Sealed fan-out is computed on the 4056-byte payload.
 */
#include "tessera/btree.h"
#include "tessera/error.h"
#include "tessera/seal.h"

#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

void tessera_crypt_openssl_install(void);

#define BS          4096
#define MAX_SECTORS 4096
#define KS          64u   /* inner fan-out 45: three levels */
#define VS          40u
#define NKEYS       6000u
#define HIST        3

static int failures = 0;
#define CHECK(cond) do {                                                 \
	if (!(cond)) {                                                   \
		fprintf(stderr, "FAIL %s:%d: %s\n",                      \
		    __FILE__, __LINE__, #cond);                          \
		failures++;                                              \
	}                                                                \
} while (0)

/* ── a disk that remembers the last few images of each sector ─────── */

struct disk {
	uint8_t  blocks[MAX_SECTORS][BS];
	uint8_t  used[MAX_SECTORS];
	uint8_t  hist[MAX_SECTORS][HIST][BS];
	uint8_t  nhist[MAX_SECTORS];
};
static struct disk *D;

static int
d_read(void *ctx, uint64_t s, uint8_t *out)
{
	(void)ctx;
	if (s >= MAX_SECTORS || !D->used[s]) return -1;
	memcpy(out, D->blocks[s], BS);
	return 0;
}

static int
d_write(void *ctx, uint64_t s, const uint8_t *buf)
{
	(void)ctx;
	if (s >= MAX_SECTORS) return -1;
	if (D->nhist[s] == HIST)
		memmove(D->hist[s][0], D->hist[s][1], (HIST - 1) * BS);
	else
		D->nhist[s]++;
	memcpy(D->hist[s][D->nhist[s] - 1], buf, BS);
	memcpy(D->blocks[s], buf, BS);
	return 0;
}

static uint64_t next_s = 1;

static int
d_alloc(void *ctx, uint64_t n, uint64_t *out)
{
	(void)ctx;
	if (n != 1) return -1;
	for (uint64_t k = 0; k < MAX_SECTORS; k++) {
		uint64_t i = 1 + (next_s + k) % (MAX_SECTORS - 1);
		if (!D->used[i]) {
			D->used[i] = 1;
			next_s = i + 1;
			*out = i;
			return 0;
		}
	}
	return -1;
}

static int
d_free(void *ctx, uint64_t s, uint64_t n)
{
	(void)ctx;
	for (uint64_t i = 0; i < n; i++)
		D->used[s + i] = 0;
	return 0;
}

static const tessera_block_io_t IO = {
	.read_block = d_read, .write_block = d_write,
	.alloc = d_alloc, .free = d_free,
};

/* ── model ─────────────────────────────────────────────────────────── */

static uint8_t present[NKEYS];
static uint32_t ver[NKEYS];

static void
mkkey(uint32_t i, uint8_t k[KS])
{
	memset(k, 0, KS);
	k[0] = 'K'; k[1] = 'E'; k[2] = 'Y';
	k[12] = (uint8_t)(i >> 24); k[13] = (uint8_t)(i >> 16);
	k[14] = (uint8_t)(i >> 8);  k[15] = (uint8_t)i;
}

static void
mkval(uint32_t i, uint32_t v, uint8_t out[VS])
{
	for (unsigned j = 0; j < VS; j++)
		out[j] = (uint8_t)(i * 31u + v * 7u + j);
}

static uint32_t rng = 12345;
static uint32_t
rnd(void)
{
	rng ^= rng << 13; rng ^= rng >> 17; rng ^= rng << 5;
	return rng;
}

/* Full check against the model. Returns the first error, if any. */
static int
check_all(tessera_btree_t *t)
{
	uint8_t k[KS], v[VS], want[VS];
	uint32_t seen = 0, expect = 0;
	int rc;

	for (uint32_t i = 0; i < NKEYS; i++) {
		mkkey(i, k);
		rc = tessera_btree_get(t, k, v);
		if (present[i]) {
			expect++;
			if (rc != TESSERA_OK) return rc;
			mkval(i, ver[i], want);
			if (memcmp(v, want, VS) != 0) return TESSERA_ECORRUPT;
		} else if (rc != TESSERA_ENOENT) {
			return rc == TESSERA_OK ? TESSERA_ECORRUPT : rc;
		}
	}
	tessera_btree_cursor_t *c = tessera_btree_seek_first(t);
	if (c == NULL) return TESSERA_EAUTH;
	uint8_t prev[KS];
	int first = 1;
	while (tessera_btree_cursor_get(c, k, v) == TESSERA_OK) {
		if (!first && memcmp(prev, k, KS) >= 0) {
			tessera_btree_cursor_free(c);
			return TESSERA_ECORRUPT;
		}
		memcpy(prev, k, KS);
		first = 0;
		seen++;
		rc = tessera_btree_cursor_next(c);
		if (rc != TESSERA_OK && rc != TESSERA_ENOENT) {
			tessera_btree_cursor_free(c);
			return rc;
		}
	}
	tessera_btree_cursor_free(c);
	return seen == expect ? TESSERA_OK : TESSERA_ECORRUPT;
}

/* walk_nodes: collect the live node sectors. */
static uint64_t live[MAX_SECTORS];
static uint32_t nlive;
static int
collect(void *ctx, uint64_t s)
{
	(void)ctx;
	live[nlive++] = s;
	return 0;
}

static int
walk_ok(tessera_btree_t *t)
{
	nlive = 0;
	return tessera_btree_walk_nodes(t, collect, NULL);
}

static void
workload(tessera_btree_t *t, uint64_t *root)
{
	uint8_t k[KS], v[VS];
	static uint8_t bk[512 * KS], bv[512 * VS];

	for (int round = 0; round < 40; round++) {
		/* singles */
		for (int j = 0; j < 150; j++) {
			uint32_t i = rnd() % NKEYS;
			mkkey(i, k);
			if (present[i] && rnd() % 3 == 0) {
				CHECK(tessera_btree_delete(t, k, root) == TESSERA_OK);
				present[i] = 0;
			} else {
				ver[i]++;
				mkval(i, ver[i], v);
				CHECK(tessera_btree_put(t, k, v, root) == TESSERA_OK);
				present[i] = 1;
			}
		}
		/* a sorted batch over a random run of keys */
		uint32_t start = rnd() % NKEYS, n = 0;
		for (uint32_t i = start; i < NKEYS && n < 512; i += 1 + rnd() % 3) {
			mkkey(i, bk + (size_t)n * KS);
			ver[i]++;
			mkval(i, ver[i], bv + (size_t)n * VS);
			present[i] = 1;
			n++;
		}
		if (n > 0)
			CHECK(tessera_btree_put_sorted_batch(t, bk, bv, n, root) ==
			    TESSERA_OK);
		if (round % 8 == 7)
			CHECK(check_all(t) == TESSERA_OK);
	}
	CHECK(check_all(t) == TESSERA_OK);
}

static void
run(uint32_t suite)
{
	static const uint8_t vk[32] = { 9, 8, 7, 6, 5, 4, 3, 2, 1 };
	static const uint8_t uuid[16] = { 0x11, 0x22, 0x33 };
	tessera_seal_keys_t keys, other;
	uint8_t tag[16], tag2[16], save[BS], probe[BS];
	uint64_t root;
	uint8_t vk2[32];

	memset(D, 0, sizeof(*D));
	memset(present, 0, sizeof(present));
	memset(ver, 0, sizeof(ver));
	CHECK(tessera_seal_keys_init(&keys, suite, vk, uuid) == TESSERA_OK);

	tessera_btree_t *t = tessera_btree_create_sealed(&IO, &keys,
	    TESSERA_BTREE_KIND_INODE, KS, VS, &root, tag);
	CHECK(t != NULL);
	if (t == NULL) return;
	workload(t, &root);
	tessera_btree_root_tag(t, tag);
	CHECK(tessera_btree_root(t) == root);
	CHECK(walk_ok(t) == TESSERA_OK);
	{
		tessera_btree_trace_t tr;
		uint8_t k[KS], v[VS];
		mkkey(NKEYS / 2, k);
		(void)tessera_btree_get_traced(t, k, v, &tr);
		CHECK(tr.depth >= 3);
	}
	printf("  suite %u: %u live nodes\n", (unsigned)suite, (unsigned)nlive);

	/* 2. no plaintext key on disk */
	{
		uint8_t k[KS];
		int found = 0;
		mkkey(4321, k);
		/* every key starts "KEY" and nine zero bytes */
		for (uint32_t s = 0; s < MAX_SECTORS && !found; s++)
			for (uint32_t o = 0; o + KS <= BS; o++)
				if (D->blocks[s][o] == 'K' &&
				    memcmp(D->blocks[s] + o, k, 12) == 0) {
					found = 1;
					break;
				}
		CHECK(!found);
	}

	/* reopen from root + tag */
	tessera_btree_close(t);
	t = tessera_btree_open_sealed(&IO, &keys, root, tag,
	    TESSERA_BTREE_KIND_INODE, KS, VS);
	CHECK(t != NULL && check_all(t) == TESSERA_OK);

	/* 3. any byte of any live node: sample offsets across every node,
	 * every offset of the first, last and a middle node */
	CHECK(walk_ok(t) == TESSERA_OK);
	uint32_t n = nlive, refused = 0, tried = 0;
	static uint64_t lv[MAX_SECTORS];
	memcpy(lv, live, n * sizeof(uint64_t));
	for (uint32_t i = 0; i < n; i++) {
		uint64_t s = lv[i];
		int full = (i == 0 || i == n / 2 || i == n - 1);
		for (uint32_t o = full ? 0 : (s * 131) % 64; o < BS;
		    o += full ? 1 : 509) {
			memcpy(save, D->blocks[s], BS);
			D->blocks[s][o] ^= 0x20;
			int rc = walk_ok(t);
			tried++;
			uint64_t fs = 0;
			if (rc == TESSERA_EAUTH &&
			    tessera_btree_last_fail(t, &fs, NULL) ==
			    TESSERA_BTREE_FAIL_AUTH && fs == s)
				refused++;
			memcpy(D->blocks[s], save, BS);
		}
	}
	CHECK(refused == tried);
	printf("  suite %u: %u/%u single-byte flips refused\n",
	    (unsigned)suite, (unsigned)refused, (unsigned)tried);
	CHECK(check_all(t) == TESSERA_OK);

	/* 4. older copies written back at the same sector */
	uint32_t replays = 0, rrefused = 0;
	for (uint32_t i = 0; i < n; i++) {
		uint64_t s = lv[i];
		for (uint32_t h = 0; h + 1 < D->nhist[s]; h++) {
			if (memcmp(D->hist[s][h], D->blocks[s], BS) == 0)
				continue;
			memcpy(save, D->blocks[s], BS);
			memcpy(D->blocks[s], D->hist[s][h], BS);
			replays++;
			if (walk_ok(t) == TESSERA_EAUTH)
				rrefused++;
			memcpy(D->blocks[s], save, BS);
		}
	}
	CHECK(replays > 20);
	CHECK(rrefused == replays);
	printf("  suite %u: %u/%u older copies refused\n", (unsigned)suite,
	    (unsigned)rrefused, (unsigned)replays);

	/* 5. wrong root tag, kind, keys, or no sealing */
	tessera_btree_close(t);
	memcpy(tag2, tag, 16);
	tag2[3] ^= 1;
	t = tessera_btree_open_sealed(&IO, &keys, root, tag2,
	    TESSERA_BTREE_KIND_INODE, KS, VS);
	CHECK(t != NULL && walk_ok(t) == TESSERA_EAUTH);
	tessera_btree_close(t);
	t = tessera_btree_open_sealed(&IO, &keys, root, tag,
	    TESSERA_BTREE_KIND_QUOTA, KS, VS);
	CHECK(t != NULL && walk_ok(t) == TESSERA_EAUTH);
	tessera_btree_close(t);
	memcpy(vk2, vk, 32);
	vk2[31] ^= 1;
	CHECK(tessera_seal_keys_init(&other, suite, vk2, uuid) == TESSERA_OK);
	t = tessera_btree_open_sealed(&IO, &other, root, tag,
	    TESSERA_BTREE_KIND_INODE, KS, VS);
	CHECK(t != NULL && walk_ok(t) == TESSERA_EAUTH);
	tessera_btree_close(t);
	tessera_seal_keys_clear(&other);
	t = tessera_btree_open(&IO, root, TESSERA_BTREE_KIND_INODE, KS, VS);
	CHECK(t != NULL && walk_ok(t) != TESSERA_OK);
	tessera_btree_close(t);
	/* the stored root itself: a node moved to another sector */
	{
		uint64_t s2;
		CHECK(d_alloc(NULL, 1, &s2) == 0);
		memcpy(D->blocks[s2], D->blocks[root], BS);
		t = tessera_btree_open_sealed(&IO, &keys, s2, tag,
		    TESSERA_BTREE_KIND_INODE, KS, VS);
		CHECK(t != NULL && walk_ok(t) == TESSERA_EAUTH);
		tessera_btree_close(t);
		d_free(NULL, s2, 1);
	}
	(void)probe;

	/* empty after deleting everything, still sealed and usable */
	t = tessera_btree_open_sealed(&IO, &keys, root, tag,
	    TESSERA_BTREE_KIND_INODE, KS, VS);
	for (uint32_t i = 0; i < NKEYS; i++) {
		if (!present[i]) continue;
		uint8_t k[KS];
		mkkey(i, k);
		CHECK(tessera_btree_delete(t, k, &root) == TESSERA_OK);
		present[i] = 0;
	}
	CHECK(check_all(t) == TESSERA_OK);
	CHECK(walk_ok(t) == TESSERA_OK && nlive == 1);
	tessera_btree_root_tag(t, tag);
	tessera_btree_close(t);
	t = tessera_btree_open_sealed(&IO, &keys, root, tag,
	    TESSERA_BTREE_KIND_INODE, KS, VS);
	CHECK(check_all(t) == TESSERA_OK);
	tessera_btree_close(t);
	tessera_seal_keys_clear(&keys);
}

static void
test_fanout(void)
{
	static const uint8_t vk[32] = { 1 };
	static const uint8_t uuid[16] = { 2 };
	tessera_seal_keys_t keys;
	uint64_t root;
	uint8_t tag[16];
	uint8_t k[8], v[152];

	/* 4056 - 64 = 3992 bytes of entries: an 8+152 inode-sized entry
	 * fits 24 to a sealed leaf (25 unsealed), so the 25th put splits. */
	memset(D, 0, sizeof(*D));
	CHECK(tessera_seal_keys_init(&keys, TESSERA_SUITE_XAES_256_GCM, vk,
	    uuid) == TESSERA_OK);
	tessera_btree_t *t = tessera_btree_create_sealed(&IO, &keys, 0, 8, 152,
	    &root, tag);
	memset(v, 0x55, sizeof v);
	for (uint32_t i = 0; i < 24; i++) {
		memset(k, 0, 8);
		k[7] = (uint8_t)i;
		CHECK(tessera_btree_put(t, k, v, &root) == TESSERA_OK);
	}
	CHECK(walk_ok(t) == TESSERA_OK && nlive == 1);
	k[7] = 24;
	CHECK(tessera_btree_put(t, k, v, &root) == TESSERA_OK);
	CHECK(walk_ok(t) == TESSERA_OK && nlive == 3);
	tessera_btree_close(t);
	tessera_seal_keys_clear(&keys);
}

int
main(void)
{
	printf("test_btree_sealed: sealed B+tree nodes with child tags\n");
	tessera_crypt_openssl_install();
	D = calloc(1, sizeof(*D));
	if (D == NULL) return 2;
	run(TESSERA_SUITE_XAES_256_GCM);
	run(TESSERA_SUITE_XCHACHA20_POLY1305);
	test_fanout();
	free(D);
	if (failures > 0) {
		fprintf(stderr, "%d failure(s)\n", failures);
		return 1;
	}
	printf("ok\n");
	return 0;
}
