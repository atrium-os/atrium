/*
 * tessera-core: circular-log journal (per tessera-fs §4).
 *
 * Layout:
 *   block 0 of [journal_start, +journal_length)
 *       tessera_journal_header_t (head_seq, tail_seq, head_block,
 *       tail_block, CRC).
 *   blocks 1 .. journal_length-1
 *       circular log of records. Each record consumes
 *       ceil((32 + body_len) / 4096) sectors. Per-record header
 *       carries a CRC over the body and a CRC over the header.
 *
 * Append protocol (one transaction at a time per handle):
 *   tx_begin  → write TX_BEGIN record (16-byte reason tag in body)
 *   append    → write a typed record carrying body+CRC
 *   tx_commit → write TX_COMMIT record; this is the durability barrier
 *   tx_abort  → write TX_ABORT record (advisory; replay drops the txn
 *               regardless, since no TX_COMMIT will be seen)
 *
 * Replay walks from tail_block forward. Records belong to a
 * transaction (delimited by TX_BEGIN..TX_COMMIT). The per-txn buffer
 * is built up as we walk; when TX_COMMIT is seen, every buffered
 * record (excluding the BEGIN/COMMIT markers) is fed to the caller's
 * replay callback. If TX_ABORT or a torn / bad-CRC record is seen, the
 * buffered txn is discarded.
 *
 * Phase 1 keeps the head/tail markers in memory and persists them
 * after every record. Advancing tail (after the application has
 * applied all replayed records) is left to Phase 2 — for now the
 * journal is treated as a write-only log that the caller checkpoints
 * by reformatting once its content has been folded into the snapshot.
 *
 * Journal-full is returned as TESSERA_ENOSPC; the application is
 * responsible for sizing.
 */

#include "tessera/journal.h"
#include "tessera/codec.h"
#include "tessera/crc.h"
#include "tessera/error.h"
#include "tessera/format.h"
#include "tessera_compat.h"

#define BLK 4096u
#define HDR 64u                /* sizeof(tessera_record_header_t) */

struct tessera_journal {
	tessera_block_io_t io;
	uint64_t  start;
	uint64_t  length;          /* in sectors, including header */
	uint64_t  head_seq;
	uint64_t  tail_seq;
	uint64_t  head_block;      /* relative to start; 1..length-1 */
	uint64_t  tail_block;
	uint64_t  next_tx_id;
};

/* ── header persistence ──────────────────────────────────────────── */

static int
write_journal_header(tessera_journal_t *j)
{
	tessera_journal_header_t h;
	memset(&h, 0, sizeof h);
	memcpy(h.magic, TESSERA_MAGIC_JOURNAL, 8);
	h.version    = 1;
	h.head_seq   = j->head_seq;
	h.tail_seq   = j->tail_seq;
	h.head_block = j->head_block;
	h.tail_block = j->tail_block;
	/* Heap-alloc'd to avoid 4 KiB on the kernel stack (FreeBSD aarch64
	 * KSTACK_PAGES=4 → 16 KiB; mountfs frame chain leaves ~6 KiB
	 * usable here). */
	uint8_t *buf = tessera_zalloc(BLK);
	if (buf == NULL) return TESSERA_ENOMEM;
	int r = tessera_encode_journal_header(&h, buf);
	if (r != TESSERA_OK) { tessera_free(buf); return r; }
	if (j->io.write_block(j->io.ctx, j->start, buf) != 0) {
		tessera_free(buf);
		return TESSERA_EIO;
	}
	tessera_free(buf);
	return TESSERA_OK;
}

static int
read_journal_header(const tessera_block_io_t *io, uint64_t start,
                    tessera_journal_header_t *out)
{
	uint8_t *buf = tessera_zalloc(BLK);
	if (buf == NULL) return TESSERA_ENOMEM;
	int rc = io->read_block(io->ctx, start, buf);
	if (rc != 0) {
		tessera_free(buf);
		return TESSERA_EIO;
	}
	int r = tessera_decode_journal_header(buf, out);
	tessera_free(buf);
	return r;
}

/* ── format / open / close ───────────────────────────────────────── */

