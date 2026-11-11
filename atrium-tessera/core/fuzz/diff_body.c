/*
 * diff_body.c — one pass of every on-disk parser over one input, recording
 * what each call returned into a trace (D3 differential test).
 *
 * This file is compiled TWICE into the fuzz_diff binary: once against the C
 * core (the C decoders), and once against the core built with
 * -DTESSERA_RUST_DECODE (the Rust decoders), whose public symbols are renamed
 * rsmode_* (scripts/tessera-decode-diff.sh does the renaming with objcopy and
 * a generated -include header). fuzz_diff.c runs both and requires the two
 * traces to be identical: same return codes, same decoded bytes, same blob
 * offsets, same B-tree walk, same replayed journal records.
 *
 * Each target takes the input in the same shape as the matching fuzz_<name>.c
 * harness, so the committed corpora drive it unchanged.
 *
 * SPDX-License-Identifier: BSD-2-Clause
 */
#include <stddef.h>
#include <stdint.h>
#include <stdlib.h>
#include <string.h>

#include "tessera/error.h"
#include "tessera/format.h"
#include "tessera/codec.h"
#include "tessera/crc.h"
#include "tessera/btree.h"
#include "tessera/journal.h"
#include "tessera/manifest.h"
#include "tessera/pack.h"
#include "tessera/volume.h"

#include "diff_trace.h"

#ifndef DIFF_SIDE
#  error "build with -DDIFF_SIDE=c or -DDIFF_SIDE=rs"
#endif
#define CAT2(a, b) a##_##b
#define CAT(a, b)  CAT2(a, b)
#define ENTRY      CAT(diff_run, DIFF_SIDE)

#define SECT TESSERA_SECTOR_SIZE

/* A position inside the input buffer, or a marker for "elsewhere". Pointers
 * themselves differ between the two runs (separate copies); offsets do not. */
static uint64_t
rel(const void *p, const uint8_t *base, size_t len)
{
	if (p == NULL) return UINT64_MAX;
	const uint8_t *q = p;
	if (q < base || q > base + len) return UINT64_MAX - 1;
	return (uint64_t)(q - base);
}

/* ── block-device image ─────────────────────────────────────────────── */

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

static void
stamp(uint8_t *p, const char *magic, size_t mlen, size_t crc_off)
{
	memcpy(p, magic, mlen);
	uint32_t c = tessera_crc32(p, crc_off);
	memcpy(p + crc_off, &c, 4);
}

/* ── codec: every fixed-layout decoder on one sector ───────────────────── */

