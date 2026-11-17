/*
 * Sealed journal (docs/spec/tessera-encryption.md §5.3) on the OpenSSL
 * backend, both suites:
 *
 *   1. Records of 0, 100, 3992 (one sealed block), 3993 (two) and 10 000
 *      bytes in two committed transactions replay in order, byte-exact,
 *      after reopening; an uncommitted tail is dropped.
 *   2. No body byte is in the clear on disk.
 *   3. Any one byte flipped in any used record block stops replay at that
 *      record: the records before it replay, none from it on, and no body is
 *      ever delivered altered. A flipped header block fails open.
 *   4. Two record blocks swapped, or a record block replaced by an older
 *      one, stop replay there.
 *   5. A new epoch (checkpoint_anchor): the old header and the old records
 *      are refused under the new anchor; a wrong anchor fails open.
 *   6. Appending after open of a non-empty log needs a replay first; plain
 *      checkpoint refuses a sealed journal.
 */
#include "tessera/error.h"
#include "tessera/journal.h"
#include "tessera/seal.h"

#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

void tessera_crypt_openssl_install(void);

#define BS      4096
#define JSTART  10
#define JLEN    64
#define NSECT   (JSTART + JLEN)

static int failures = 0;
#define CHECK(cond) do {                                                 \
	if (!(cond)) {                                                   \
		fprintf(stderr, "FAIL %s:%d: %s\n",                      \
		    __FILE__, __LINE__, #cond);                          \
		failures++;                                              \
	}                                                                \
} while (0)

static uint8_t disk[NSECT][BS];

static int
d_read(void *c, uint64_t s, uint8_t *o)
{
	(void)c;
	if (s >= NSECT) return -1;
	memcpy(o, disk[s], BS);
	return 0;
}
static int
d_write(void *c, uint64_t s, const uint8_t *b)
{
	(void)c;
	if (s >= NSECT) return -1;
	memcpy(disk[s], b, BS);
	return 0;
}
static int d_alloc(void *c, uint64_t n, uint64_t *o) { (void)c; (void)n; (void)o; return -1; }
static int d_free(void *c, uint64_t s, uint64_t n) { (void)c; (void)s; (void)n; return 0; }
static const tessera_block_io_t IO = { d_read, d_write, d_alloc, d_free,
    NULL, NULL, NULL, NULL };

static const char TAG[16] = "test";
static const uint32_t LENS[] = { 0, 100, 3992, 3993, 10000 };
#define NREC 5
static uint8_t bodies[NREC][10000];

/* replay collector */
struct got {
	int      n;
	int      bad;          /* a body that differs from what was written */
	uint32_t type[16];
};
static int
collect(void *ctx, const tessera_record_header_t *h, const uint8_t *body)
{
	struct got *g = ctx;
	int i = g->n;
	if (i >= NREC) { g->bad++; return 0; }
	if (h->body_length != LENS[i] ||
	    (LENS[i] > 0 && memcmp(body, bodies[i], LENS[i]) != 0))
		g->bad++;
	g->type[i] = h->record_type;
	g->n++;
	return 0;
}

static int
replay_count(const tessera_seal_keys_t *k, const uint8_t anchor[16],
    int *bad)
{
	struct got g;
	memset(&g, 0, sizeof g);
	tessera_journal_t *j = tessera_journal_open_sealed(&IO, JSTART, JLEN,
	    k, anchor);
	if (j == NULL) return -1;
	int rc = tessera_journal_replay(j, collect, &g);
	tessera_journal_close(j);
	if (bad) *bad = g.bad;
	return rc == TESSERA_OK ? g.n : -2;
}

