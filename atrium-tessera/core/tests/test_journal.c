/*
 * Tests for the circular-log journal.
 *
 *   1. format → open → empty replay (callback never invoked).
 *   2. one txn (BEGIN + N records + COMMIT) → replay yields exactly the
 *      N committed records, in order, with body bytes intact.
 *   3. mid-stream txn aborted via TX_ABORT → its records are not
 *      replayed, but a later committed txn IS.
 *   4. torn append (write BEGIN + records + crash before COMMIT) →
 *      replay drops the open txn cleanly.
 *   5. multi-block body (>4064 bytes) records round-trip through both
 *      append and replay.
 */

#include "tessera/journal.h"
#include "tessera/error.h"
#include "tessera/format.h"
#include "tessera/crc.h"

#include <stddef.h>

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

/* ── memory-backed block I/O ─────────────────────────────────────── */

#define MAX_SECTORS 256

struct mem_disk {
	uint8_t blocks[MAX_SECTORS][4096];
	uint8_t valid[MAX_SECTORS];
};

static int mem_read(void *ctx, uint64_t s, uint8_t *out) {
	struct mem_disk *d = ctx;
	if (s >= MAX_SECTORS || !d->valid[s]) return -1;
	memcpy(out, d->blocks[s], 4096);
	return 0;
}
static int mem_write(void *ctx, uint64_t s, const uint8_t *buf) {
	struct mem_disk *d = ctx;
	if (s >= MAX_SECTORS) return -1;
	memcpy(d->blocks[s], buf, 4096);
	d->valid[s] = 1;
	return 0;
}
static int mem_alloc(void *ctx, uint64_t n, uint64_t *o) { (void)ctx;(void)n;(void)o; return -1; }
static int mem_free (void *ctx, uint64_t s, uint64_t n)  { (void)ctx;(void)s;(void)n; return 0; }

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

/* ── replay capture ──────────────────────────────────────────────── */

struct captured {
	uint32_t  type;
	uint32_t  body_len;
	uint8_t  *body;
};

struct cap_ctx {
	struct captured *recs;
	size_t cap;
	size_t count;
};

static int
cap_cb(void *ctx, const tessera_record_header_t *h, const uint8_t *body)
{
	struct cap_ctx *cc = ctx;
	if (cc->count == cc->cap) {
		cc->cap = cc->cap ? cc->cap * 2 : 16;
		cc->recs = realloc(cc->recs, cc->cap * sizeof *cc->recs);
	}
	struct captured *r = &cc->recs[cc->count++];
	r->type = h->record_type;
	r->body_len = h->body_length;
	r->body = h->body_length ? malloc(h->body_length) : NULL;
	if (r->body) memcpy(r->body, body, h->body_length);
	return 0;
}

static void
cap_free(struct cap_ctx *cc)
{
	for (size_t i = 0; i < cc->count; i++) free(cc->recs[i].body);
	free(cc->recs);
	memset(cc, 0, sizeof *cc);
}

/* ── tests ───────────────────────────────────────────────────────── */

static void
test_empty(void)
{
	struct mem_disk *d = calloc(1, sizeof *d);
	tessera_block_io_t io = mk_io(d);
	const uint64_t START = 4, LEN = 32;
	CHECK(tessera_journal_format(&io, START, LEN) == TESSERA_OK);
	tessera_journal_t *j = tessera_journal_open(&io, START, LEN);
	CHECK(j != NULL);

	struct cap_ctx cc = {0};
	CHECK(tessera_journal_replay(j, cap_cb, &cc) == TESSERA_OK);
	CHECK(cc.count == 0);
	cap_free(&cc);
	tessera_journal_close(j);
	free(d);
}

static void
test_one_txn(void)
{
	struct mem_disk *d = calloc(1, sizeof *d);
	tessera_block_io_t io = mk_io(d);
	const uint64_t START = 4, LEN = 32;
	tessera_journal_format(&io, START, LEN);
	tessera_journal_t *j = tessera_journal_open(&io, START, LEN);

	uint64_t tx;
	CHECK(tessera_journal_tx_begin(j, &tx, "begin-1xxxxxxxxx") == TESSERA_OK);

	uint8_t body1[64], body2[200];
	memset(body1, 0xa1, sizeof body1);
	memset(body2, 0xb2, sizeof body2);
	CHECK(tessera_journal_append(j, tx, TESSERA_INODE_WRITE,
	    body1, sizeof body1) == TESSERA_OK);
	CHECK(tessera_journal_append(j, tx, TESSERA_MANIFEST_REPOINT,
	    body2, sizeof body2) == TESSERA_OK);
	CHECK(tessera_journal_tx_commit(j, tx) == TESSERA_OK);

	tessera_journal_close(j);
	j = tessera_journal_open(&io, START, LEN);

	struct cap_ctx cc = {0};
	CHECK(tessera_journal_replay(j, cap_cb, &cc) == TESSERA_OK);
	CHECK(cc.count == 2);
	CHECK(cc.recs[0].type == TESSERA_INODE_WRITE);
	CHECK(cc.recs[0].body_len == sizeof body1);
	CHECK(memcmp(cc.recs[0].body, body1, sizeof body1) == 0);
	CHECK(cc.recs[1].type == TESSERA_MANIFEST_REPOINT);
	CHECK(cc.recs[1].body_len == sizeof body2);
	CHECK(memcmp(cc.recs[1].body, body2, sizeof body2) == 0);

	cap_free(&cc);
	tessera_journal_close(j);
	free(d);
}