static void
run_codec(const uint8_t *data, size_t len, diff_trace_t *t)
{
	if (len < 1) return;
	const uint8_t ctl = data[0];
	uint8_t *in = calloc(1, SECT);
	uint8_t *out = malloc(SECT);
	if (in == NULL || out == NULL) { free(in); free(out); return; }
	memcpy(in, data + 1, len - 1 < SECT ? len - 1 : SECT);

	/* ctl picks which structure's magic and CRC to stamp, so the checks
	 * past those gates are reachable. */
	switch (ctl % 8u) {
	case 1: stamp(in, TESSERA_MAGIC_SUPERBLOCK, 8,
	            offsetof(tessera_superblock_t, crc32)); break;
	case 2: stamp(in, TESSERA_MAGIC_JOURNAL, 8,
	            offsetof(tessera_journal_header_t, crc32)); break;
	case 3: stamp(in, TESSERA_MAGIC_TXR, 4,
	            offsetof(tessera_record_header_t, crc32_header)); break;
	case 4: stamp(in, TESSERA_MAGIC_PACK, 8,
	            offsetof(tessera_pack_header_t, crc32_header)); break;
	case 5: stamp(in, TESSERA_MAGIC_BTREE_NODE, 4,
	            offsetof(tessera_btree_node_header_t, crc32)); break;
	case 6: {
		uint64_t m = TESSERA_PEL_MAGIC;
		memcpy(in, &m, 8);
		tessera_pack_extent_list_t *pel = (void *)in;
		/* 0x20: a count near the limit (0..255), 0x40: no zero-length
		 * extent, 0x80: total_length = the true sum. With all three
		 * only the count rule is left to decide. */
		if (ctl & 0x20) pel->extent_count &= 0xffu;
		uint32_t n = pel->extent_count;
		if (n > TESSERA_PEL_MAX_EXTENTS) n = TESSERA_PEL_MAX_EXTENTS;
		if (ctl & 0x40)
			for (uint32_t i = 0; i < n; i++)
				if (pel->extents[i].length_sectors == 0)
					pel->extents[i].length_sectors = 1;
		if (ctl & 0x80) {
			uint64_t sum = 0;
			for (uint32_t i = 0; i < n; i++)
				sum += pel->extents[i].length_sectors;
			pel->total_length = sum;
		}
		const size_t o = offsetof(tessera_pack_extent_list_t, crc32);
		uint32_t c = tessera_crc32(in, o);
		memcpy(in + o, &c, 4);
		break;
	}
	case 7: memcpy(in, ctl & 0x80 ? TESSERA_MAGIC_MANIFEST
	                              : TESSERA_MAGIC_BLOB, 4); break;
	default: break;
	}

#define DEC(fn, type) do {                                              \
		memset(out, 0xa5, SECT);                                \
		int _r = fn(in, (type *)(void *)out);                   \
		diff_put(t, (uint64_t)(int64_t)_r);                     \
		diff_put(t, tessera_crc32(out, sizeof(type)));          \
	} while (0)
	DEC(tessera_decode_superblock,        tessera_superblock_t);
	DEC(tessera_decode_journal_header,    tessera_journal_header_t);
	DEC(tessera_decode_record_header,     tessera_record_header_t);
	DEC(tessera_decode_pack_header,       tessera_pack_header_t);
	DEC(tessera_decode_pack_footer,       tessera_pack_footer_t);
	DEC(tessera_decode_pack_index_entry,  tessera_pack_index_entry_t);
	DEC(tessera_decode_blob_descriptor,   tessera_blob_descriptor_t);
	DEC(tessera_decode_manifest_header,   tessera_manifest_header_t);
	DEC(tessera_decode_inode,             tessera_inode_record_t);
	DEC(tessera_decode_quota_domain,      tessera_quota_domain_t);
	DEC(tessera_decode_btree_node_header, tessera_btree_node_header_t);
	DEC(tessera_decode_registry_entry,    tessera_registry_entry_t);
	DEC(tessera_decode_pack_extent_list,  tessera_pack_extent_list_t);
	DEC(tessera_decode_free_extent,       tessera_free_extent_t);
#undef DEC
	free(in);
	free(out);
}

/* ── manifest: parse + every accessor (fuzz_manifest.c input shape) ───── */

#define MFT_CAP 256u

static void
run_manifest(const uint8_t *data, size_t len, diff_trace_t *t)
{
	if (len < TESSERA_MANIFEST_HEADER_SIZE) return;
	uint8_t *buf = malloc(len);
	if (buf == NULL) return;
	memcpy(buf, data, len);
	memcpy(buf, TESSERA_MAGIC_MANIFEST, 4);

	tessera_manifest_parser_t *p = tessera_manifest_parse(buf, len);
	diff_put(t, p != NULL);
	if (p == NULL) { free(buf); return; }
	uint32_t count = tessera_manifest_parser_count(p);
	diff_put(t, (uint64_t)tessera_manifest_parser_kind(p));
	diff_put(t, tessera_manifest_parser_size(p));
	diff_put(t, count);
	/* Go past the count too: the ENOENT and ECORRUPT edges matter. */
	uint32_t n = count < MFT_CAP ? count + 2 : MFT_CAP;

	for (uint32_t i = 0; i < n; i++) {
		union {
			tessera_chunk_record_t c;
			tessera_tree_record_t tr;
			tessera_dir_bucket_record_t b;
			tessera_hash_t h;
		} u;
		memset(&u, 0, sizeof u);
		diff_put(t, (uint64_t)(int64_t)tessera_manifest_chunk_at(p, i, &u.c));
		diff_put(t, tessera_crc32((const uint8_t *)&u, sizeof u));
		memset(&u, 0, sizeof u);
		diff_put(t, (uint64_t)(int64_t)tessera_manifest_tree_at(p, i, &u.tr));
		diff_put(t, tessera_crc32((const uint8_t *)&u, sizeof u));
		memset(&u, 0, sizeof u);
		diff_put(t, (uint64_t)(int64_t)tessera_manifest_dir_bucket_at(p, i, &u.b));
		diff_put(t, tessera_crc32((const uint8_t *)&u, sizeof u));
		memset(&u, 0, sizeof u);
		diff_put(t, (uint64_t)(int64_t)tessera_manifest_dir_btree_inner_at(p, i, u.h));
		diff_put(t, tessera_crc32((const uint8_t *)&u, sizeof u));

		const char *nm = NULL; const uint8_t *val = NULL;
		uint16_t nl = 0, vl = 0; uint64_t ino = 0;
		diff_put(t, (uint64_t)(int64_t)tessera_manifest_xattr_at(p, i,
		    &nm, &nl, &val, &vl));
		diff_put(t, rel(nm, buf, len)); diff_put(t, nl);
		diff_put(t, rel(val, buf, len)); diff_put(t, vl);
		nm = NULL; nl = 0;
		diff_put(t, (uint64_t)(int64_t)tessera_manifest_dirent_at(p, i,
		    &ino, &nm, &nl));
		diff_put(t, ino); diff_put(t, rel(nm, buf, len)); diff_put(t, nl);
		nm = NULL; nl = 0; ino = 0;
		diff_put(t, (uint64_t)(int64_t)tessera_manifest_dir_btree_leaf_at(p,
		    i, &ino, &nm, &nl));
		diff_put(t, ino); diff_put(t, rel(nm, buf, len)); diff_put(t, nl);
	}
	/* NULL out-pointers, which the C accepts for some accessors. */
	diff_put(t, (uint64_t)(int64_t)tessera_manifest_xattr_at(p, 0,
	    NULL, NULL, NULL, NULL));
	diff_put(t, (uint64_t)(int64_t)tessera_manifest_dirent_at(p, 0,
	    NULL, NULL, NULL));
	diff_put(t, (uint64_t)(int64_t)tessera_manifest_dir_btree_is_leaf(p));
	const uint8_t *d = NULL; size_t dl = 0;
	diff_put(t, (uint64_t)(int64_t)tessera_manifest_inline_data(p, &d, &dl));
	diff_put(t, rel(d, buf, len)); diff_put(t, dl);
	tessera_manifest_parser_free(p);
	free(buf);
}

