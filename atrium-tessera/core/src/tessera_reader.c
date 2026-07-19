/*
 * tessera_reader.c — read-only path/content reader (tessera/reader.h).
 *
 * Read side only: locate blobs by hash comparison (never re-hashed), so
 * this links with no hash implementation and reads volumes of any
 * content-hash algorithm. Built on the volume / btree / manifest / pack
 * primitives. Used by the FreeBSD loader's Tessera fs_ops and by tools.
 *
 * v1 limitations (documented, not silent): single-extent packs only
 * (multi-extent/PEL packs are skipped in the blob scan — large files that
 * spilled into gang extents won't read yet); no symlink following.
 */
#include "tessera_compat.h"

#include "tessera/error.h"
#include "tessera/format.h"
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

/* A pack whose body has been read + opened, kept so it is read at most
 * once per reader lifetime. The blob-fetch scan checks every cached pack
 * (cheap in-memory lookup) before reading any new pack, so reading a file
 * touches each of its packs exactly once — O(total bytes), not
 * O(chunks × packs). */
struct tessera_cached_pack {
	uint8_t  id[16];
	uint8_t *buf;
	tessera_pack_reader_t *pr;
	struct tessera_cached_pack *next;
};

/* Cap on cached pack bodies. Sequential file reads (the loader loading a
 * kernel) touch each pack once in order, so a small MRU window keeps the
 * O(total bytes) behaviour while bounding memory — critical in the loader,
 * whose heap can't hold a whole 19 MiB kernel's worth of pack bodies at
 * once (that exhausts it and the kernel load fails). */
#define TESSERA_MAX_CACHED_PACKS 6u

/* Bulk-read batch cap (16 sectors = 64 KiB): collapses per-sector
 * round-trips ~16x while staying within backends' per-transfer limits. */
#define TESSERA_READ_BATCH 16u

struct tessera_reader {
	tessera_block_io_t io;
	tessera_volume_t  *v;
	uint64_t inode_root;
	uint64_t pack_root;
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
	while (done < len) {
		uint64_t rem = len - done;
		uint32_t n = rem > TESSERA_READ_BATCH ? TESSERA_READ_BATCH : (uint32_t)rem;
		int ok = (rd->read_blocks != NULL) &&
		    rd->read_blocks(rd->io.ctx, start + done, n, buf + done * SECTOR) == 0;
		if (!ok) {
			for (uint32_t k = 0; k < n; k++)
				if (rd->io.read_block(rd->io.ctx, start + done + k,
				    buf + (done + k) * SECTOR) != 0)
					return -1;
		}
		done += n;
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
	return rd;
}

void
tessera_reader_close(tessera_reader_t *rd)
{
	if (rd == NULL) return;
	struct tessera_cached_pack *cp = rd->packs;
	while (cp != NULL) {
		struct tessera_cached_pack *n = cp->next;
		if (cp->pr) tessera_pack_close(cp->pr);
		if (cp->buf) tessera_free(cp->buf);
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

uint32_t tessera_reader_root_ino(const tessera_reader_t *rd)
{ (void)rd; return TESSERA_INODE_ROOT_DIR; }

/* Read a whole pack body (single-extent) into a fresh buffer. */
static uint8_t *
read_pack_body(tessera_reader_t *rd, uint64_t start, uint64_t len)
{
	if (len == 0 || len > (1u<<22)) return NULL;   /* sanity: < 16 GiB */
	uint8_t *buf = tessera_malloc((size_t)len * SECTOR);
	if (buf == NULL) return NULL;
	if (read_run(rd, start, len, buf) != 0) {
		tessera_free(buf);
		return NULL;
	}
	return buf;
}

/* Read a multi-extent pack body: walk the PEL (pack-extent-list) chain from
 * `pel_head`, concatenating the data extents into one contiguous buffer.
 * PEL layout: magic@0, extent_count@12, next_pel_sector@24, then
 * extent_count × {u64 start, u64 len} at offset 32. */
static uint8_t *
read_pack_body_multi(tessera_reader_t *rd, uint64_t pel_head, size_t *out_len)
{
	/* pass 1: collect extents (grow a small dynamic list) */
	uint64_t *es = NULL, *el = NULL;
	uint32_t ne = 0, cap = 0;
	uint64_t total = 0, cur = pel_head;
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
				uint64_t *ns = tessera_realloc(es, nc * sizeof *ns);
				uint64_t *nl = tessera_realloc(el, nc * sizeof *nl);
				if (ns == NULL || nl == NULL) { ok = 0; break; }
				es = ns; el = nl; cap = nc;
			}
			es[ne] = rd_u64(pel, 32 + i * 16);
			el[ne] = rd_u64(pel, 32 + i * 16 + 8);
			total += el[ne];
			ne++;
		}
		if (!ok) break;
		cur = next;
	}
	if (!ok || total == 0 || total > (1u << 22)) {
		if (es) tessera_free(es);
		if (el) tessera_free(el);
		return NULL;
	}
	/* pass 2: read the extents concatenated */
	uint8_t *buf = tessera_malloc((size_t)total * SECTOR);
	if (buf != NULL) {
		uint64_t pos = 0;
		for (uint32_t i = 0; i < ne && buf; i++) {
			if (read_run(rd, es[i], el[i], buf + pos * SECTOR) != 0) {
				tessera_free(buf); buf = NULL; break;
			}
			pos += el[i];
		}
	}
	tessera_free(es); tessera_free(el);
	if (buf) *out_len = (size_t)total * SECTOR;
	return buf;
}

