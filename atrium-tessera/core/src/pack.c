/*
 * tessera-core: pack-file builder + reader.
 *
 * Layout (per tessera-fs §5):
 *
 *   sector 0                   pack header (tessera_pack_header_t, 4096 B)
 *   sector 1 .. 1+I-1          sorted blob index (blob_count × 48 B,
 *                              zero-padded to a multiple of 4096)
 *   sector 1+I .. 1+I+B-1      bloom filter (bloom_bytes, zero-padded
 *                              to a multiple of 4096)
 *   data_offset .. +data_len   blob data area, each blob =
 *                              tessera_blob_descriptor_t (16 B) + bytes,
 *                              64-byte aligned to keep cache-line-friendly
 *                              random access.
 *   final sector               pack footer (4096 B), with crc32_pack
 *                              covering bytes [data_offset, data_offset +
 *                              data_length).
 *
 * Writer is one-shot: collect blobs, sort by hash, lay out, hash-stamp
 * the data CRC, return contiguous bytes. Reader is read-only and
 * binary-searches the sorted index. Bloom filter is computed on
 * publish from blob hashes (Kirsch–Mitzenmacher h1 + i·h2 from the
 * blob's SHA-256).
 *
 * Sizing: we use ~10 bits/blob as the bloom budget — the well-studied
 * sweet spot for ~1 % false-positive rate at ~7 hashes (FPR formula:
 * (1 - e^(-k·n/m))^k). bloom_bytes is rounded up to a multiple of 4096
 * for sector-aligned I/O. For very small packs we floor the bloom at
 * 64 bytes so the formula stays sane.
 */

#include "tessera/pack.h"
#include "tessera/codec.h"
#include "tessera/crc.h"
#include "tessera/error.h"
#include "tessera/format.h"
#include "tessera_compat.h"

#define BLOB_DATA_ALIGN  64u
#define BLOOM_BITS_PER_BLOB 10u
#define BLOOM_MIN_BYTES  64u

/* ── helpers ─────────────────────────────────────────────────────── */

static size_t
round_up(size_t x, size_t a)
{
	return (x + a - 1) / a * a;
}

/* Derive two independent 64-bit hashes from a SHA-256 for Kirsch–
 * Mitzenmacher: just split the first 16 bytes into two LE u64s. */
static void
bloom_h1_h2(const uint8_t hash[32], uint64_t *h1, uint64_t *h2)
{
	memcpy(h1, hash + 0, 8);
	memcpy(h2, hash + 8, 8);
	if (*h2 == 0) *h2 = 1;          /* avoid pathological cycle */
}

static void
bloom_set(uint8_t *bloom, size_t bloom_bits, uint32_t k_hashes,
          const uint8_t hash[32])
{
	uint64_t h1, h2;
	bloom_h1_h2(hash, &h1, &h2);
	for (uint32_t i = 0; i < k_hashes; i++) {
		uint64_t bit = (h1 + (uint64_t)i * h2) % bloom_bits;
		bloom[bit >> 3] |= (uint8_t)(1u << (bit & 7));
	}
}

static int
bloom_test(const uint8_t *bloom, size_t bloom_bits, uint32_t k_hashes,
           const uint8_t hash[32])
{
	uint64_t h1, h2;
	bloom_h1_h2(hash, &h1, &h2);
	for (uint32_t i = 0; i < k_hashes; i++) {
		uint64_t bit = (h1 + (uint64_t)i * h2) % bloom_bits;
		if ((bloom[bit >> 3] & (1u << (bit & 7))) == 0) return 0;
	}
	return 1;
}

/* ── Builder ─────────────────────────────────────────────────────── */

struct staging_blob {
	tessera_hash_t  hash;
	uint8_t        *bytes;
	uint32_t        len;
	uint32_t        flags;
};

struct tessera_pack_builder {
	uint32_t              pack_kind;
	uint8_t               pack_id[16];
	uint64_t              creator_tx_id;
	struct staging_blob  *blobs;
	size_t                count;
	size_t                cap;
};