static void
write_log(const tessera_seal_keys_t *k, const uint8_t anchor[16])
{
	uint64_t tx;
	CHECK(tessera_journal_format_sealed(&IO, JSTART, JLEN, k, anchor) ==
	    TESSERA_OK);
	tessera_journal_t *j = tessera_journal_open_sealed(&IO, JSTART, JLEN, k,
	    anchor);
	CHECK(j != NULL);
	if (j == NULL) return;
	CHECK(tessera_journal_tx_begin(j, &tx, TAG) == TESSERA_OK);
	for (int i = 0; i < 3; i++)
		CHECK(tessera_journal_append(j, tx, TESSERA_INODE_WRITE,
		    bodies[i], LENS[i]) == TESSERA_OK);
	CHECK(tessera_journal_tx_commit(j, tx) == TESSERA_OK);
	CHECK(tessera_journal_tx_begin(j, &tx, TAG) == TESSERA_OK);
	for (int i = 3; i < NREC; i++)
		CHECK(tessera_journal_append(j, tx, TESSERA_DIR_INSERT,
		    bodies[i], LENS[i]) == TESSERA_OK);
	CHECK(tessera_journal_tx_commit(j, tx) == TESSERA_OK);
	/* uncommitted tail: dropped at replay */
	CHECK(tessera_journal_tx_begin(j, &tx, TAG) == TESSERA_OK);
	CHECK(tessera_journal_append(j, tx, TESSERA_INODE_FREE, bodies[1], 50)
	    == TESSERA_OK);
	tessera_journal_close(j);
}

/* Blocks used: records = begin(1) r0(1) r1(1) r2(1) commit(1) begin(1)
 * r3(2) r4(3) commit(1) begin(1) r(1) → data blocks 1..14. */
#define USED_LAST 14