static void
test_aborted_then_committed(void)
{
	struct mem_disk *d = calloc(1, sizeof *d);
	tessera_block_io_t io = mk_io(d);
	const uint64_t START = 4, LEN = 32;
	tessera_journal_format(&io, START, LEN);
	tessera_journal_t *j = tessera_journal_open(&io, START, LEN);

	uint64_t tx1, tx2;
	tessera_journal_tx_begin(j, &tx1, "abort-meeeeeeeee");
	uint8_t body_drop[40]; memset(body_drop, 0xcc, sizeof body_drop);
	tessera_journal_append(j, tx1, TESSERA_INODE_WRITE, body_drop,
	    sizeof body_drop);
	tessera_journal_tx_abort(j, tx1, 7);

	tessera_journal_tx_begin(j, &tx2, "commit-meeeeeeee");
	uint8_t body_keep[24]; memset(body_keep, 0x99, sizeof body_keep);
	tessera_journal_append(j, tx2, TESSERA_PACK_PUBLISH, body_keep,
	    sizeof body_keep);
	tessera_journal_tx_commit(j, tx2);

	struct cap_ctx cc = {0};
	tessera_journal_replay(j, cap_cb, &cc);
	CHECK(cc.count == 1);
	CHECK(cc.recs[0].type == TESSERA_PACK_PUBLISH);
	CHECK(cc.recs[0].body_len == sizeof body_keep);
	CHECK(memcmp(cc.recs[0].body, body_keep, sizeof body_keep) == 0);

	cap_free(&cc);
	tessera_journal_close(j);
	free(d);
}

static void
test_torn_then_committed(void)
{
	/* Append BEGIN + records but no COMMIT (simulate crash), then
	 * append a fully-committed second txn afterward. Replay must
	 * silently drop the torn one and process only the committed. */
	struct mem_disk *d = calloc(1, sizeof *d);
	tessera_block_io_t io = mk_io(d);
	const uint64_t START = 4, LEN = 32;
	tessera_journal_format(&io, START, LEN);
	tessera_journal_t *j = tessera_journal_open(&io, START, LEN);

	uint64_t tx1, tx2;
	tessera_journal_tx_begin(j, &tx1, "torn-omittedccmt");
	uint8_t body_drop[80]; memset(body_drop, 0xdd, sizeof body_drop);
	tessera_journal_append(j, tx1, TESSERA_INODE_WRITE, body_drop,
	    sizeof body_drop);
	/* deliberately no commit / abort */

	tessera_journal_tx_begin(j, &tx2, "good-tx-2xxxxxxx");
	uint8_t body_keep[16]; memset(body_keep, 0xee, sizeof body_keep);
	tessera_journal_append(j, tx2, TESSERA_DIR_INSERT, body_keep,
	    sizeof body_keep);
	tessera_journal_tx_commit(j, tx2);

	struct cap_ctx cc = {0};
	tessera_journal_replay(j, cap_cb, &cc);
	CHECK(cc.count == 1);
	CHECK(cc.recs[0].type == TESSERA_DIR_INSERT);
	CHECK(memcmp(cc.recs[0].body, body_keep, sizeof body_keep) == 0);

	cap_free(&cc);
	tessera_journal_close(j);
	free(d);
}

static void
test_multiblock_body(void)
{
	struct mem_disk *d = calloc(1, sizeof *d);
	tessera_block_io_t io = mk_io(d);
	const uint64_t START = 4, LEN = 64;
	tessera_journal_format(&io, START, LEN);
	tessera_journal_t *j = tessera_journal_open(&io, START, LEN);

	uint64_t tx;
	tessera_journal_tx_begin(j, &tx, "big-body-test---");
	const uint32_t LEN_BIG = 12000;
	uint8_t *big = malloc(LEN_BIG);
	for (uint32_t i = 0; i < LEN_BIG; i++) big[i] = (uint8_t)(i * 17);
	CHECK(tessera_journal_append(j, tx, TESSERA_INODE_WRITE,
	    big, LEN_BIG) == TESSERA_OK);
	tessera_journal_tx_commit(j, tx);

	struct cap_ctx cc = {0};
	tessera_journal_replay(j, cap_cb, &cc);
	CHECK(cc.count == 1);
	CHECK(cc.recs[0].body_len == LEN_BIG);
	CHECK(memcmp(cc.recs[0].body, big, LEN_BIG) == 0);

	free(big);
	cap_free(&cc);
	tessera_journal_close(j);
	free(d);
}