/* Locate blob `hash`, returning a freshly-allocated copy (caller frees).
 * Consults the one-pack cache, then scans the pack registry. */
static int
dup_blob(const uint8_t *b, uint32_t bl, uint8_t **out, size_t *out_len)
{
	uint8_t *cp = tessera_malloc(bl);
	if (cp == NULL) return TESSERA_EIO;
	memcpy(cp, b, bl); *out = cp; *out_len = bl;
	return TESSERA_OK;
}

static int
fetch_blob_dup(tessera_reader_t *rd, const uint8_t *hash,
               uint8_t **out, size_t *out_len)
{
	const uint8_t *b; uint32_t bl;

	/* 1. check every already-read pack (cheap in-memory lookup) */
	for (struct tessera_cached_pack *cp = rd->packs; cp; cp = cp->next)
		if (tessera_pack_lookup(cp->pr, hash, &b, &bl) == 0 && b != NULL)
			return dup_blob(b, bl, out, out_len);

	/* 2. scan the registry, reading + caching each not-yet-cached pack
	 *    exactly once, until the blob turns up. */
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
		size_t body_len;
		uint8_t *body = (flags & TESSERA_REGISTRY_FLAG_MULTI_EXTENT)
		    ? read_pack_body_multi(rd, start, &body_len)
		    : (body_len = (size_t)len * SECTOR, read_pack_body(rd, start, len));
		if (body != NULL) {
			tessera_pack_reader_t *pr = tessera_pack_open(body, body_len);
			if (pr != NULL) {
				/* cache it (read at most once) */
				struct tessera_cached_pack *cp = tessera_zalloc(sizeof *cp);
				if (cp != NULL) {
					memcpy(cp->id, key, 16); cp->buf = body; cp->pr = pr;
					cp->next = rd->packs; rd->packs = cp; rd->npacks++;
					int found = (tessera_pack_lookup(pr, hash, &b, &bl) == 0 && b != NULL);
					if (found) rc = dup_blob(b, bl, out, out_len);
					/* evict the oldest pack(s) beyond the cap (tail of the
					 * MRU list); never the head we just added/used. */
					while (rd->npacks > TESSERA_MAX_CACHED_PACKS) {
						struct tessera_cached_pack *p = rd->packs;
						while (p->next && p->next->next) p = p->next;
						if (p->next) {
							if (p->next->pr) tessera_pack_close(p->next->pr);
							if (p->next->buf) tessera_free(p->next->buf);
							tessera_free(p->next);
							p->next = NULL;
							rd->npacks--;
						} else break;
					}
					if (found) break;
				} else { tessera_pack_close(pr); tessera_free(body); }
			} else {
				tessera_free(body);
			}
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
