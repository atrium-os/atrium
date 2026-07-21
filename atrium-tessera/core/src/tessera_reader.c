/*
 * tessera_reader.c — read-only path/content reader (tessera/reader.h).
 *
 * Read side only: locate blobs by hash comparison (never re-hashed), so
 * this links with no hash implementation and reads volumes of any
 * content-hash algorithm. Built on the volume / btree / manifest / pack
 * primitives. Used by the FreeBSD loader's Tessera fs_ops and by tools.
 *
 * Memory is bounded: only each touched pack's HEADER-derived layout, its
 * sorted blob INDEX, and its extent map are cached (a few KiB per pack).
 * Blob bytes are read on demand straight into the caller's buffer, never
 * cached — so reading a 19 MiB kernel that spans dozens of packs costs a few
 * hundred KiB in the loader's small heap, not the whole kernel's worth of
 * pack bodies. Single- and multi-extent (PEL) packs both read. No symlink
 * following (v1).
 */
#include "tessera_compat.h"

#include "tessera/error.h"
#include "tessera/format.h"
#include "tessera/codec.h"
#include "tessera/btree.h"
#include "tessera/volume.h"
#include "tessera/manifest.h"
#include "tessera/pack.h"
#include "tessera/reader.h"

#define SECTOR   TESSERA_SECTOR_SIZE
#define INOREC   TESSERA_INODE_RECORD_SIZE
#define REGENT   TESSERA_REGISTRY_ENTRY_SIZE

/* inode record field offsets (packed tessera_inode_record_t) */
#define INO_OFF_MODE     8u
#define INO_OFF_SIZE     56u
#define INO_OFF_MANIFEST 72u
/* registry entry field offsets (packed tessera_registry_entry_t) */
#define REG_OFF_START    16u
#define REG_OFF_LEN      24u
#define REG_OFF_FLAGS    60u

/* A pack's cached METADATA — its extent map (pack-logical → disk sectors),
 * the header-derived data layout, and the sorted blob index — read once per
 * pack. Blob bytes are NOT cached here: each blob is read on demand straight
 * into the caller's buffer. The blob-fetch scan bsearches every cached
 * index (cheap, in memory) before reading any new pack, so reading a file
 * touches each of its packs' metadata exactly once. */
struct pack_extent { uint64_t start; uint64_t len; };   /* disk sectors */

struct tessera_cached_pack {
	uint8_t  id[16];
	struct pack_extent *extents;    /* pack-logical → disk sector map */
	uint32_t n_extents;
	uint64_t total_sectors;         /* sum of extent lengths */
	uint64_t data_offset;           /* header.data_offset (bytes) */
	uint64_t data_length;           /* header.data_length (bytes) */
	uint32_t blob_count;
	uint32_t index_blocks;
	uint8_t *index;                 /* blob_count sorted 48-byte entries,
	                                 * padded to index_blocks sectors */
	struct tessera_cached_pack *next;
};

/* Cap on cached pack METADATA entries. Each is only a few KiB (index +
 * extent map), so a generous cap costs little and a real file's packs
 * (a kernel ≈ 30) all stay resident with no eviction. Past the cap,
 * eviction only re-reads a small index, never a whole pack body — so
 * unlike whole-body caching there is no thrash-to-failure or OOM. */
#define TESSERA_MAX_CACHED_PACKS 128u

/* Bulk-read batch ceiling (256 sectors = 1 MiB). read_run starts here and
 * halves on any bulk-read failure, so it auto-tunes DOWN to whatever the
 * backend's max transfer actually is (the FreeBSD loader's EFI ReadBlocks
 * accepts far less than 1 MiB) without a hard-coded guess. */
#define TESSERA_READ_BATCH 256u

struct tessera_reader {
	tessera_block_io_t io;
	tessera_volume_t  *v;
	uint64_t inode_root;
	uint64_t pack_root;
	uint64_t blob_index_root;            /* 0 = none (fall back to scan) */
	tessera_read_blocks_fn read_blocks;  /* optional bulk fast path */
	struct tessera_cached_pack *packs;   /* MRU list, capped */
	uint32_t npacks;
};

/* Read `len` contiguous sectors into buf: bulk fast path in batches when
 * available, falling back to per-sector read_block for any failed batch. */