int
tessera_journal_format(const tessera_block_io_t *io, uint64_t start,
                       uint64_t length)
{
	if (io == NULL || length < 4) return TESSERA_EINVAL;
	/* sizeof(tessera_journal_header_t) == 4096; never on the stack
	 * in kernel mode — it overflows the 16 KiB FreeBSD aarch64
	 * kstack right at the prologue and the resulting fault is
	 * unrecoverable enough to hang the VM silently. */
	tessera_journal_header_t *h = tessera_zalloc(sizeof *h);
	if (h == NULL) return TESSERA_ENOMEM;
	memcpy(h->magic, TESSERA_MAGIC_JOURNAL, 8);
	h->version    = 1;
	h->head_seq   = 1;
	h->tail_seq   = 1;
	h->head_block = 1;
	h->tail_block = 1;
	uint8_t *buf = tessera_zalloc(BLK);
	if (buf == NULL) { tessera_free(h); return TESSERA_ENOMEM; }
	int r = tessera_encode_journal_header(h, buf);
	tessera_free(h);
	if (r != TESSERA_OK) { tessera_free(buf); return r; }
	if (io->write_block(io->ctx, start, buf) != 0) {
		tessera_free(buf);
		return TESSERA_EIO;
	}
	tessera_free(buf);
	return TESSERA_OK;
}

tessera_journal_t *
tessera_journal_open(const tessera_block_io_t *io, uint64_t start,
                     uint64_t length)
{
	if (io == NULL || length < 4) return NULL;
	tessera_journal_header_t *h = tessera_zalloc(sizeof *h);
	if (h == NULL) return NULL;
	if (read_journal_header(io, start, h) != TESSERA_OK) {
		tessera_free(h);
		return NULL;
	}
	tessera_journal_t *j = tessera_zalloc(sizeof *j);
	if (j == NULL) { tessera_free(h); return NULL; }
	j->io         = *io;
	j->start      = start;
	j->length     = length;
	j->head_seq   = h->head_seq;
	j->tail_seq   = h->tail_seq;
	j->head_block = h->head_block;
	j->tail_block = h->tail_block;
	j->next_tx_id = 1;
	tessera_free(h);
	return j;
}

void
tessera_journal_close(tessera_journal_t *j)
{
	tessera_free(j);
}

/* ── append helpers ──────────────────────────────────────────────── */

static uint32_t
sectors_for(uint32_t body_len)
{
	return (HDR + body_len + (BLK - 1u)) / BLK;
}

static uint64_t
free_sectors(const tessera_journal_t *j)
{
	const uint64_t cap = j->length - 1;
	uint64_t used;
	if (j->head_block >= j->tail_block)
		used = j->head_block - j->tail_block;
	else
		used = cap - (j->tail_block - j->head_block);
	if (used + 1 > cap) return 0;
	return cap - used - 1;
}

static int
write_record(tessera_journal_t *j, tessera_record_type_t type,
             const void *body, uint32_t body_len, uint64_t seq)
{
	const uint32_t need = sectors_for(body_len);
	if (free_sectors(j) < need) return TESSERA_ENOSPC;

	tessera_record_header_t rh;
	memset(&rh, 0, sizeof rh);
	memcpy(rh.magic, TESSERA_MAGIC_TXR, 4);
	rh.record_type = (uint32_t)type;
	rh.sequence    = seq;
	rh.body_length = body_len;
	rh.block_count = need;
	rh.crc32_body  = (body_len > 0)
	    ? tessera_crc32(body, body_len) : 0u;

	uint8_t *blk = tessera_zalloc(BLK);
	if (blk == NULL) return TESSERA_ENOMEM;
	int r = tessera_encode_record_header(&rh, blk);
	if (r != TESSERA_OK) { tessera_free(blk); return r; }

	const uint32_t in_first = (body_len > BLK - HDR) ? (BLK - HDR) : body_len;
	if (in_first > 0)
		memcpy(blk + HDR, body, in_first);

	uint64_t cap = j->length - 1;
	uint64_t b = j->head_block;
	if (j->io.write_block(j->io.ctx, j->start + b, blk) != 0) {
		tessera_free(blk);
		return TESSERA_EIO;
	}
	tessera_free(blk);
	uint32_t remaining = body_len - in_first;
	const uint8_t *src = (const uint8_t *)body + in_first;
	if (remaining > 0) {
		uint8_t *cont = tessera_zalloc(BLK);
		if (cont == NULL) return TESSERA_ENOMEM;
		for (uint32_t k = 1; k < need; k++) {
			b++;
			if (b > cap) b = 1;
			memset(cont, 0, BLK);
			uint32_t take = remaining > BLK ? BLK : remaining;
			if (take > 0) memcpy(cont, src, take);
			if (j->io.write_block(j->io.ctx, j->start + b, cont) != 0) {
				tessera_free(cont);
				return TESSERA_EIO;
			}
			src += take;
			remaining -= take;
		}
		tessera_free(cont);
	}
	b++;
	if (b > cap) b = 1;
	j->head_block = b;
	j->head_seq   = seq + 1;
	return write_journal_header(j);
}

