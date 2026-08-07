/* Fuzz journal replay — the crash-recovery path.
 *
 * WHY THIS TARGET IS THE MOST CONSEQUENTIAL ONE. Every other parser here reads
 * a structure that was written by a healthy system. The journal is the one
 * that is READ PRECISELY WHEN THE SYSTEM WAS NOT HEALTHY: replay runs at mount
 * after a crash, over exactly the torn, half-written, partially-ordered state a
 * power cut leaves behind. And unlike the others it does not merely return
 * data — the records it accepts are replayed as WRITES. A replay that believes
 * a damaged record does not return a wrong answer, it commits one.
 *
 * SHAPE: BUILD A REAL JOURNAL, THEN DAMAGE IT. A purely random image would be
 * useless here — every record carries a header CRC and a body CRC, so blind
 * bytes fail at the first record and replay stops having tested nothing (the
 * #124 cov:2 failure mode). Instead the harness:
 *
 *   1. formats a journal in a RAM image and writes REAL transactions through
 *      the real writer API (tx_begin / append / commit / abort / checkpoint),
 *      with the record types, body sizes and transaction shapes all chosen by
 *      the fuzzer;
 *   2. applies fuzzer-chosen byte mutations to the resulting image — this is
 *      the torn write, the flipped bit, the sector that never landed;
 *   3. RE-STAMPS the header CRC of every sector still carrying a record magic.
 *
 * Step 3 is the subtle one and it is deliberate. Without it, any mutation that
 * touches a record header is caught by the header CRC and the record is simply
 * rejected — which tests the CRC (already covered by test_codec) and nothing
 * else. Re-stamping models the case that actually matters: a record that is
 * STRUCTURALLY intact but SEMANTICALLY wrong — a plausible header with a
 * nonsense block_count, body_length or record_type, which is what a torn write
 * landing inside a header looks like once the CRC is recomputed over it. Body
 * CRCs are deliberately NOT re-stamped, so body damage is still caught by
 * crc32_body exactly as the design intends.
 *
 * The journal header itself is re-stamped for the same reason: head_block and
 * tail_block are the loop bounds of replay, and they are read off disk.
 */
#include <stdint.h>
#include <stddef.h>
#include <stdlib.h>
#include <string.h>

#include "tessera/error.h"
#include "tessera/format.h"
#include "tessera/journal.h"
#include "tessera/crc.h"

#define SECT      TESSERA_SECTOR_SIZE
#define SECTORS   64u                 /* journal image, incl. header block */
#define MAX_BODY  (SECT * 3u)         /* spans the multi-block body path */
#define MAX_OPS   64u
#define MAX_MUTS  16u

struct img { uint8_t *base; uint64_t sectors; };

static int
img_read(void *ctx, uint64_t sector, uint8_t *out)
{
	struct img *m = ctx;
	if (sector >= m->sectors) return -1;
	memcpy(out, m->base + sector * SECT, SECT);
	return 0;
}
static int
img_write(void *ctx, uint64_t sector, const uint8_t *in)
{
	struct img *m = ctx;
	if (sector >= m->sectors) return -1;
	memcpy(m->base + sector * SECT, in, SECT);
	return 0;
}
static int img_alloc(void *c, uint64_t n, uint64_t *o)
{ (void)c; (void)n; (void)o; return -1; }
static int img_free(void *c, uint64_t s, uint64_t n)
{ (void)c; (void)s; (void)n; return -1; }

/* A cursor over the fuzz input; returns 0 once exhausted so the op script
 * simply ends rather than reading past the buffer. */
struct bits { const uint8_t *p; size_t len, off; };
static uint8_t  b8 (struct bits *b) { return b->off < b->len ? b->p[b->off++] : 0; }
static uint32_t b32(struct bits *b)
{ uint32_t v = 0; for (int i = 0; i < 4; i++) v = (v << 8) | b8(b); return v; }

/* Replay callback: TOUCH the body. A replay handing back an out-of-range
 * pointer/length pair is only a defect once someone reads it, and the real
 * callbacks (inode write, dir insert, root update) all do. */