static int
read_run(tessera_reader_t *rd, uint64_t start, uint64_t len, uint8_t *buf)
{
	uint64_t done = 0;
	uint32_t batch = TESSERA_READ_BATCH;   /* shrinks to the backend's max */
	while (done < len) {
		uint64_t rem = len - done;
		uint32_t n = rem > batch ? batch : (uint32_t)rem;
		if (rd->read_blocks != NULL &&
		    rd->read_blocks(rd->io.ctx, start + done, n, buf + done * SECTOR) == 0) {
			done += n;                 /* bulk read succeeded */
		} else if (rd->read_blocks != NULL && n > 1) {
			batch = n / 2;             /* too big — halve, retry same offset */
		} else {
			/* per-sector: either no bulk path, or even 1 sector failed
			 * via bulk (retry that one sector with read_block). */
			if (rd->io.read_block(rd->io.ctx, start + done,
			    buf + done * SECTOR) != 0)
				return -1;
			done += 1;
		}
	}
	return 0;
}

static uint32_t rd_u32(const uint8_t *b, unsigned o)
{ return (uint32_t)b[o] | ((uint32_t)b[o+1]<<8) | ((uint32_t)b[o+2]<<16) | ((uint32_t)b[o+3]<<24); }
static uint64_t rd_u64(const uint8_t *b, unsigned o)
{ uint64_t v=0; for (int i=7;i>=0;i--) v=(v<<8)|b[o+i]; return v; }
static void ino_key_be(uint32_t ino, uint8_t k[4])
{ k[0]=(uint8_t)(ino>>24); k[1]=(uint8_t)(ino>>16); k[2]=(uint8_t)(ino>>8); k[3]=(uint8_t)ino; }
static int is_null_hash(const uint8_t *h)
{ for (int i=0;i<32;i++) if (h[i]) return 0; return 1; }

tessera_reader_t *
tessera_reader_open(const tessera_block_io_t *io)
{
	return tessera_reader_open_ex(io, NULL);
}

tessera_reader_t *
tessera_reader_open_ex(const tessera_block_io_t *io,
                       tessera_read_blocks_fn read_blocks)
{
	if (io == NULL || io->read_block == NULL) return NULL;
	tessera_reader_t *rd = tessera_zalloc(sizeof *rd);
	if (rd == NULL) return NULL;
	rd->io = *io;
	rd->read_blocks = read_blocks;
	if (tessera_volume_open(&rd->io, &rd->v) != TESSERA_OK) {
		tessera_free(rd);
		return NULL;
	}
	rd->inode_root = tessera_volume_inode_root(rd->v);
	rd->pack_root  = tessera_volume_pack_registry_root(rd->v);
	rd->blob_index_root = tessera_volume_blob_index_root(rd->v);
	return rd;
}

void
tessera_reader_close(tessera_reader_t *rd)
{
	if (rd == NULL) return;
	struct tessera_cached_pack *cp = rd->packs;
	while (cp != NULL) {
		struct tessera_cached_pack *n = cp->next;
		if (cp->index) tessera_free(cp->index);
		if (cp->extents) tessera_free(cp->extents);
		tessera_free(cp);
		cp = n;
	}
	if (rd->v) tessera_volume_close(rd->v);
	tessera_free(rd);
}

/* Is pack_id already in the cache? */
static int
pack_cached(const tessera_reader_t *rd, const uint8_t id[16])
{
	for (const struct tessera_cached_pack *cp = rd->packs; cp; cp = cp->next)
		if (memcmp(cp->id, id, 16) == 0) return 1;
	return 0;
}

/* Fetch a pack's registry entry by pack_id (O(log n)). Returns 0 + fills
 * out[REGENT] on hit, -1 on miss. Used by the blob→pack index fast path. */
static int
registry_get(tessera_reader_t *rd, const uint8_t id[16], uint8_t out[REGENT])
{
	tessera_btree_t *t = tessera_btree_open(&rd->io, rd->pack_root,
	    TESSERA_BTREE_KIND_PACK_REG, 16, REGENT);
	if (t == NULL) return -1;
	int rc = tessera_btree_get(t, id, out);
	tessera_btree_close(t);
	return rc == TESSERA_OK ? 0 : -1;
}