/* ── tx ──────────────────────────────────────────────────────────── */

int
tessera_journal_tx_begin(tessera_journal_t *j, uint64_t *out_tx_id,
                         const char reason_tag[16])
{
	if (j == NULL || out_tx_id == NULL) return TESSERA_EINVAL;
	uint64_t tx = j->next_tx_id++;
	uint8_t body[16];
	memset(body, 0, sizeof body);
	if (reason_tag) memcpy(body, reason_tag, 16);
	int r = write_record(j, TESSERA_TX_BEGIN, body, sizeof body,
	    j->head_seq);
	if (r != TESSERA_OK) { j->next_tx_id--; return r; }
	*out_tx_id = tx;
	return TESSERA_OK;
}

int
tessera_journal_append(tessera_journal_t *j, uint64_t tx_id,
                       tessera_record_type_t type,
                       const void *body, uint32_t body_len)
{
	if (j == NULL) return TESSERA_EINVAL;
	(void)tx_id;
	if (type == TESSERA_TX_BEGIN || type == TESSERA_TX_COMMIT ||
	    type == TESSERA_TX_ABORT)
		return TESSERA_EINVAL;
	return write_record(j, type, body, body_len, j->head_seq);
}

int
tessera_journal_tx_commit(tessera_journal_t *j, uint64_t tx_id)
{
	if (j == NULL) return TESSERA_EINVAL;
	(void)tx_id;
	return write_record(j, TESSERA_TX_COMMIT, NULL, 0, j->head_seq);
}

int
tessera_journal_tx_abort(tessera_journal_t *j, uint64_t tx_id,
                         uint32_t reason_code)
{
	if (j == NULL) return TESSERA_EINVAL;
	(void)tx_id;
	uint8_t body[4];
	memcpy(body, &reason_code, 4);
	return write_record(j, TESSERA_TX_ABORT, body, sizeof body,
	    j->head_seq);
}

int
tessera_journal_checkpoint(tessera_journal_t *j)
{
	if (j == NULL) return TESSERA_EINVAL;
	j->head_seq   = 1;
	j->tail_seq   = 1;
	j->head_block = 1;
	j->tail_block = 1;
	j->next_tx_id = 1;
	return write_journal_header(j);
}

/* ── replay ──────────────────────────────────────────────────────── */

struct buffered_rec {
	tessera_record_header_t hdr;
	uint8_t                *body;
};