/* ── pack: open, index, lookup, bloom (fuzz_pack.c input shape) ──────── */

#define PACK_CAP 256u

/* The kernel's partial-read API (pack.h): geometry from the header sector
 * against a pack_len the input varies around the real size, then index
 * searches over an index buffer whose length the input also varies, so
 * short indexes and entries pointing anywhere are reached. */
static void
run_pack_partial(const uint8_t *buf, size_t len, uint8_t knob,
    diff_trace_t *t)
{
	static const int64_t dl[] = { 0, -1, -(int64_t)SECT, (int64_t)SECT,
	    (int64_t)SECT * 1000 };
	const uint64_t plen = (uint64_t)((int64_t)len + dl[knob % 5]);
	tessera_pack_geom_t g;
	memset(&g, 0, sizeof g);
	int rc = tessera_pack_geom_from_header(buf, plen, &g);
	diff_put(t, (uint64_t)(int64_t)rc);
	if (rc != TESSERA_OK) return;
	diff_put(t, g.index_off); diff_put(t, g.bloom_off);
	diff_put(t, g.bloom_bytes); diff_put(t, g.data_off);
	diff_put(t, g.data_len); diff_put(t, g.blob_count);
	diff_put(t, g.k_hashes);
	if (g.index_off > len) return;
	size_t idx_len = (size_t)(g.bloom_off - g.index_off);
	if (idx_len > len - g.index_off) idx_len = len - g.index_off;
	if (knob & 0x80) idx_len /= 2;          /* a short index */
	const uint8_t *idx = buf + g.index_off;
	uint32_t n = g.blob_count < PACK_CAP ? g.blob_count : PACK_CAP;
	for (uint32_t i = 0; i <= n; i++) {
		tessera_hash_t hh;
		if (i < n && (size_t)i * TESSERA_PACK_INDEX_ENTRY_SIZE + 32 <= idx_len)
			memcpy(hh, idx + (size_t)i * TESSERA_PACK_INDEX_ENTRY_SIZE, 32);
		else
			memcpy(hh, buf + SECT + 7, sizeof hh);
		uint64_t off = 0; uint32_t sz = 0;
		diff_put(t, (uint64_t)(int64_t)tessera_pack_index_find(idx, idx_len,
		    &g, hh, &off, &sz));
		diff_put(t, off); diff_put(t, sz);
	}
}