tessera_pack_builder_t *
tessera_pack_begin(uint32_t pack_kind, const uint8_t pack_id[16],
                   uint64_t creator_tx_id)
{
	tessera_pack_builder_t *b = tessera_zalloc(sizeof *b);
	if (b == NULL) return NULL;
	b->pack_kind     = pack_kind;
	b->creator_tx_id = creator_tx_id;
	if (pack_id != NULL) memcpy(b->pack_id, pack_id, 16);
	return b;
}

void
tessera_pack_free(tessera_pack_builder_t *b)
{
	if (b == NULL) return;
	for (size_t i = 0; i < b->count; i++) tessera_free(b->blobs[i].bytes);
	tessera_free(b->blobs);
	tessera_free(b);
}

int
tessera_pack_add_blob(tessera_pack_builder_t *b,
                      const tessera_hash_t blob_hash,
                      const uint8_t *bytes, uint32_t len,
                      uint32_t flags)
{
	if (b == NULL || blob_hash == NULL || (bytes == NULL && len > 0))
		return TESSERA_EINVAL;
	if (b->count + 1 > b->cap) {
		size_t cap = b->cap ? b->cap * 2 : 16;
		struct staging_blob *p = tessera_realloc(b->blobs, cap * sizeof *p);
		if (p == NULL) return TESSERA_ENOMEM;
		b->blobs = p;
		b->cap = cap;
	}
	struct staging_blob *s = &b->blobs[b->count];
	memcpy(s->hash, blob_hash, sizeof s->hash);
	s->len = len;
	s->flags = flags;
	s->bytes = NULL;
	if (len > 0) {
		s->bytes = tessera_malloc(len);
		if (s->bytes == NULL) return TESSERA_ENOMEM;
		memcpy(s->bytes, bytes, len);
	}
	b->count++;
	return TESSERA_OK;
}

static int
cmp_blobs(const void *a, const void *b)
{
	return memcmp(((const struct staging_blob *)a)->hash,
	              ((const struct staging_blob *)b)->hash, 32);
}