/* ── #126: replay must TERMINATE on a corrupt journal ───────────────
 *
 * Found by fuzzing (fuzz/fuzz_journal.c). block_count is read off disk and was
 * used unchecked to advance the replay cursor, so a torn write inside a record
 * header could point the cursor at an unrelated block and form a RING among
 * the records — a ring that never reaches head_block. Replay then span
 * forever, at mount, in the kernel: an unbootable volume from one flipped
 * byte. The reproducer walked 3->4->...->17->3 with head_block=19 outside the
 * cycle.
 *
 * Two guards, tested separately, because the second must hold even if the
 * first is ever incomplete:
 *   A. read_record rejects a record whose block_count disagrees with
 *      body_length (they are not independent — block_count is derived).
 *   B. replay bounds its own walk by the journal length, so termination does
 *      not depend on validation being exhaustive.
 */
static void
corrupt_record_field(struct mem_disk *d, uint64_t sector, size_t off,
                     uint32_t value)
{
	uint8_t *r = d->blocks[sector];
	memcpy(r + off, &value, 4);
	/* Re-stamp the header CRC: we are modelling a torn write that lands
	 * INSIDE a header, which after recomputation is structurally valid and
	 * semantically absurd. Leaving the CRC stale would only test the CRC. */
	const size_t crc_off = offsetof(tessera_record_header_t, crc32_header);
	uint32_t c = tessera_crc32(r, crc_off);
	memcpy(r + crc_off, &c, 4);
}

static void
test_corrupt_block_count_terminates(void)
{
	struct mem_disk *d = calloc(1, sizeof *d);
	tessera_block_io_t io = mk_io(d);
	const uint64_t START = 4, LEN = 32;
	tessera_journal_format(&io, START, LEN);
	tessera_journal_t *j = tessera_journal_open(&io, START, LEN);
	CHECK(j != NULL);

	/* A few committed transactions, so there is a chain to corrupt. */
	for (int t = 0; t < 6; t++) {
		uint64_t tx = 0;
		CHECK(tessera_journal_tx_begin(j, &tx, "test\0\0\0\0\0\0\0\0\0\0\0")
		    == TESSERA_OK);
		uint8_t body[16];
		memset(body, 0xA5 ^ t, sizeof body);
		CHECK(tessera_journal_append(j, tx, TESSERA_INODE_WRITE,
		    body, sizeof body) == TESSERA_OK);
		CHECK(tessera_journal_tx_commit(j, tx) == TESSERA_OK);
	}
	tessera_journal_close(j);

	/* (A) A wild block_count on one record. Before the fix this both cost
	 * 12.5M iterations to "advance" and landed the cursor mid-chain. */
	corrupt_record_field(d, START + 5,
	    offsetof(tessera_record_header_t, block_count), 12582913u);

	j = tessera_journal_open(&io, START, LEN);
	CHECK(j != NULL);
	struct cap_ctx cc = {0};
	int rc = tessera_journal_replay(j, cap_cb, &cc);
	/* The ONLY assertion that matters is that we got here at all — before
	 * the fix this call never returned. Whether it reports ECORRUPT or
	 * simply stops early is a policy detail; not hanging is not. */
	CHECK(rc == TESSERA_OK || rc == TESSERA_ECORRUPT);
	cap_free(&cc);
	tessera_journal_close(j);

	/* (B) block_count = 0, the degenerate case: the cursor advanced by
	 * nothing, so replay re-read ONE record forever. */
	corrupt_record_field(d, START + 5,
	    offsetof(tessera_record_header_t, block_count), 0u);
	j = tessera_journal_open(&io, START, LEN);
	CHECK(j != NULL);
	struct cap_ctx cc2 = {0};
	rc = tessera_journal_replay(j, cap_cb, &cc2);
	CHECK(rc == TESSERA_OK || rc == TESSERA_ECORRUPT);
	cap_free(&cc2);
	tessera_journal_close(j);

	free(d);
}

int
main(void)
{
	printf("test_journal: format / append / replay with abort+torn fixups\n");
	test_empty();
	test_one_txn();
	test_aborted_then_committed();
	test_torn_then_committed();
	test_multiblock_body();
	test_corrupt_block_count_terminates();
	if (failures > 0) {
		fprintf(stderr, "%d failure(s)\n", failures);
		return 1;
	}
	printf("ok\n");
	return 0;
}