uint32_t tessera_reader_root_ino(const tessera_reader_t *rd)
{ (void)rd; return TESSERA_INODE_ROOT_DIR; }

/* Collect a pack's disk extents from its registry entry. Single-extent packs
 * are one {start,len}; multi-extent packs walk the PEL (pack-extent-list)
 * chain from `start` (= pel_head). PEL layout: magic@0, extent_count@12,
 * next_pel_sector@24, then extent_count × {u64 start, u64 len} at 32.
 * Returns 0 (fills *out_ext malloc'd / *out_n / *out_total sectors) or -1. */
static int
collect_extents(tessera_reader_t *rd, uint32_t flags, uint64_t start,
                uint64_t len, struct pack_extent **out_ext,
                uint32_t *out_n, uint64_t *out_total)
{
	if (!(flags & TESSERA_REGISTRY_FLAG_MULTI_EXTENT)) {
		if (len == 0 || len > (1u << 22)) return -1;
		struct pack_extent *e = tessera_malloc(sizeof *e);
		if (e == NULL) return -1;
		e[0].start = start; e[0].len = len;
		*out_ext = e; *out_n = 1; *out_total = len;
		return 0;
	}
	struct pack_extent *es = NULL;
	uint32_t ne = 0, cap = 0;
	uint64_t total = 0, cur = start;
	int guard = 0, ok = 1;
	uint8_t pel[SECTOR];
	while (cur != 0 && guard++ < 512) {
		if (rd->io.read_block(rd->io.ctx, cur, pel) != 0) { ok = 0; break; }
		if (rd_u64(pel, 0) != TESSERA_PEL_MAGIC) { ok = 0; break; }
		uint32_t ec = rd_u32(pel, 12);
		if (ec > TESSERA_PEL_MAX_EXTENTS) { ok = 0; break; }
		uint64_t next = rd_u64(pel, 24);
		for (uint32_t i = 0; i < ec; i++) {
			if (ne == cap) {
				uint32_t nc = cap ? cap * 2 : 32;
				struct pack_extent *ns = tessera_realloc(es, nc * sizeof *ns);
				if (ns == NULL) { ok = 0; break; }
				es = ns; cap = nc;
			}
			es[ne].start = rd_u64(pel, 32 + i * 16);
			es[ne].len   = rd_u64(pel, 32 + i * 16 + 8);
			total += es[ne].len;
			ne++;
		}
		if (!ok) break;
		cur = next;
	}
	if (!ok || total == 0 || total > (1u << 22)) {
		if (es) tessera_free(es);
		return -1;
	}
	*out_ext = es; *out_n = ne; *out_total = total;
	return 0;
}

/* Read `len` bytes at pack-logical byte offset `off` into buf, mapping
 * pack-logical sectors to disk sectors through the extent map (a run per
 * extent). Reads whole covering sectors then copies out the sub-range. */
static int
read_pack_bytes(tessera_reader_t *rd, const struct tessera_cached_pack *pk,
                uint64_t off, uint64_t len, uint8_t *buf)
{
	if (len == 0) return 0;
	uint64_t end = off + len;
	if (end < off || end > pk->total_sectors * SECTOR) return -1;
	uint64_t first_sec = off / SECTOR;
	uint64_t last_sec  = (end - 1) / SECTOR;
	uint64_t n_sec = last_sec - first_sec + 1;
	uint8_t *tmp = tessera_malloc((size_t)n_sec * SECTOR);
	if (tmp == NULL) return -1;

	uint64_t psec = first_sec, remaining = n_sec;
	uint8_t *dst = tmp;
	while (remaining > 0) {
		/* locate pack-logical sector `psec` in the extent map */
		uint64_t acc = 0, in_ext = 0;
		const struct pack_extent *ex = NULL;
		for (uint32_t i = 0; i < pk->n_extents; i++) {
			if (psec < acc + pk->extents[i].len) {
				ex = &pk->extents[i]; in_ext = psec - acc; break;
			}
			acc += pk->extents[i].len;
		}
		if (ex == NULL) { tessera_free(tmp); return -1; }
		uint64_t run_left = ex->len - in_ext;
		uint64_t run = remaining < run_left ? remaining : run_left;
		if (read_run(rd, ex->start + in_ext, run, dst) != 0) {
			tessera_free(tmp); return -1;
		}
		dst += run * SECTOR; psec += run; remaining -= run;
	}
	memcpy(buf, tmp + (off - first_sec * SECTOR), (size_t)len);
	tessera_free(tmp);
	return 0;
}