static int
replay_cb(void *ctx, const tessera_record_header_t *hdr, const uint8_t *body)
{
	unsigned long *n = ctx;
	volatile uint8_t sink = 0;
	if (body != NULL)
		for (uint32_t i = 0; i < hdr->body_length; i++) sink ^= body[i];
	(void)sink;
	/* Bound the work one input can cause. A journal can legitimately
	 * replay many records; without a cap a single input could dominate the
	 * whole fuzzing budget. Returning nonzero makes replay unwind, which
	 * is itself a path worth covering. */
	return (++*n > 4096) ? -1 : 0;
}

int
LLVMFuzzerTestOneInput(const uint8_t *data, size_t len)
{
	if (len < 8) return 0;

	uint8_t *img = calloc(SECTORS, SECT);
	if (img == NULL) return 0;
	struct img m = { img, SECTORS };
	tessera_block_io_t io = { img_read, img_write, img_alloc, img_free, &m };
	struct bits b = { data, len, 0 };

	/* ── 1. build a real journal ─────────────────────────────────────── */
	if (tessera_journal_format(&io, 0, SECTORS) != TESSERA_OK) {
		free(img); return 0;
	}
	tessera_journal_t *j = tessera_journal_open(&io, 0, SECTORS);
	if (j == NULL) { free(img); return 0; }

	uint8_t *body = malloc(MAX_BODY);
	if (body == NULL) { tessera_journal_close(j); free(img); return 0; }

	uint64_t tx = 0;
	int open_tx = 0;
	for (unsigned op = 0; op < MAX_OPS && b.off < b.len; op++) {
		switch (b8(&b) % 5u) {
		case 0:
			if (!open_tx &&
			    tessera_journal_tx_begin(j, &tx, "fuzz\0\0\0\0\0\0\0\0\0\0\0")
			    == TESSERA_OK)
				open_tx = 1;
			break;
		case 1: {
			uint32_t bl = b32(&b) % (MAX_BODY + 1u);
			/* Body bytes come from the input too, so dedup and CRC
			 * paths see varied content rather than a constant. */
			for (uint32_t i = 0; i < bl; i++) body[i] = b8(&b);
			uint32_t ty = (b8(&b) % 12u) + 4u;   /* non-control types */
			(void)tessera_journal_append(j, tx,
			    (tessera_record_type_t)ty, body, bl);
			break;
		}
		case 2:
			if (open_tx) { (void)tessera_journal_tx_commit(j, tx); open_tx = 0; }
			break;
		case 3:
			if (open_tx) { (void)tessera_journal_tx_abort(j, tx, b32(&b)); open_tx = 0; }
			break;
		default:
			(void)tessera_journal_checkpoint(j);
			break;
		}
	}
	tessera_journal_close(j);
	free(body);

	/* ── 2. damage it ────────────────────────────────────────────────── */
	unsigned nmut = b8(&b) % (MAX_MUTS + 1u);
	for (unsigned i = 0; i < nmut; i++) {
		uint32_t off = b32(&b) % (SECTORS * SECT);
		img[off] = b8(&b);
	}

	/* ── 3. re-stamp structural CRCs (see header comment) ────────────── */
	{
		const size_t jcrc = offsetof(tessera_journal_header_t, crc32);
		if (memcmp(img, TESSERA_MAGIC_JOURNAL, 8) == 0) {
			uint32_t c = tessera_crc32(img, jcrc);
			memcpy(img + jcrc, &c, 4);
		}
		const size_t rcrc = offsetof(tessera_record_header_t, crc32_header);
		for (uint64_t s = 1; s < SECTORS; s++) {
			uint8_t *r = img + s * SECT;
			if (memcmp(r, TESSERA_MAGIC_TXR, 4) != 0) continue;
			uint32_t c = tessera_crc32(r, rcrc);
			memcpy(r + rcrc, &c, 4);
		}
	}

	/* ── 4. replay the wreckage ──────────────────────────────────────── */
	tessera_journal_t *j2 = tessera_journal_open(&io, 0, SECTORS);
	if (j2 != NULL) {
		unsigned long n = 0;
		(void)tessera_journal_replay(j2, replay_cb, &n);
		/* NOTE: tessera_journal_peek_pos() is #ifdef _KERNEL, so the
		 * head/tail cursor cannot be inspected from a host build. */
		tessera_journal_close(j2);
	}
	free(img);
	return 0;
}