static void
run_pack(const uint8_t *data, size_t len, diff_trace_t *t)
{
	if (len < SECT * 2 || len > (1u << 20)) return;
	uint8_t *buf = malloc(len);
	if (buf == NULL) return;
	memcpy(buf, data, len);
	memcpy(buf, TESSERA_MAGIC_PACK, 8);
	uint64_t tpb = (uint64_t)len;
	memcpy(buf + offsetof(tessera_pack_header_t, total_pack_bytes), &tpb, 8);
	/* Also make the footer acceptable when the input asks for it (low bit
	 * of the last byte), so lookup and bloom are reached often: magic,
	 * blob count and the CRC over whatever data region the header names,
	 * if it lies inside the buffer. */
	tessera_pack_header_t *h = (void *)buf;
	if (data[len - 1] & 1) {
		uint8_t *f = buf + len - SECT;
		memcpy(f, TESSERA_MAGIC_PACK_END, 8);
		memcpy(f + offsetof(tessera_pack_footer_t, blob_count_check),
		    &h->blob_count, 4);
		if (h->data_offset <= len && h->data_length <= len - h->data_offset) {
			uint32_t c = tessera_crc32(buf + h->data_offset,
			    (size_t)h->data_length);
			memcpy(f + offsetof(tessera_pack_footer_t, crc32_pack), &c, 4);
		}
	}
	const size_t crc_off = offsetof(tessera_pack_header_t, crc32_header);
	uint32_t hcrc = tessera_crc32(buf, crc_off);
	memcpy(buf + crc_off, &hcrc, 4);

	run_pack_partial(buf, len, data[len - 2], t);

	tessera_pack_reader_t *r = tessera_pack_open(buf, len);
	diff_put(t, r != NULL);
	if (r == NULL) { free(buf); return; }
	uint32_t count = tessera_pack_blob_count(r);
	diff_put(t, count);
	diff_put(t, rel(tessera_pack_pack_id(r), buf, len) == UINT64_MAX - 1
	    ? tessera_crc32(tessera_pack_pack_id(r), 16) : 0);
	uint32_t n = count < PACK_CAP ? count : PACK_CAP;
	for (uint32_t i = 0; i <= n; i++) {
		tessera_hash_t hh;
		memset(hh, 0, sizeof hh);
		if (i < n) {
			diff_put(t, (uint64_t)(int64_t)tessera_pack_blob_hash_at(r, i, hh));
		} else {
			/* one hash the index does not hold (most likely) */
			memcpy(hh, buf + SECT + 7, sizeof hh);
		}
		diff_put(t, tessera_crc32(hh, sizeof hh));
		const uint8_t *bytes = NULL; uint32_t sz = 0;
		diff_put(t, (uint64_t)(int64_t)tessera_pack_lookup(r, hh, &bytes, &sz));
		diff_put(t, rel(bytes, buf, len)); diff_put(t, sz);
		/* Both readers cap probes at 64 (the C one looped up to 2^32
		 * times; found by this test as a libFuzzer timeout). */
		diff_put(t, (uint64_t)tessera_pack_bloom_might_contain(r, hh));
	}
	uint32_t edge[] = { count, count + 1, 0x7fffffffu, 0x80000000u, 0xffffffffu };
	for (unsigned e = 0; e < sizeof edge / sizeof edge[0]; e++) {
		tessera_hash_t hh;
		diff_put(t, (uint64_t)(int64_t)tessera_pack_blob_hash_at(r, edge[e], hh));
	}
	tessera_pack_close(r);
	free(buf);
}

/* ── B-tree: open, get, walks (fuzz_btree.c input shape) ──────────────── */

static const struct { uint8_t kind; uint32_t key; uint32_t val; } KINDS[] = {
	{ TESSERA_BTREE_KIND_INODE,      4,  TESSERA_INODE_RECORD_SIZE   },
	{ TESSERA_BTREE_KIND_PACK_REG,  16,  TESSERA_REGISTRY_ENTRY_SIZE },
	{ TESSERA_BTREE_KIND_FREE_EXT,   8,  8                           },
	{ TESSERA_BTREE_KIND_BLOB_INDEX,
	  TESSERA_BLOB_INDEX_KEY_SIZE,       TESSERA_BLOB_INDEX_VAL_SIZE },
};
#define NKINDS (sizeof KINDS / sizeof KINDS[0])
#define MAX_STEPS 4096u