int
tessera_pack_finalize(tessera_pack_builder_t *b,
                      uint8_t *out_buffer, size_t buffer_len,
                      size_t *out_size)
{
	if (b == NULL || out_size == NULL) return TESSERA_EINVAL;

	/* Sort blobs by hash; reject duplicates. */
	if (b->count > 0)
		qsort(b->blobs, b->count, sizeof *b->blobs, cmp_blobs);
	for (size_t i = 1; i < b->count; i++)
		if (cmp_blobs(&b->blobs[i-1], &b->blobs[i]) == 0)
			return TESSERA_EEXIST;

	/* Bloom sizing. */
	size_t bloom_bits = (size_t)b->count * BLOOM_BITS_PER_BLOB;
	if (bloom_bits < BLOOM_MIN_BYTES * 8) bloom_bits = BLOOM_MIN_BYTES * 8;
	size_t bloom_bytes = (bloom_bits + 7) / 8;
	bloom_bytes = round_up(bloom_bytes, 64);   /* word-align for SIMD */
	bloom_bits = bloom_bytes * 8;
	uint32_t k_hashes = (uint32_t)((bloom_bits / (b->count ? b->count : 1))
	    * 693 / 1000);                          /* ln(2) ≈ 0.693 */
	if (k_hashes < 1) k_hashes = 1;
	if (k_hashes > 16) k_hashes = 16;

	/* Index sizing. */
	const size_t index_size_raw = b->count * TESSERA_PACK_INDEX_ENTRY_SIZE;
	const size_t index_size_pad = round_up(index_size_raw,
	    TESSERA_SECTOR_SIZE);
	const size_t bloom_size_pad = round_up(bloom_bytes,
	    TESSERA_SECTOR_SIZE);

	/* Data area sizing. */
	size_t data_len = 0;
	for (size_t i = 0; i < b->count; i++) {
		data_len = round_up(data_len + sizeof(tessera_blob_descriptor_t)
		    + b->blobs[i].len, BLOB_DATA_ALIGN);
	}
	const size_t data_size_pad = round_up(data_len, TESSERA_SECTOR_SIZE);

	const size_t header_off = 0;
	const size_t index_off  = TESSERA_SECTOR_SIZE;
	const size_t bloom_off  = index_off + index_size_pad;
	const size_t data_off   = bloom_off + bloom_size_pad;
	const size_t footer_off = data_off + data_size_pad;
	const size_t total      = footer_off + TESSERA_SECTOR_SIZE;

	*out_size = total;
	if (out_buffer == NULL || buffer_len < total) return TESSERA_ETOOBIG;
	memset(out_buffer, 0, total);

	/* Header. */
	tessera_pack_header_t hdr;
	memset(&hdr, 0, sizeof hdr);
	memcpy(hdr.magic, TESSERA_MAGIC_PACK, 8);
	hdr.version          = 1;
	hdr.pack_kind        = b->pack_kind;
	memcpy(hdr.pack_id, b->pack_id, 16);
	hdr.create_time      = 0;        /* caller may set via raw rewrite */
	hdr.creator_tx_id    = b->creator_tx_id;
	hdr.blob_count       = (uint32_t)b->count;
	hdr.index_blocks     = (uint32_t)(index_size_pad / TESSERA_SECTOR_SIZE);
	hdr.bloom_bytes      = (uint32_t)bloom_bytes;
	hdr.bloom_hash_count = k_hashes;
	hdr.data_offset      = data_off;
	hdr.data_length      = data_len;
	hdr.total_pack_bytes = total;

	int r = tessera_encode_pack_header(&hdr,
	    out_buffer + header_off);
	if (r != TESSERA_OK) return r;

	/* Bloom filter. */
	uint8_t *bloom = out_buffer + bloom_off;
	for (size_t i = 0; i < b->count; i++)
		bloom_set(bloom, bloom_bits, k_hashes, b->blobs[i].hash);

	/* Index + data area. */
	uint8_t *idx_base  = out_buffer + index_off;
	uint8_t *data_base = out_buffer + data_off;
	size_t   data_pos  = 0;
	for (size_t i = 0; i < b->count; i++) {
		const struct staging_blob *s = &b->blobs[i];

		tessera_pack_index_entry_t ie;
		memset(&ie, 0, sizeof ie);
		memcpy(ie.blob_hash, s->hash, 32);
		ie.data_offset = data_pos;
		ie.data_size   = s->len;
		ie.flags       = s->flags;
		(void)tessera_encode_pack_index_entry(&ie,
		    idx_base + i * TESSERA_PACK_INDEX_ENTRY_SIZE);

		tessera_blob_descriptor_t bd;
		memset(&bd, 0, sizeof bd);
		memcpy(bd.magic, TESSERA_MAGIC_BLOB, 4);
		bd.uncompressed_size = s->len;
		bd.compressed_size   = 0;
		(void)tessera_encode_blob_descriptor(&bd, data_base + data_pos);

		if (s->len > 0)
			memcpy(data_base + data_pos +
			    sizeof(tessera_blob_descriptor_t),
			    s->bytes, s->len);

		data_pos = round_up(data_pos +
		    sizeof(tessera_blob_descriptor_t) + s->len,
		    BLOB_DATA_ALIGN);
	}

	/* CRC over the data area (the full padded data_len). */
	uint32_t data_crc = tessera_crc32(data_base, data_len);

	/* Footer. */
	tessera_pack_footer_t ft;
	memset(&ft, 0, sizeof ft);
	memcpy(ft.magic, TESSERA_MAGIC_PACK_END, 8);
	ft.blob_count_check = (uint32_t)b->count;
	ft.crc32_pack       = data_crc;
	(void)tessera_encode_pack_footer(&ft, out_buffer + footer_off);

	return TESSERA_OK;
}

/* ── Reader ──────────────────────────────────────────────────────── */

struct tessera_pack_reader {
	const uint8_t          *data;
	size_t                  len;
	tessera_pack_header_t   header;
	const uint8_t          *index_base;
	const uint8_t          *bloom_base;
	const uint8_t          *data_base;
	size_t                  bloom_bits;
	uint32_t                k_hashes;
};