/* Read + cache a pack's metadata (header + index + extent map). Prepends to
 * the MRU list and returns the new entry, or NULL. */
static struct tessera_cached_pack *
pack_load(tessera_reader_t *rd, const uint8_t id[16], uint32_t flags,
          uint64_t start, uint64_t len)
{
	struct pack_extent *ext = NULL; uint32_t ne = 0; uint64_t total = 0;
	if (collect_extents(rd, flags, start, len, &ext, &ne, &total) != 0)
		return NULL;
	struct tessera_cached_pack *pk = tessera_zalloc(sizeof *pk);
	if (pk == NULL) { tessera_free(ext); return NULL; }
	memcpy(pk->id, id, 16);
	pk->extents = ext; pk->n_extents = ne; pk->total_sectors = total;

	/* header = pack-logical sector 0 */
	uint8_t hdr[SECTOR];
	tessera_pack_header_t h;
	if (read_pack_bytes(rd, pk, 0, SECTOR, hdr) != 0) goto fail;
	if (tessera_decode_pack_header(hdr, &h) != TESSERA_OK) goto fail;
	if (h.total_pack_bytes != total * SECTOR) goto fail;
	pk->data_offset  = h.data_offset;
	pk->data_length  = h.data_length;
	pk->blob_count   = h.blob_count;
	pk->index_blocks = h.index_blocks;
	/* index must be large enough to hold blob_count sorted entries */
	if (pk->index_blocks == 0 ||
	    (uint64_t)pk->index_blocks * SECTOR <
	        (uint64_t)pk->blob_count * TESSERA_PACK_INDEX_ENTRY_SIZE)
		goto fail;

	/* index = index_blocks sectors starting at pack-logical sector 1 */
	pk->index = tessera_malloc((size_t)pk->index_blocks * SECTOR);
	if (pk->index == NULL) goto fail;
	if (read_pack_bytes(rd, pk, SECTOR,
	    (uint64_t)pk->index_blocks * SECTOR, pk->index) != 0)
		goto fail;

	pk->next = rd->packs; rd->packs = pk; rd->npacks++;
	return pk;
fail:
	if (pk->index) tessera_free(pk->index);
	tessera_free(pk->extents);
	tessera_free(pk);
	return NULL;
}

/* Binary-search a cached pack's sorted index for `hash`; fill *ie on a hit. */
static int
pack_find_blob(const struct tessera_cached_pack *pk, const uint8_t *hash,
               tessera_pack_index_entry_t *ie)
{
	int lo = 0, hi = (int)pk->blob_count - 1;
	while (lo <= hi) {
		int mid = lo + (hi - lo) / 2;
		const uint8_t *e = pk->index +
		    (size_t)mid * TESSERA_PACK_INDEX_ENTRY_SIZE;
		int c = memcmp(e, hash, 32);            /* blob_hash is first field */
		if (c == 0) { tessera_decode_pack_index_entry(e, ie); return 1; }
		if (c < 0) lo = mid + 1; else hi = mid - 1;
	}
	return 0;
}

/* Read the blob described by `ie` from pack `pk` into a fresh buffer (caller
 * frees). Blob bytes follow a 16-byte descriptor in the pack's data area. */
static int
read_blob(tessera_reader_t *rd, const struct tessera_cached_pack *pk,
          const tessera_pack_index_entry_t *ie, uint8_t **out, size_t *out_len)
{
	uint64_t desc = sizeof(tessera_blob_descriptor_t);
	if (ie->data_offset + desc + ie->data_size > pk->data_length)
		return TESSERA_ECORRUPT;
	uint8_t *cp = tessera_malloc(ie->data_size ? ie->data_size : 1);
	if (cp == NULL) return TESSERA_EIO;
	if (ie->data_size > 0 &&
	    read_pack_bytes(rd, pk, pk->data_offset + ie->data_offset + desc,
	    ie->data_size, cp) != 0) {
		tessera_free(cp);
		return TESSERA_EIO;
	}
	*out = cp; *out_len = ie->data_size;
	return TESSERA_OK;
}