static void
walk_cursor(tessera_btree_cursor_t *c, uint32_t ks, uint32_t vs,
            diff_trace_t *t)
{
	uint8_t key[64], val[1024];	/* btree_ops values reach 1000 */
	diff_put(t, c != NULL);
	for (unsigned n = 0; c != NULL && n < MAX_STEPS; n++) {
		int r = tessera_btree_cursor_get(c, key, val);
		diff_put(t, (uint64_t)(int64_t)r);
		if (r != TESSERA_OK) break;
		diff_put(t, tessera_crc32(key, ks));
		diff_put(t, tessera_crc32(val, vs));
		r = tessera_btree_cursor_next(c);
		diff_put(t, (uint64_t)(int64_t)r);
		if (r != TESSERA_OK) break;
	}
	tessera_btree_cursor_free(c);
}

static void
run_btree(const uint8_t *data, size_t len, diff_trace_t *t)
{
	if (len < 2 + SECT) return;
	const uint8_t sel = data[0], rsel = data[1];
	const uint8_t *body = data + 2;
	size_t body_len = len - 2;
	uint64_t sectors = body_len / SECT;
	if (sectors < 8) sectors = 8;
	if (sectors > 64) sectors = 64;
	uint8_t *img = malloc((size_t)sectors * SECT);
	if (img == NULL) return;
	for (size_t off = 0; off < (size_t)sectors * SECT; off += body_len) {
		size_t n = (size_t)sectors * SECT - off;
		if (n > body_len) n = body_len;
		memcpy(img + off, body, n);
	}
	const unsigned k = sel % NKINDS;
	/* Stamp like fuzz_btree.c, except when bit 7 of sel is set: then leave
	 * every other sector unstamped, so header, kind and geometry failures
	 * (and their last_fail codes) are compared too. Bit 6 keeps every
	 * entry_count below 512, where the per-kind fanout limits (27 to 336)
	 * decide, rather than in the billions where any limit does. */
	const size_t crc_off = offsetof(tessera_btree_node_header_t, crc32);
	for (uint64_t s = 0; s < sectors; s++) {
		if ((sel & 0x80) && (s & 1)) continue;
		uint8_t *n = img + s * SECT;
		if (sel & 0x40) {
			uint32_t ec;
			memcpy(&ec, n + offsetof(tessera_btree_node_header_t,
			    entry_count), 4);
			ec %= 512u;
			memcpy(n + offsetof(tessera_btree_node_header_t,
			    entry_count), &ec, 4);
		}
		uint8_t *nk = n + offsetof(tessera_btree_node_header_t, node_kind);
		if (sel & 0x40)
			*nk &= 1;	/* half leaves, half inner nodes */
		/* An inner node's value is its 8-byte child pointer. Stamping
		 * the leaf value size on every node (as at first) made every
		 * inner node fail the geometry check on both sides, so the
		 * inner-node fanout limit was never compared: a planted bug
		 * there went unnoticed. */
		const uint32_t vs = *nk == 0 ? KINDS[k].val : 8u;
		memcpy(n, TESSERA_MAGIC_BTREE_NODE, 4);
		n[offsetof(tessera_btree_node_header_t, tree_kind)] = KINDS[k].kind;
		memcpy(n + offsetof(tessera_btree_node_header_t, key_size), &KINDS[k].key, 4);
		memcpy(n + offsetof(tessera_btree_node_header_t, value_size), &vs, 4);
		uint32_t c = tessera_crc32(n, crc_off);
		memcpy(n + crc_off, &c, 4);
	}
	struct img m = { img, sectors };
	tessera_block_io_t io = { img_read, img_write, img_alloc, img_free, &m };
	tessera_btree_t *bt = tessera_btree_open(&io, rsel % sectors,
	    KINDS[k].kind, KINDS[k].key, KINDS[k].val);
	diff_put(t, bt != NULL);
	if (bt == NULL) { free(img); return; }
	tessera_btree_set_quiet_kind_mismatch(bt, 1);
	uint8_t key[64], val[512];
	memset(val, 0, sizeof val);
	memcpy(key, img, KINDS[k].key);
	diff_put(t, (uint64_t)(int64_t)tessera_btree_get(bt, key, val));
	diff_put(t, tessera_crc32(val, KINDS[k].val));
	walk_cursor(tessera_btree_seek_first(bt), KINDS[k].key, KINDS[k].val, t);
	memcpy(key, img + SECT + 100, KINDS[k].key);
	walk_cursor(tessera_btree_seek_at(bt, key), KINDS[k].key, KINDS[k].val, t);
	uint64_t fs = 0; uint8_t fk = 0;
	diff_put(t, (uint64_t)tessera_btree_last_fail(bt, &fs, &fk));
	diff_put(t, fs); diff_put(t, fk);
	tessera_btree_close(bt);
	free(img);
}