tessera_pack_reader_t *
tessera_pack_open(const uint8_t *data, size_t len)
{
	if (data == NULL || len < TESSERA_SECTOR_SIZE * 2) return NULL;

	tessera_pack_reader_t *r = tessera_zalloc(sizeof *r);
	if (r == NULL) return NULL;
	r->data = data;
	r->len  = len;

	if (tessera_decode_pack_header(data, &r->header) != TESSERA_OK)
		goto fail;
	if (r->header.total_pack_bytes != len) goto fail;

	r->index_base = data + TESSERA_SECTOR_SIZE;
	r->bloom_base = r->index_base +
	    (size_t)r->header.index_blocks * TESSERA_SECTOR_SIZE;
	r->data_base  = data + r->header.data_offset;
	r->bloom_bits = (size_t)r->header.bloom_bytes * 8u;
	r->k_hashes   = r->header.bloom_hash_count;

	/* Footer integrity. */
	tessera_pack_footer_t ft;
	const size_t footer_off = len - TESSERA_SECTOR_SIZE;
	if (tessera_decode_pack_footer(data + footer_off, &ft) != TESSERA_OK)
		goto fail;
	if (ft.blob_count_check != r->header.blob_count) goto fail;
	uint32_t want = tessera_crc32(r->data_base, r->header.data_length);
	if (ft.crc32_pack != want) goto fail;

	return r;
fail:
	tessera_free(r);
	return NULL;
}

uint32_t
tessera_pack_blob_count(const tessera_pack_reader_t *r)
{
	return r ? r->header.blob_count : 0;
}

int
tessera_pack_bloom_might_contain(const tessera_pack_reader_t *r,
                                 const tessera_hash_t blob_hash)
{
	if (r == NULL || r->bloom_bits == 0) return 1;
	return bloom_test(r->bloom_base, r->bloom_bits, r->k_hashes, blob_hash);
}

int
tessera_pack_lookup(const tessera_pack_reader_t *r,
                    const tessera_hash_t blob_hash,
                    const uint8_t **out_bytes, uint32_t *out_len)
{
	if (r == NULL || blob_hash == NULL ||
	    out_bytes == NULL || out_len == NULL)
		return TESSERA_EINVAL;

	int lo = 0, hi = (int)r->header.blob_count - 1;
	while (lo <= hi) {
		int mid = lo + (hi - lo) / 2;
		const uint8_t *e = r->index_base +
		    (size_t)mid * TESSERA_PACK_INDEX_ENTRY_SIZE;
		int c = memcmp(e, blob_hash, 32);
		if (c == 0) {
			tessera_pack_index_entry_t ie;
			(void)tessera_decode_pack_index_entry(e, &ie);
			if (ie.data_offset + sizeof(tessera_blob_descriptor_t)
			    + ie.data_size > r->header.data_length)
				return TESSERA_ECORRUPT;
			tessera_blob_descriptor_t bd;
			if (tessera_decode_blob_descriptor(
			    r->data_base + ie.data_offset, &bd) != TESSERA_OK)
				return TESSERA_ECORRUPT;
			*out_bytes = r->data_base + ie.data_offset +
			    sizeof(tessera_blob_descriptor_t);
			*out_len   = ie.data_size;
			return TESSERA_OK;
		}
		if (c < 0) lo = mid + 1;
		else       hi = mid - 1;
	}
	return TESSERA_ENOENT;
}

int
tessera_pack_blob_hash_at(const tessera_pack_reader_t *r, uint32_t index,
                          tessera_hash_t out)
{
	if (r == NULL || out == NULL) return TESSERA_EINVAL;
	if (index >= r->header.blob_count) return TESSERA_EINVAL;
	const uint8_t *e = r->index_base +
	    (size_t)index * TESSERA_PACK_INDEX_ENTRY_SIZE;
	memcpy(out, e, 32);
	return TESSERA_OK;
}

void
tessera_pack_close(tessera_pack_reader_t *r)
{
	tessera_free(r);
}