/* Evict MRU-tail packs beyond the cap (only small metadata is freed). */
static void
evict_packs(tessera_reader_t *rd)
{
	while (rd->npacks > TESSERA_MAX_CACHED_PACKS) {
		struct tessera_cached_pack *p = rd->packs;
		while (p->next && p->next->next) p = p->next;
		if (p->next == NULL) break;
		if (p->next->index) tessera_free(p->next->index);
		if (p->next->extents) tessera_free(p->next->extents);
		tessera_free(p->next);
		p->next = NULL;
		rd->npacks--;
	}
}

/* Locate blob `hash` and return a freshly-allocated copy (caller frees).
 * Consults each cached pack's index, then scans the registry, loading each
 * not-yet-cached pack's METADATA once and reading the blob on demand. */
static int
fetch_blob_dup(tessera_reader_t *rd, const uint8_t *hash,
               uint8_t **out, size_t *out_len)
{
	tessera_pack_index_entry_t ie;

	/* 1. bsearch every cached pack's index (in-memory) */
	for (struct tessera_cached_pack *pk = rd->packs; pk; pk = pk->next)
		if (pack_find_blob(pk, hash, &ie))
			return read_blob(rd, pk, &ie, out, out_len);

	/* 2. blob→pack index (if present): resolve hash→pack_id in O(log n),
	 *    load that one pack, and verify. Only reached on a cold-pack miss
	 *    (sequential reads keep hitting the cached pack at step 1), so this
	 *    costs one index lookup + one pack load per distinct pack — not per
	 *    blob. A stale/wrong entry (repack/GC moved the blob) simply falls
	 *    through to the scan. */
	if (rd->blob_index_root != 0) {
		tessera_btree_t *bt = tessera_btree_open(&rd->io,
		    rd->blob_index_root, TESSERA_BTREE_KIND_BLOB_INDEX,
		    TESSERA_BLOB_INDEX_KEY_SIZE, TESSERA_BLOB_INDEX_VAL_SIZE);
		if (bt != NULL) {
			uint8_t pack_id[16];
			int got = tessera_btree_get(bt, hash, pack_id);
			tessera_btree_close(bt);
			if (got == TESSERA_OK) {
				uint8_t rv[REGENT];
				if (registry_get(rd, pack_id, rv) == 0) {
					uint32_t flags = rd_u32(rv, REG_OFF_FLAGS);
					uint64_t start = rd_u64(rv, REG_OFF_START);
					uint64_t len   = rd_u64(rv, REG_OFF_LEN);
					struct tessera_cached_pack *pk =
					    pack_load(rd, pack_id, flags, start, len);
					if (pk != NULL && pack_find_blob(pk, hash, &ie)) {
						int rc = read_blob(rd, pk, &ie, out, out_len);
						evict_packs(rd);
						return rc;
					}
					evict_packs(rd);   /* stale → fall to scan */
				}
			}
		}
	}

	/* 3. scan the registry, caching each not-yet-cached pack's metadata
	 *    once, until the blob's pack turns up (fallback: no/stale index). */
	tessera_btree_t *t = tessera_btree_open(&rd->io, rd->pack_root,
	    TESSERA_BTREE_KIND_PACK_REG, 16, REGENT);
	if (t == NULL) return TESSERA_EIO;
	int rc = TESSERA_ENOENT;
	tessera_btree_cursor_t *c = tessera_btree_seek_first(t);
	while (c != NULL) {
		uint8_t key[16], val[REGENT];
		if (tessera_btree_cursor_get(c, key, val) != 0) break;
		if (pack_cached(rd, key)) { if (tessera_btree_cursor_next(c) != 0) break; else continue; }

		uint32_t flags = rd_u32(val, REG_OFF_FLAGS);
		uint64_t start = rd_u64(val, REG_OFF_START);
		uint64_t len   = rd_u64(val, REG_OFF_LEN);
		struct tessera_cached_pack *pk = pack_load(rd, key, flags, start, len);
		if (pk != NULL) {
			int found = pack_find_blob(pk, hash, &ie);
			if (found) rc = read_blob(rd, pk, &ie, out, out_len);
			evict_packs(rd);   /* head (just loaded) is never evicted */
			if (found) break;
		}
		if (tessera_btree_cursor_next(c) != 0) break;
	}
	if (c) tessera_btree_cursor_free(c);
	tessera_btree_close(t);
	return rc;
}