/* ── B-tree writes: put / delete / sorted batch on one image ────────────
 *
 * The node edits (insert, split, remove, the batch's leaf merge) run on a
 * tree both cores grow from empty with the same operations. Each side's
 * allocator is deterministic (lowest free sector), so identical edits give
 * byte-identical images; the trace records every return code, the root, each
 * displaced entry, and the image's CRC at intervals and at the end.
 *
 * The input picks the geometry (four from 4-entry leaves to 64-byte keys),
 * seeds the operation stream, and with bit 7 of the first byte corrupts
 * entry bytes of live nodes between operations (entries are outside the
 * header CRC), so edits on unsorted or self-referencing nodes are compared
 * too. */

#define OPS_SECTORS 1024u

static const struct { uint32_t key; uint32_t val; } OPS_GEOM[] = {
	{ 4,  TESSERA_INODE_RECORD_SIZE },	/* inode tree: fanout 27 */
	{ 8,  1000 },				/* 4-entry leaves: split often */
	{ 64, 8 },				/* widest key */
	{ 8,  8 },				/* free-extent: fanout 252 */
};

struct ops_img {
	struct img m;
	uint8_t    used[OPS_SECTORS];
};

static int
ops_alloc(void *c, uint64_t n, uint64_t *o)
{
	struct ops_img *d = c;
	if (n != 1) return -1;
	for (uint32_t i = 1; i < OPS_SECTORS; i++)
		if (!d->used[i]) { d->used[i] = 1; *o = i; return 0; }
	return -1;
}

static int
ops_free(void *c, uint64_t s, uint64_t n)
{
	struct ops_img *d = c;
	if (n != 1 || s == 0 || s >= OPS_SECTORS || !d->used[s]) return -1;
	d->used[s] = 0;
	return 0;
}

static uint64_t
ops_rand(uint64_t *x)
{
	*x ^= *x << 13; *x ^= *x >> 7; *x ^= *x << 17;
	return *x;
}

static void
ops_key(uint8_t *k, uint32_t ks, uint32_t id, uint8_t fill)
{
	memset(k, fill, ks);
	/* big-endian id at the end, so the key order is the id order */
	uint32_t at = ks >= 4 ? ks - 4 : 0;
	for (uint32_t i = 0; i < 4 && at + i < ks; i++)
		k[at + i] = (uint8_t)(id >> (24 - 8 * i));
}

static diff_trace_t *ops_trace;
static void
ops_displaced(void *ctx, const void *key, const void *old, const void *new_)
{
	const uint32_t *g = ctx;
	diff_put(ops_trace, tessera_crc32(key, g[0]));
	diff_put(ops_trace, tessera_crc32(old, g[1]));
	diff_put(ops_trace, tessera_crc32(new_, g[1]));
}

static uint64_t
ops_image_crc(const struct ops_img *d)
{
	uint32_t c = 0;
	for (uint32_t i = 1; i < OPS_SECTORS; i++)
		if (d->used[i])
			c = tessera_crc32(d->m.base + (size_t)i * SECT, SECT) ^
			    (c * 31u + i);
	return c;
}