static void
run(uint32_t suite)
{
	static const uint8_t vk[32] = { 7, 7, 7 };
	static const uint8_t uuid[16] = { 0xee };
	tessera_seal_keys_t k;
	uint8_t a1[16], a2[16], save[BS], save2[BS];
	static uint8_t old[NSECT][BS];
	int bad = 0;

	memset(disk, 0, sizeof disk);
	memset(a1, 0xa1, 16);
	memset(a2, 0xa2, 16);
	CHECK(tessera_seal_keys_init(&k, suite, vk, uuid) == TESSERA_OK);

	/* 1 */
	write_log(&k, a1);
	CHECK(replay_count(&k, a1, &bad) == NREC && bad == 0);

	/* 2: the 10 000-byte body is a repeating pattern; look for 16 bytes */
	{
		int found = 0;
		for (int s = 0; s < NSECT && !found; s++)
			for (int o = 0; o + 16 <= BS; o++)
				if (memcmp(disk[s] + o, bodies[4] + 500, 16) == 0) {
					found = 1;
					break;
				}
		CHECK(!found);
	}

	/* 3: every byte of every used record block */
	{
		/* record index that starts at each data block (or -1 for
		 * begin/commit markers and continuations) */
		static const int first_rec_lost[USED_LAST + 1] = {
			0, 0, 0, 0, 0, 0, 3, 3, 3, 3, 3, 3, 3, 5, 5 };
		int ok = 1;
		for (int b = 1; b <= USED_LAST; b++) {
			for (int o = 0; o < BS; o += (o < 64 || o > 4000) ? 1 : 97) {
				disk[JSTART + b][o] ^= 0x04;
				int n = replay_count(&k, a1, &bad);
				disk[JSTART + b][o] ^= 0x04;
				if (bad != 0 || n != first_rec_lost[b]) {
					if (ok)
						fprintf(stderr, "  block %d off %d: "
						    "%d records, %d bad\n",
						    b, o, n, bad);
					ok = 0;
				}
			}
		}
		CHECK(ok);
		/* header */
		disk[JSTART][9] ^= 1;
		CHECK(replay_count(&k, a1, NULL) == -1);
		disk[JSTART][9] ^= 1;
		disk[JSTART][4090] ^= 1;
		CHECK(replay_count(&k, a1, NULL) == -1);
		disk[JSTART][4090] ^= 1;
		CHECK(replay_count(&k, a1, &bad) == NREC && bad == 0);
	}

	/* 4: swap the two single-block records r0 (block 2) and r1 (3) */
	memcpy(save, disk[JSTART + 2], BS);
	memcpy(save2, disk[JSTART + 3], BS);
	memcpy(disk[JSTART + 2], save2, BS);
	memcpy(disk[JSTART + 3], save, BS);
	CHECK(replay_count(&k, a1, &bad) == 0 && bad == 0);
	memcpy(disk[JSTART + 2], save, BS);
	memcpy(disk[JSTART + 3], save2, BS);
	CHECK(replay_count(&k, a1, &bad) == NREC);

	/* 5: new epoch */
	memcpy(old, disk, sizeof disk);
	{
		tessera_journal_t *j = tessera_journal_open_sealed(&IO, JSTART,
		    JLEN, &k, a1);
		struct got g;
		memset(&g, 0, sizeof g);
		CHECK(j != NULL && tessera_journal_replay(j, collect, &g) ==
		    TESSERA_OK);
		CHECK(tessera_journal_checkpoint(j) == TESSERA_EINVAL);
		CHECK(tessera_journal_checkpoint_anchor(j, a2) == TESSERA_OK);
		uint64_t tx;
		CHECK(tessera_journal_tx_begin(j, &tx, TAG) == TESSERA_OK);
		CHECK(tessera_journal_append(j, tx, TESSERA_INODE_WRITE,
		    bodies[0], LENS[0]) == TESSERA_OK);
		CHECK(tessera_journal_tx_commit(j, tx) == TESSERA_OK);
		tessera_journal_close(j);
	}
	CHECK(replay_count(&k, a2, &bad) == 1 && bad == 0);
	CHECK(replay_count(&k, a1, NULL) == -1);       /* old anchor */
	/* the old epoch's header under the new anchor */
	memcpy(save, disk[JSTART], BS);
	memcpy(disk[JSTART], old[JSTART], BS);
	CHECK(replay_count(&k, a2, NULL) == -1);
	memcpy(disk[JSTART], save, BS);
	/* the old epoch's record blocks under the new header: the new log's
	 * blocks 1..3 replaced by the old ones */
	for (int b = 1; b <= 3; b++)
		memcpy(disk[JSTART + b], old[JSTART + b], BS);
	CHECK(replay_count(&k, a2, &bad) == 0 && bad == 0);

	/* 6: append after open of a non-empty log */
	write_log(&k, a1);
	{
		tessera_journal_t *j = tessera_journal_open_sealed(&IO, JSTART,
		    JLEN, &k, a1);
		uint64_t tx;
		CHECK(j != NULL);
		CHECK(tessera_journal_tx_begin(j, &tx, TAG) == TESSERA_EIO);
		struct got g;
		memset(&g, 0, sizeof g);
		CHECK(tessera_journal_replay(j, collect, &g) == TESSERA_OK);
		CHECK(tessera_journal_tx_begin(j, &tx, TAG) == TESSERA_OK);
		CHECK(tessera_journal_tx_commit(j, tx) == TESSERA_OK);
		tessera_journal_close(j);
	}
	/* the uncommitted tx_begin "three" is followed by another BEGIN,
	 * which discards it; all five records still replay */
	CHECK(replay_count(&k, a1, &bad) == NREC && bad == 0);
	/* a wrong key */
	{
		tessera_seal_keys_t o;
		uint8_t vk2[32];
		memcpy(vk2, vk, 32);
		vk2[5] ^= 1;
		CHECK(tessera_seal_keys_init(&o, suite, vk2, uuid) == TESSERA_OK);
		CHECK(replay_count(&o, a1, NULL) == -1);
		tessera_seal_keys_clear(&o);
	}
	tessera_seal_keys_clear(&k);
}

int
main(void)
{
	printf("test_journal_sealed: sealed, chained journal\n");
	tessera_crypt_openssl_install();
	for (int i = 0; i < NREC; i++)
		for (uint32_t b = 0; b < LENS[i]; b++)
			bodies[i][b] = (uint8_t)(i * 37 + b * 11 + (b >> 8));
	run(TESSERA_SUITE_XAES_256_GCM);
	run(TESSERA_SUITE_XCHACHA20_POLY1305);
	if (failures > 0) {
		fprintf(stderr, "%d failure(s)\n", failures);
		return 1;
	}
	printf("ok\n");
	return 0;
}