int
tessera_reader_stat_ino(tessera_reader_t *rd, uint32_t ino,
                        uint32_t *out_mode, uint64_t *out_size)
{
	tessera_btree_t *t = tessera_btree_open(&rd->io, rd->inode_root,
	    TESSERA_BTREE_KIND_INODE, 4, INOREC);
	if (t == NULL) return TESSERA_EIO;
	uint8_t key[4], val[INOREC];
	ino_key_be(ino, key);
	int rc = tessera_btree_get(t, key, val);
	tessera_btree_close(t);
	if (rc != TESSERA_OK) return TESSERA_ENOENT;
	if (out_mode) *out_mode = rd_u32(val, INO_OFF_MODE);
	if (out_size) *out_size = rd_u64(val, INO_OFF_SIZE);
	return TESSERA_OK;
}

/* fetch inode's manifest_hash into out[32]; 0 or ENOENT */
static int
inode_manifest(tessera_reader_t *rd, uint32_t ino, uint8_t out[32],
               uint32_t *out_mode, uint64_t *out_size)
{
	tessera_btree_t *t = tessera_btree_open(&rd->io, rd->inode_root,
	    TESSERA_BTREE_KIND_INODE, 4, INOREC);
	if (t == NULL) return TESSERA_EIO;
	uint8_t key[4], val[INOREC];
	ino_key_be(ino, key);
	int rc = tessera_btree_get(t, key, val);
	tessera_btree_close(t);
	if (rc != TESSERA_OK) return TESSERA_ENOENT;
	memcpy(out, val + INO_OFF_MANIFEST, 32);
	if (out_mode) *out_mode = rd_u32(val, INO_OFF_MODE);
	if (out_size) *out_size = rd_u64(val, INO_OFF_SIZE);
	return TESSERA_OK;
}

/* ── directory walk ─────────────────────────────────────────────── */

typedef int (*dirent_fn)(void *ctx, uint64_t child, const char *name, uint16_t nlen);

static int
walk_dir(tessera_reader_t *rd, const uint8_t *dir_hash, dirent_fn cb,
         void *ctx, int depth)
{
	if (is_null_hash(dir_hash) || depth > 64) return 0;
	uint8_t *bytes = NULL; size_t blen = 0;
	if (fetch_blob_dup(rd, dir_hash, &bytes, &blen) != TESSERA_OK) return 0;
	tessera_manifest_parser_t *p = tessera_manifest_parse(bytes, blen);
	if (p == NULL) { tessera_free(bytes); return 0; }
	int stop = 0;
	int kind = tessera_manifest_parser_kind(p);
	uint32_t n = tessera_manifest_parser_count(p);
	if (kind == TESSERA_MFT_DIRECTORY) {
		for (uint32_t i = 0; !stop; i++) {
			uint64_t child; const char *nm; uint16_t nl;
			if (tessera_manifest_dirent_at(p, i, &child, &nm, &nl) != 0) break;
			stop = cb(ctx, child, nm, nl);
		}
	} else if (kind == TESSERA_MFT_DIRECTORY_2L) {
		for (uint32_t i = 0; i < n && !stop; i++) {
			tessera_dir_bucket_record_t br;
			if (tessera_manifest_dir_bucket_at(p, i, &br) == 0)
				stop = walk_dir(rd, br.bucket_manifest_hash, cb, ctx, depth+1);
		}
	} else if (kind == TESSERA_MFT_DIRECTORY_BTREE) {
		if (tessera_manifest_dir_btree_is_leaf(p) == 1) {
			for (uint32_t i = 0; !stop; i++) {
				uint64_t child; const char *nm; uint16_t nl;
				if (tessera_manifest_dir_btree_leaf_at(p, i, &child, &nm, &nl) != 0) break;
				stop = cb(ctx, child, nm, nl);
			}
		} else {
			for (uint32_t i = 0; !stop; i++) {
				uint8_t ch[32];
				if (tessera_manifest_dir_btree_inner_at(p, i, ch) != 0) break;
				stop = walk_dir(rd, ch, cb, ctx, depth+1);
			}
		}
	}
	tessera_manifest_parser_free(p);
	tessera_free(bytes);
	return stop;
}