static int
read_record(tessera_journal_t *j, uint64_t *block,
            tessera_record_header_t *out_hdr, uint8_t **out_body)
{
	const uint64_t cap = j->length - 1;
	uint8_t *blk = tessera_zalloc(BLK);
	if (blk == NULL) return TESSERA_ENOMEM;
	if (j->io.read_block(j->io.ctx, j->start + *block, blk) != 0) {
		tessera_free(blk);
		return TESSERA_EIO;
	}

	int r = tessera_decode_record_header(blk, out_hdr);
	if (r != TESSERA_OK) { tessera_free(blk); return r; }

	const uint32_t bl = out_hdr->body_length;
	uint8_t *body = NULL;
	if (bl > 0) {
		body = tessera_malloc(bl);
		if (body == NULL) { tessera_free(blk); return TESSERA_ENOMEM; }
		const uint32_t in_first =
		    (bl > BLK - HDR) ? (BLK - HDR) : bl;
		memcpy(body, blk + HDR, in_first);
		tessera_free(blk); blk = NULL;
		uint32_t remaining = bl - in_first;
		uint64_t b = *block;
		if (remaining > 0) {
			uint8_t *cont = tessera_zalloc(BLK);
			if (cont == NULL) { tessera_free(body); return TESSERA_ENOMEM; }
			for (uint32_t k = 1; k < out_hdr->block_count; k++) {
				b++;
				if (b > cap) b = 1;
				if (j->io.read_block(j->io.ctx, j->start + b, cont)
				    != 0) {
					tessera_free(body);
					tessera_free(cont);
					return TESSERA_EIO;
				}
				uint32_t take = remaining > BLK ? BLK : remaining;
				memcpy(body + bl - remaining, cont, take);
				remaining -= take;
			}
			tessera_free(cont);
		}
	} else {
		tessera_free(blk);
	}
	uint32_t body_crc = (bl > 0) ? tessera_crc32(body, bl) : 0u;
	if (out_hdr->crc32_body != body_crc) {
		tessera_free(body);
		return TESSERA_EBADCRC;
	}

	uint64_t b = *block;
	for (uint32_t k = 0; k < out_hdr->block_count; k++) {
		b++;
		if (b > cap) b = 1;
	}
	*block = b;
	*out_body = body;
	return TESSERA_OK;
}

int
tessera_journal_replay(tessera_journal_t *j,
                       tessera_replay_cb_t cb, void *ctx)
{
	if (j == NULL || cb == NULL) return TESSERA_EINVAL;

	struct buffered_rec *buf = NULL;
	size_t buf_count = 0, buf_cap = 0;
	uint64_t cur = j->tail_block;
	int in_tx = 0;
	int rc = TESSERA_OK;

	while (cur != j->head_block) {
		tessera_record_header_t hdr;
		uint8_t *body = NULL;
		int r = read_record(j, &cur, &hdr, &body);
		if (r != TESSERA_OK) {
			break;
		}

		if (hdr.record_type == (uint32_t)TESSERA_TX_BEGIN) {
			for (size_t i = 0; i < buf_count; i++)
				tessera_free(buf[i].body);
			buf_count = 0;
			tessera_free(body);
			in_tx = 1;
			continue;
		}

		if (hdr.record_type == (uint32_t)TESSERA_TX_COMMIT) {
			if (in_tx) {
				for (size_t i = 0; i < buf_count; i++) {
					int cr = cb(ctx, &buf[i].hdr,
					    buf[i].body);
					tessera_free(buf[i].body);
					if (cr != 0) {
						rc = cr;
						/* free the rest */
						for (size_t k = i + 1;
						     k < buf_count; k++)
							tessera_free(buf[k].body);
						buf_count = 0;
						goto cleanup;
					}
				}
				buf_count = 0;
			}
			tessera_free(body);
			in_tx = 0;
			continue;
		}

		if (hdr.record_type == (uint32_t)TESSERA_TX_ABORT) {
			for (size_t i = 0; i < buf_count; i++)
				tessera_free(buf[i].body);
			buf_count = 0;
			tessera_free(body);
			in_tx = 0;
			continue;
		}

		if (in_tx) {
			if (buf_count == buf_cap) {
				size_t nc = buf_cap ? buf_cap * 2 : 16;
				struct buffered_rec *nb =
				    tessera_realloc(buf, nc * sizeof *nb);
				if (nb == NULL) {
					tessera_free(body);
					rc = TESSERA_ENOMEM;
					goto cleanup;
				}
				buf = nb; buf_cap = nc;
			}
			buf[buf_count].hdr = hdr;
			buf[buf_count].body = body;
			buf_count++;
		} else {
			tessera_free(body);
		}
	}
cleanup:
	for (size_t i = 0; i < buf_count; i++) tessera_free(buf[i].body);
	tessera_free(buf);
	return rc;
}