static void
run_btree_ops(const uint8_t *data, size_t len, diff_trace_t *t)
{
	if (len < 4) return;
	const uint8_t ctl = data[0];
	const unsigned g = ctl % (sizeof OPS_GEOM / sizeof OPS_GEOM[0]);
	const uint32_t ks = OPS_GEOM[g].key, vs = OPS_GEOM[g].val;
	const uint32_t kspace = 16u + (uint32_t)data[1] * 4u;
	const uint32_t nops = 64u + (uint32_t)data[2] * 4u;
	uint64_t x = 0x9e3779b97f4a7c15ULL;
	for (size_t i = 0; i < len; i++) { x ^= data[i]; ops_rand(&x); }
	if (x == 0) x = 1;

	struct ops_img *d = calloc(1, sizeof *d);
	uint8_t *img = d ? calloc(OPS_SECTORS, SECT) : NULL;
	uint8_t *keys = malloc(64u * 64u), *vals = malloc(64u * 1000u);
	if (d == NULL || img == NULL || keys == NULL || vals == NULL) goto out;
	d->m.base = img; d->m.sectors = OPS_SECTORS;
	tessera_block_io_t io = { img_read, img_write, ops_alloc, ops_free, d };
	uint64_t root = 0;
	tessera_btree_t *bt = tessera_btree_create(&io, 0, ks, vs, &root);
	diff_put(t, bt != NULL);
	if (bt == NULL) goto out;
	uint32_t geom[2] = { ks, vs };
	ops_trace = t;

	uint8_t key[64], val[1000];
	for (uint32_t op = 0; op < nops; op++) {
		const uint8_t b = data[3 + op % (len - 3)];
		uint64_t r = ops_rand(&x);
		uint32_t id = (uint32_t)(r % kspace);
		int rc;
		switch ((b ^ (uint8_t)(r >> 32)) % 8) {
		case 0: case 1: case 2: case 3:		/* put */
			ops_key(key, ks, id, 0x5a);
			memset(val, (int)(r >> 40), vs);
			rc = tessera_btree_put(bt, key, val, &root);
			break;
		case 4: case 5:				/* delete */
			ops_key(key, ks, id, 0x5a);
			rc = tessera_btree_delete(bt, key, &root);
			break;
		case 6: {				/* sorted batch */
			uint32_t n = 1 + (uint32_t)(r >> 48) % 64u, k = 0;
			uint32_t step = 1 + (uint32_t)(r >> 56) % 4u;
			for (uint32_t i = 0; i < n && id + i * step < kspace * 2; i++, k++) {
				ops_key(keys + (size_t)k * ks, ks, id + i * step, 0x5a);
				memset(vals + (size_t)k * vs, (int)(op + i), vs);
			}
			rc = tessera_btree_put_sorted_batch_ex(bt, keys, vals, k,
			    &root, ops_displaced, geom);
			break;
		}
		default:				/* get */
			ops_key(key, ks, id, 0x5a);
			memset(val, 0, vs);
			rc = tessera_btree_get(bt, key, val);
			diff_put(t, tessera_crc32(val, vs));
			break;
		}
		diff_put(t, (uint64_t)(int64_t)rc);
		diff_put(t, tessera_btree_root(bt));
		if ((op & 31) == 31)
			diff_put(t, ops_image_crc(d));
		/* Damage a live node's entry bytes (not its header). */
		if ((ctl & 0x80) && (r & 0xf) == 0) {
			uint32_t s = 1 + (uint32_t)(r >> 8) % (OPS_SECTORS - 1);
			for (uint32_t i = 0; i < OPS_SECTORS && !d->used[s]; i++)
				s = s % (OPS_SECTORS - 1) + 1;
			if (d->used[s]) {
				uint32_t off = 64u + (uint32_t)(r >> 20) % (SECT - 64u);
				uint8_t *p = img + (size_t)s * SECT + off;
				if ((r >> 30) & 1) {
					/* point an 8-byte child slot at the root */
					uint64_t rt = tessera_btree_root(bt);
					if (off + 8 <= SECT) memcpy(p, &rt, 8);
				} else {
					*p ^= (uint8_t)(r >> 32) | 1;
				}
			}
		}
	}
	diff_put(t, ops_image_crc(d));
	walk_cursor(tessera_btree_seek_first(bt), ks, vs, t);
	tessera_btree_close(bt);
out:
	free(keys); free(vals); free(img); free(d);
}

/* ── journal: build, damage, replay (fuzz_journal.c input shape) ──────── */

#define J_SECTORS 64u
#define J_MAX_BODY (SECT * 3u)
#define J_MAX_OPS 64u
#define J_MAX_MUTS 16u

struct bits { const uint8_t *p; size_t len, off; };
static uint8_t  b8 (struct bits *b) { return b->off < b->len ? b->p[b->off++] : 0; }
static uint32_t b32(struct bits *b)
{
	uint32_t v = 0;
	for (int i = 0; i < 4; i++) v |= (uint32_t)b8(b) << (8 * i);
	return v;
}

static int
replay_cb(void *ctx, const tessera_record_header_t *h, const uint8_t *body)
{
	diff_trace_t *t = ctx;
	diff_put(t, tessera_crc32((const uint8_t *)h, sizeof *h));
	diff_put(t, h->body_length ? tessera_crc32(body, h->body_length) : 0);
	return 0;
}