struct lookup_ctx { const char *name; uint16_t nlen; uint64_t found; int hit; };
static int
lookup_cb(void *vctx, uint64_t child, const char *name, uint16_t nlen)
{
	struct lookup_ctx *c = vctx;
	if (nlen == c->nlen && memcmp(name, c->name, nlen) == 0) {
		c->found = child; c->hit = 1; return 1; /* stop */
	}
	return 0;
}

/* resolve one component within dir (by manifest hash) */
static int
dir_lookup(tessera_reader_t *rd, const uint8_t *dir_hash,
           const char *name, size_t nlen, uint32_t *out_child)
{
	struct lookup_ctx c = { name, (uint16_t)nlen, 0, 0 };
	walk_dir(rd, dir_hash, lookup_cb, &c, 0);
	if (!c.hit) return TESSERA_ENOENT;
	*out_child = (uint32_t)c.found;
	return TESSERA_OK;
}

int
tessera_reader_lookup(tessera_reader_t *rd, const char *path,
                      uint32_t *out_ino, uint32_t *out_mode, uint64_t *out_size)
{
	uint32_t ino = TESSERA_INODE_ROOT_DIR;
	uint32_t mode; uint64_t size; uint8_t mhash[32];
	if (inode_manifest(rd, ino, mhash, &mode, &size) != TESSERA_OK)
		return TESSERA_ENOENT;

	const char *p = path;
	while (*p == '/') p++;
	while (*p) {
		const char *start = p;
		while (*p && *p != '/') p++;
		size_t clen = (size_t)(p - start);
		while (*p == '/') p++;
		if (clen == 0) continue;
		if (clen == 1 && start[0] == '.') continue;
		uint32_t child;
		if (dir_lookup(rd, mhash, start, clen, &child) != TESSERA_OK)
			return TESSERA_ENOENT;
		ino = child;
		if (inode_manifest(rd, ino, mhash, &mode, &size) != TESSERA_OK)
			return TESSERA_ENOENT;
	}
	if (out_ino) *out_ino = ino;
	if (out_mode) *out_mode = mode;
	if (out_size) *out_size = size;
	return TESSERA_OK;
}

/* ── content read ───────────────────────────────────────────────── */

/* copy the overlap of source span [soff, soff+slen) with request
 * [off, off+len) into buf; `src` NULL means a zero span. */
static void
copy_overlap(uint8_t *buf, uint64_t off, size_t len,
             const uint8_t *src, uint64_t soff, uint64_t slen)
{
	uint64_t a = off > soff ? off : soff;
	uint64_t b = (off + len) < (soff + slen) ? (off + len) : (soff + slen);
	if (a >= b) return;
	size_t dst = (size_t)(a - off);
	if (src) memcpy(buf + dst, src + (a - soff), (size_t)(b - a));
	else memset(buf + dst, 0, (size_t)(b - a));
}

static int
read_range(tessera_reader_t *rd, const uint8_t *mhash, uint64_t file_size,
           uint64_t off, uint8_t *buf, size_t len, int depth)
{
	if (is_null_hash(mhash) || depth > 64) return TESSERA_OK;
	uint8_t *bytes = NULL; size_t blen = 0;
	if (fetch_blob_dup(rd, mhash, &bytes, &blen) != TESSERA_OK) return TESSERA_EIO;
	tessera_manifest_parser_t *p = tessera_manifest_parse(bytes, blen);
	if (p == NULL) { tessera_free(bytes); return TESSERA_EIO; }
	int kind = tessera_manifest_parser_kind(p);
	uint32_t n = tessera_manifest_parser_count(p);
	int rc = TESSERA_OK;

	if (kind == TESSERA_MFT_INLINE) {
		const uint8_t *d; size_t dl;
		if (tessera_manifest_inline_data(p, &d, &dl) == 0)
			copy_overlap(buf, off, len, d, 0, dl);
	} else if (kind == TESSERA_MFT_CHUNK_LIST) {
		for (uint32_t i = 0; i < n; i++) {
			tessera_chunk_record_t cr;
			if (tessera_manifest_chunk_at(p, i, &cr) != 0) continue;
			uint64_t co = cr.logical_offset, cl = cr.uncompressed_size;
			if (co + cl <= off || co >= off + len) continue;  /* no overlap */
			if (cr.flags & TESSERA_CHUNK_FLAG_ZERO_HOLE) {
				copy_overlap(buf, off, len, NULL, co, cl);
			} else {
				uint8_t *cb = NULL; size_t cbl = 0;
				if (fetch_blob_dup(rd, cr.chunk_hash, &cb, &cbl) == TESSERA_OK) {
					copy_overlap(buf, off, len, cb, co, cbl);
					tessera_free(cb);
				} else { rc = TESSERA_EIO; break; }
			}
		}
	} else if (kind == TESSERA_MFT_CHUNK_TREE) {
		for (uint32_t i = 0; i < n; i++) {
			tessera_tree_record_t tr;
			if (tessera_manifest_tree_at(p, i, &tr) != 0) continue;
			uint64_t cstart = tr.logical_offset;
			uint64_t cend = file_size;   /* last child extends to EOF */
			tessera_tree_record_t nx;
			if (i + 1 < n && tessera_manifest_tree_at(p, i + 1, &nx) == 0)
				cend = nx.logical_offset;
			if (cend <= off || cstart >= off + len) continue;
			rc = read_range(rd, tr.child_manifest_hash, file_size, off, buf, len, depth + 1);
			if (rc != TESSERA_OK) break;
		}
	}
	tessera_manifest_parser_free(p);
	tessera_free(bytes);
	return rc;
}

int
tessera_reader_pread(tessera_reader_t *rd, uint32_t ino, uint64_t off,
                     void *buf, size_t len, size_t *out_read)
{
	uint32_t mode; uint64_t size; uint8_t mhash[32];
	if (inode_manifest(rd, ino, mhash, &mode, &size) != TESSERA_OK)
		return TESSERA_ENOENT;
	if (out_read) *out_read = 0;
	if (off >= size) return TESSERA_OK;
	if (off + len > size) len = (size_t)(size - off);
	if (len == 0) return TESSERA_OK;
	memset(buf, 0, len);
	int rc = read_range(rd, mhash, size, off, buf, len, 0);
	if (rc == TESSERA_OK && out_read) *out_read = len;
	return rc;
}

/* ── readdir ────────────────────────────────────────────────────── */

struct rd_ctx { uint32_t want; uint32_t cur; uint64_t child; char *name; size_t cap; int hit; };
static int
rd_cb(void *vctx, uint64_t child, const char *name, uint16_t nlen)
{
	struct rd_ctx *c = vctx;
	if (nlen == 1 && name[0] == '.') return 0;
	if (nlen == 2 && name[0] == '.' && name[1] == '.') return 0;
	if (c->cur == c->want) {
		size_t k = nlen < c->cap - 1 ? nlen : c->cap - 1;
		memcpy(c->name, name, k); c->name[k] = '\0';
		c->child = child; c->hit = 1;
		return 1;
	}
	c->cur++;
	return 0;
}

int
tessera_reader_readdir(tessera_reader_t *rd, uint32_t dir_ino, uint32_t idx,
                       char *name_out, size_t name_cap,
                       uint64_t *out_child_ino, uint32_t *out_child_mode)
{
	uint32_t mode; uint64_t size; uint8_t mhash[32];
	if (inode_manifest(rd, dir_ino, mhash, &mode, &size) != TESSERA_OK)
		return TESSERA_ENOENT;
	struct rd_ctx c = { idx, 0, 0, name_out, name_cap, 0 };
	walk_dir(rd, mhash, rd_cb, &c, 0);
	if (!c.hit) return TESSERA_ENOENT;
	if (out_child_ino) *out_child_ino = c.child;
	if (out_child_mode) {
		uint32_t cm = 0;
		(void)tessera_reader_stat_ino(rd, (uint32_t)c.child, &cm, NULL);
		*out_child_mode = cm;
	}
	return TESSERA_OK;
}