static void
run_journal(const uint8_t *data, size_t len, diff_trace_t *t)
{
	if (len < 8) return;
	uint8_t *img = calloc(J_SECTORS, SECT);
	uint8_t *body = malloc(J_MAX_BODY);
	if (img == NULL || body == NULL) { free(img); free(body); return; }
	struct img m = { img, J_SECTORS };
	tessera_block_io_t io = { img_read, img_write, img_alloc, img_free, &m };
	struct bits b = { data, len, 0 };

	if (tessera_journal_format(&io, 0, J_SECTORS) != TESSERA_OK) goto out;
	tessera_journal_t *j = tessera_journal_open(&io, 0, J_SECTORS);
	diff_put(t, j != NULL);
	if (j == NULL) goto out;
	uint64_t tx = 0;
	int open_tx = 0;
	for (unsigned op = 0; op < J_MAX_OPS && b.off < b.len; op++) {
		switch (b8(&b) % 5u) {
		case 0:
			if (!open_tx && tessera_journal_tx_begin(j, &tx,
			    "fuzz\0\0\0\0\0\0\0\0\0\0\0") == TESSERA_OK)
				open_tx = 1;
			break;
		case 1: {
			uint32_t bl = b32(&b) % (J_MAX_BODY + 1u);
			for (uint32_t i = 0; i < bl; i++) body[i] = b8(&b);
			uint32_t ty = (b8(&b) % 12u) + 4u;
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
	unsigned nmut = b8(&b) % (J_MAX_MUTS + 1u);
	for (unsigned i = 0; i < nmut; i++) {
		uint32_t off = b32(&b) % (J_SECTORS * SECT);
		img[off] = b8(&b);
	}
	{
		const size_t jcrc = offsetof(tessera_journal_header_t, crc32);
		if (memcmp(img, TESSERA_MAGIC_JOURNAL, 8) == 0) {
			uint32_t c = tessera_crc32(img, jcrc);
			memcpy(img + jcrc, &c, 4);
		}
		const size_t rcrc = offsetof(tessera_record_header_t, crc32_header);
		for (uint64_t s = 1; s < J_SECTORS; s++) {
			uint8_t *r = img + s * SECT;
			if (memcmp(r, TESSERA_MAGIC_TXR, 4) != 0) continue;
			uint32_t c = tessera_crc32(r, rcrc);
			memcpy(r + rcrc, &c, 4);
		}
	}
	tessera_journal_t *j2 = tessera_journal_open(&io, 0, J_SECTORS);
	diff_put(t, j2 != NULL);
	if (j2 != NULL) {
		diff_put(t, (uint64_t)(int64_t)tessera_journal_replay(j2, replay_cb, t));
		tessera_journal_close(j2);
	}
out:
	free(body);
	free(img);
}

/* ── superblock: volume_open (fuzz_superblock.c input shape) ──────────── */

static void
run_superblock(const uint8_t *data, size_t len, diff_trace_t *t)
{
	if (len < 1 + SECT * 2) return;
	const uint8_t ctl = data[0];
	uint64_t sectors = (len - 1) / SECT;
	if (sectors > 8) sectors = 8;
	uint8_t *img = malloc((size_t)sectors * SECT);
	if (img == NULL) return;
	memcpy(img, data + 1, (size_t)sectors * SECT);
	if (ctl & 1) stamp(img, TESSERA_MAGIC_SUPERBLOCK, 8, offsetof(tessera_superblock_t, crc32));
	if (ctl & 2) stamp(img + SECT, TESSERA_MAGIC_SUPERBLOCK, 8, offsetof(tessera_superblock_t, crc32));
	struct img m = { img, sectors };
	tessera_block_io_t io = { img_read, img_write, img_alloc, img_free, &m };
	tessera_volume_t *v = NULL;
	int r = tessera_volume_open(&io, &v);
	diff_put(t, (uint64_t)(int64_t)r);
	if (r == TESSERA_OK && v != NULL) {
		diff_put(t, tessera_volume_total_sectors(v));
		diff_put(t, tessera_volume_pack_registry_root(v));
		diff_put(t, tessera_volume_meta_reserve_length(v));
		tessera_volume_close(v);
	}
	free(img);
}

void
ENTRY(int target, const uint8_t *data, size_t len, diff_trace_t *t)
{
	switch (target) {
	case DIFF_CODEC:      run_codec(data, len, t); break;
	case DIFF_MANIFEST:   run_manifest(data, len, t); break;
	case DIFF_PACK:       run_pack(data, len, t); break;
	case DIFF_BTREE:      run_btree(data, len, t); break;
	case DIFF_JOURNAL:    run_journal(data, len, t); break;
	case DIFF_SUPERBLOCK: run_superblock(data, len, t); break;
	case DIFF_BTREE_OPS:  run_btree_ops(data, len, t); break;
	default: break;
	}
}
