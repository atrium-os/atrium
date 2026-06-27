/*
 * tessera-core: manifest builder + parser.
 *
 * Layout on disk:
 *   bytes 0..31    tessera_manifest_header_t  (kind, level, logical_size,
 *                                              chunk_size_avg, entry_count)
 *   bytes 32..     body, encoding determined by kind:
 *
 *     INLINE       raw bytes (logical_size = entry_count = byte length)
 *     CHUNK_LIST   N × tessera_chunk_record_t   (48 B each)
 *     CHUNK_TREE   N × tessera_tree_record_t    (40 B each)
 *     SYMLINK      target string (no NUL); logical_size = byte length
 *     DIRECTORY    N × { uint64 child_inode | uint16 name_len | name[] }
 *                  Sorted by name (memcmp); enables a future binary-
 *                  search lookup with no further work.
 *
 * The manifest hash is SHA-256 over the encoded bytes (header + body).
 * tessera_manifest_finalize() returns both the byte buffer and the hash
 * so callers can publish (hash → blob) atomically.
 *
 * Phase 1 implements INLINE, CHUNK_LIST, CHUNK_TREE, SYMLINK, DIRECTORY.
 * XATTR_STORE and GC_ROOT_LIST share the directory encoding shape and
 * land alongside the xattr / GC implementations.
 */

#include "tessera/manifest.h"
#include "tessera/codec.h"
#include "tessera/error.h"
#include "tessera/format.h"
#include "tessera/hash.h"

#include "tessera_compat.h"

#define HEADER_SIZE 32u

/* ── Builder ─────────────────────────────────────────────────────── */

struct tessera_manifest_builder {
	tessera_manifest_kind_t  kind;
	uint8_t                 *body;
	size_t                   body_len;
	size_t                   body_cap;
	uint32_t                 entry_count;
	uint64_t                 logical_size;
};

static int
body_reserve(tessera_manifest_builder_t *b, size_t need)
{
	if (b->body_cap >= need) return 0;
	size_t cap = b->body_cap ? b->body_cap : 256;
	while (cap < need) cap *= 2;
	uint8_t *p = tessera_realloc(b->body, cap);
	if (p == NULL) return -1;
	b->body = p;
	b->body_cap = cap;
	return 0;
}

static int
body_append(tessera_manifest_builder_t *b, const void *data, size_t n)
{
	if (body_reserve(b, b->body_len + n) != 0) return -1;
	memcpy(b->body + b->body_len, data, n);
	b->body_len += n;
	return 0;
}

tessera_manifest_builder_t *
tessera_manifest_begin(tessera_manifest_kind_t kind)
{
	tessera_manifest_builder_t *b = tessera_zalloc(sizeof *b);
	if (b == NULL) return NULL;
	b->kind = kind;
	return b;
}

void
tessera_manifest_free(tessera_manifest_builder_t *b)
{
	if (b == NULL) return;
	tessera_free(b->body);
	tessera_free(b);
}

int
tessera_manifest_add_chunk(tessera_manifest_builder_t *b,
                           const tessera_hash_t chunk_hash,
                           uint64_t logical_offset,
                           uint32_t size,
                           uint32_t flags)
{
	if (b == NULL) return TESSERA_EINVAL;
	if (b->kind != TESSERA_MFT_CHUNK_LIST) return TESSERA_EINVAL;

	tessera_chunk_record_t r;
	memset(&r, 0, sizeof r);
	memcpy(r.chunk_hash, chunk_hash, sizeof r.chunk_hash);
	r.logical_offset    = logical_offset;
	r.uncompressed_size = size;
	r.flags             = flags;
	if (body_append(b, &r, sizeof r) != 0) return TESSERA_ENOMEM;

	b->entry_count++;
	if (logical_offset + size > b->logical_size)
		b->logical_size = logical_offset + size;
	return TESSERA_OK;
}

int
tessera_manifest_add_tree_child(tessera_manifest_builder_t *b,
                                const tessera_hash_t child_hash,
                                uint64_t logical_offset)
{
	if (b == NULL) return TESSERA_EINVAL;
	if (b->kind != TESSERA_MFT_CHUNK_TREE) return TESSERA_EINVAL;

	tessera_tree_record_t r;
	memset(&r, 0, sizeof r);
	memcpy(r.child_manifest_hash, child_hash, sizeof r.child_manifest_hash);
	r.logical_offset = logical_offset;
	if (body_append(b, &r, sizeof r) != 0) return TESSERA_ENOMEM;

	b->entry_count++;
	if (logical_offset > b->logical_size) b->logical_size = logical_offset;
	return TESSERA_OK;
}

int
tessera_manifest_set_logical_size(tessera_manifest_builder_t *b,
                                  uint64_t logical_size)
{
	if (b == NULL) return TESSERA_EINVAL;
	/* Only meaningful for CHUNK_TREE — other kinds derive size
	 * from their entry contents. Silently ignore for those. */
	if (b->kind != TESSERA_MFT_CHUNK_TREE) return TESSERA_OK;
	b->logical_size = logical_size;
	return TESSERA_OK;
}

int
tessera_manifest_set_inline(tessera_manifest_builder_t *b,
                            const uint8_t *data, size_t len)
{
	if (b == NULL || (data == NULL && len > 0)) return TESSERA_EINVAL;
	if (b->kind != TESSERA_MFT_INLINE) return TESSERA_EINVAL;
	if (b->body_len != 0) return TESSERA_EINVAL;     /* already set */
	if (body_append(b, data, len) != 0) return TESSERA_ENOMEM;
	b->entry_count = (uint32_t)len;
	b->logical_size = len;
	return TESSERA_OK;
}

int
tessera_manifest_set_symlink(tessera_manifest_builder_t *b, const char *target)
{
	if (b == NULL || target == NULL) return TESSERA_EINVAL;
	if (b->kind != TESSERA_MFT_SYMLINK) return TESSERA_EINVAL;
	if (b->body_len != 0) return TESSERA_EINVAL;
	size_t n = strlen(target);
	if (body_append(b, target, n) != 0) return TESSERA_ENOMEM;
	b->entry_count = (uint32_t)n;
	b->logical_size = n;
	return TESSERA_OK;
}

/* DIRECTORY_2L bucket entries — kept sorted by first_name_hash.
 * Builder API that the kmod's dir-promotion path emits. */
int
tessera_manifest_add_dir_bucket(tessera_manifest_builder_t *b,
                                uint64_t first_name_hash,
                                const tessera_hash_t bucket_manifest_hash)
{
	if (b == NULL || bucket_manifest_hash == NULL)
		return TESSERA_EINVAL;
	if (b->kind != TESSERA_MFT_DIRECTORY_2L) return TESSERA_EINVAL;

	tessera_dir_bucket_record_t r;
	memset(&r, 0, sizeof r);
	r.first_name_hash = first_name_hash;
	memcpy(r.bucket_manifest_hash, bucket_manifest_hash,
	    sizeof r.bucket_manifest_hash);

	/* Insertion-sort by first_name_hash. Bucket counts are bounded
	 * (~few thousand for the dir sizes that justify promotion), so
	 * O(N) per insert is fine here too. */
	size_t pos = 0;
	while (pos < b->body_len) {
		tessera_dir_bucket_record_t cur;
		memcpy(&cur, b->body + pos, sizeof cur);
		if (cur.first_name_hash == first_name_hash)
			return TESSERA_EEXIST;
		if (cur.first_name_hash > first_name_hash) break;
		pos += sizeof cur;
	}
	if (body_reserve(b, b->body_len + sizeof r) != 0)
		return TESSERA_ENOMEM;
	memmove(b->body + pos + sizeof r, b->body + pos,
	    b->body_len - pos);
	memcpy(b->body + pos, &r, sizeof r);
	b->body_len += sizeof r;
	b->entry_count++;
	return TESSERA_OK;
}

uint64_t
tessera_dir_name_hash(const char *name, size_t name_len)
{
	if (name == NULL || name_len == 0) return 0;
	tessera_hash_t h;
	tessera_sha256((const uint8_t *)name, name_len, h);
	uint64_t v = 0;
	for (int i = 0; i < 8; i++)
		v = (v << 8) | h[i];
	return v;
}

/* Directory entries kept sorted by name (memcmp) — enables future
 * binary-search lookup at the parser. Insertion sort here is O(N) per
 * add; fine for typical directory sizes (median sub-100 entries). For
 * giant directories the builder may want a deferred-sort API; that's
 * a Phase-2 concern. */
int
tessera_manifest_add_dirent(tessera_manifest_builder_t *b,
                            uint64_t child_inode,
                            const char *name, size_t name_len)
{
	if (b == NULL || name == NULL) return TESSERA_EINVAL;
	if (b->kind != TESSERA_MFT_DIRECTORY) return TESSERA_EINVAL;
	if (name_len == 0 || name_len > TESSERA_PATH_NAME_MAX)
		return TESSERA_EINVAL;
	for (size_t i = 0; i < name_len; i++)
		if (name[i] == '/' || name[i] == '\0') return TESSERA_EINVAL;

	const size_t hdr_size = 8 + 2;
	const size_t add_size = hdr_size + name_len;

	size_t pos = 0;
	while (pos < b->body_len) {
		uint16_t nl;
		memcpy(&nl, b->body + pos + 8, 2);
		const uint8_t *nm = b->body + pos + hdr_size;
		size_t cmp_len = nl < name_len ? nl : name_len;
		int c = memcmp(nm, name, cmp_len);
		if (c == 0 && nl == name_len) return TESSERA_EEXIST;
		if (c > 0 || (c == 0 && nl > name_len)) break;
		pos += hdr_size + nl;
	}

	if (body_reserve(b, b->body_len + add_size) != 0) return TESSERA_ENOMEM;
	memmove(b->body + pos + add_size, b->body + pos, b->body_len - pos);
	memcpy(b->body + pos, &child_inode, 8);
	uint16_t nl16 = (uint16_t)name_len;
	memcpy(b->body + pos + 8, &nl16, 2);
	memcpy(b->body + pos + hdr_size, name, name_len);
	b->body_len += add_size;
	b->entry_count++;
	return TESSERA_OK;
}

/* XATTR_STORE (tessera-vfs §6.1) — body is a stream of entries, kept
 * SORTED by name for stable listxattr + O(log n)-ish lookup. Each entry:
 *   [u16 name_len][u16 value_len][name bytes][value bytes]
 * v1 stores values inline (≤4096); larger-value blob-hash form is later.
 * add replaces an entry with the same name. */
int
tessera_manifest_add_xattr(tessera_manifest_builder_t *b,
                           const char *name, size_t name_len,
                           const uint8_t *value, size_t value_len)
{
	if (b == NULL || name == NULL) return TESSERA_EINVAL;
	if (b->kind != TESSERA_MFT_XATTR_STORE) return TESSERA_EINVAL;
	if (name_len == 0 || name_len > TESSERA_XATTR_NAME_MAX)
		return TESSERA_EINVAL;
	if (value_len > 4096) return TESSERA_ETOOBIG;   /* v1: inline only */
	if (value_len > 0 && value == NULL) return TESSERA_EINVAL;

	const size_t hdr = 4;   /* u16 name_len + u16 value_len */

	/* Find the sorted insertion point; if the name already exists, splice
	 * the old entry out first (replace semantics). */
	size_t pos = 0;
	while (pos < b->body_len) {
		uint16_t nl, vl;
		memcpy(&nl, b->body + pos, 2);
		memcpy(&vl, b->body + pos + 2, 2);
		const uint8_t *nm = b->body + pos + hdr;
		size_t cmp_len = nl < name_len ? nl : name_len;
		int c = memcmp(nm, name, cmp_len);
		if (c == 0 && nl == name_len) {
			size_t old = hdr + nl + vl;
			memmove(b->body + pos, b->body + pos + old,
			    b->body_len - pos - old);
			b->body_len -= old;
			b->entry_count--;
			break;   /* pos is now the insertion point */
		}
		if (c > 0 || (c == 0 && nl > name_len)) break;
		pos += hdr + nl + vl;
	}

	const size_t add = hdr + name_len + value_len;
	if (body_reserve(b, b->body_len + add) != 0) return TESSERA_ENOMEM;
	memmove(b->body + pos + add, b->body + pos, b->body_len - pos);
	uint16_t nl16 = (uint16_t)name_len, vl16 = (uint16_t)value_len;
	memcpy(b->body + pos, &nl16, 2);
	memcpy(b->body + pos + 2, &vl16, 2);
	memcpy(b->body + pos + hdr, name, name_len);
	if (value_len > 0)
		memcpy(b->body + pos + hdr + name_len, value, value_len);
	b->body_len += add;
	b->entry_count++;
	return TESSERA_OK;
}

/* DIRECTORY_BTREE — body layout is [u8 leaf_flag][u8 reserved×3]
 * [u32 reserved] then a stream of records. Builder appends bytes
 * verbatim; caller is responsible for adding records in ascending
 * key order (split / migration paths already iterate sorted, so
 * this is cheap). */
int
tessera_manifest_dir_btree_set_leaf(tessera_manifest_builder_t *b, int leaf_flag)
{
	if (b == NULL) return TESSERA_EINVAL;
	if (b->kind != TESSERA_MFT_DIRECTORY_BTREE) return TESSERA_EINVAL;
	if (b->body_len > 0) return TESSERA_EINVAL;  /* must be first call */
	uint8_t hdr[8] = { (uint8_t)(leaf_flag ? 1 : 0), 0, 0, 0, 0, 0, 0, 0 };
	if (body_append(b, hdr, sizeof hdr) != 0) return TESSERA_ENOMEM;
	return TESSERA_OK;
}

int
tessera_manifest_dir_btree_add_leaf(tessera_manifest_builder_t *b,
                                    uint64_t name_hash, uint64_t inode_no,
                                    const char *name, size_t name_len)
{
	if (b == NULL || name == NULL) return TESSERA_EINVAL;
	if (b->kind != TESSERA_MFT_DIRECTORY_BTREE) return TESSERA_EINVAL;
	if (b->body_len < 8) return TESSERA_EINVAL;  /* set_leaf first */
	if (b->body[0] != 1) return TESSERA_EINVAL;
	if (name_len == 0 || name_len > TESSERA_PATH_NAME_MAX)
		return TESSERA_EINVAL;
	if (body_reserve(b, b->body_len + 8 + 8 + 2 + name_len) != 0)
		return TESSERA_ENOMEM;
	memcpy(b->body + b->body_len, &name_hash, 8); b->body_len += 8;
	memcpy(b->body + b->body_len, &inode_no, 8);  b->body_len += 8;
	uint16_t nl = (uint16_t)name_len;
	memcpy(b->body + b->body_len, &nl, 2);        b->body_len += 2;
	memcpy(b->body + b->body_len, name, name_len); b->body_len += name_len;
	b->entry_count++;
	return TESSERA_OK;
}

int
tessera_manifest_dir_btree_add_inner(tessera_manifest_builder_t *b,
                                     uint64_t max_name_hash,
                                     const tessera_hash_t child_hash)
{
	if (b == NULL || child_hash == NULL) return TESSERA_EINVAL;
	if (b->kind != TESSERA_MFT_DIRECTORY_BTREE) return TESSERA_EINVAL;
	if (b->body_len < 8) return TESSERA_EINVAL;
	if (b->body[0] != 0) return TESSERA_EINVAL;
	if (body_reserve(b, b->body_len + 8 + TESSERA_HASH_SIZE) != 0)
		return TESSERA_ENOMEM;
	memcpy(b->body + b->body_len, &max_name_hash, 8); b->body_len += 8;
	memcpy(b->body + b->body_len, child_hash, TESSERA_HASH_SIZE);
	b->body_len += TESSERA_HASH_SIZE;
	b->entry_count++;
	return TESSERA_OK;
}

int
tessera_manifest_finalize(tessera_manifest_builder_t *b,
                          uint8_t *out_buffer, size_t buffer_len,
                          size_t *out_size, tessera_hash_t out_hash)
{
	if (b == NULL || out_size == NULL) return TESSERA_EINVAL;

	const size_t total = HEADER_SIZE + b->body_len;
	*out_size = total;
	if (out_buffer == NULL || buffer_len < total) return TESSERA_ETOOBIG;

	tessera_manifest_header_t h;
	memset(&h, 0, sizeof h);
	memcpy(h.magic, TESSERA_MAGIC_MANIFEST, 4);
	h.version        = 1;
	h.manifest_kind  = (uint8_t)b->kind;
	h.level          = 0;
	h.logical_size   = b->logical_size;
	h.chunk_size_avg = 0;
	h.entry_count    = b->entry_count;
	int r = tessera_encode_manifest_header(&h, out_buffer);
	if (r != TESSERA_OK) return r;

	if (b->body_len > 0)
		memcpy(out_buffer + HEADER_SIZE, b->body, b->body_len);

	if (out_hash != NULL)
		tessera_sha256(out_buffer, total, out_hash);

	return TESSERA_OK;
}

/* ── Parser ──────────────────────────────────────────────────────── */

struct tessera_manifest_parser {
	tessera_manifest_header_t  header;
	const uint8_t             *body;
	size_t                     body_len;
};

tessera_manifest_parser_t *
tessera_manifest_parse(const uint8_t *data, size_t len)
{
	if (data == NULL || len < HEADER_SIZE) return NULL;
	tessera_manifest_parser_t *p = tessera_zalloc(sizeof *p);
	if (p == NULL) return NULL;
	if (tessera_decode_manifest_header(data, &p->header) != TESSERA_OK) {
		tessera_free(p);
		return NULL;
	}
	p->body     = data + HEADER_SIZE;
	p->body_len = len - HEADER_SIZE;
	return p;
}

tessera_manifest_kind_t
tessera_manifest_parser_kind(const tessera_manifest_parser_t *p)
{
	return p ? (tessera_manifest_kind_t)p->header.manifest_kind : 0;
}

uint64_t
tessera_manifest_parser_size(const tessera_manifest_parser_t *p)
{
	return p ? p->header.logical_size : 0;
}

uint32_t
tessera_manifest_parser_count(const tessera_manifest_parser_t *p)
{
	return p ? p->header.entry_count : 0;
}

int
tessera_manifest_chunk_at(const tessera_manifest_parser_t *p,
                          uint32_t index, tessera_chunk_record_t *out)
{
	if (p == NULL || out == NULL) return TESSERA_EINVAL;
	if (p->header.manifest_kind != TESSERA_MFT_CHUNK_LIST)
		return TESSERA_EINVAL;
	if (index >= p->header.entry_count) return TESSERA_ENOENT;
	const size_t off = (size_t)index * sizeof *out;
	if (off + sizeof *out > p->body_len) return TESSERA_ECORRUPT;
	memcpy(out, p->body + off, sizeof *out);
	return TESSERA_OK;
}

int
tessera_manifest_tree_at(const tessera_manifest_parser_t *p,
                         uint32_t index, tessera_tree_record_t *out)
{
	if (p == NULL || out == NULL) return TESSERA_EINVAL;
	if (p->header.manifest_kind != TESSERA_MFT_CHUNK_TREE)
		return TESSERA_EINVAL;
	if (index >= p->header.entry_count) return TESSERA_ENOENT;
	const size_t off = (size_t)index * sizeof *out;
	if (off + sizeof *out > p->body_len) return TESSERA_ECORRUPT;
	memcpy(out, p->body + off, sizeof *out);
	return TESSERA_OK;
}

int
tessera_manifest_dir_bucket_at(const tessera_manifest_parser_t *p,
                               uint32_t index,
                               tessera_dir_bucket_record_t *out)
{
	if (p == NULL || out == NULL) return TESSERA_EINVAL;
	if (p->header.manifest_kind != TESSERA_MFT_DIRECTORY_2L)
		return TESSERA_EINVAL;
	if (index >= p->header.entry_count) return TESSERA_ENOENT;
	const size_t off = (size_t)index * sizeof *out;
	if (off + sizeof *out > p->body_len) return TESSERA_ECORRUPT;
	memcpy(out, p->body + off, sizeof *out);
	return TESSERA_OK;
}

int
tessera_manifest_inline_data(const tessera_manifest_parser_t *p,
                             const uint8_t **out_data, size_t *out_len)
{
	if (p == NULL || out_data == NULL || out_len == NULL)
		return TESSERA_EINVAL;
	if (p->header.manifest_kind != TESSERA_MFT_INLINE &&
	    p->header.manifest_kind != TESSERA_MFT_SYMLINK)
		return TESSERA_EINVAL;
	*out_data = p->body;
	*out_len  = p->body_len;
	return TESSERA_OK;
}

/* Read the idx-th xattr entry (sorted by name). The out name and value
 * pointers reference the parser's body (valid for the parser's life); their
 * lengths come back via the len out-params. TESSERA_ENOENT past the end,
 * TESSERA_ECORRUPT on a truncated record. */
int
tessera_manifest_xattr_at(const tessera_manifest_parser_t *p, uint32_t idx,
                          const char **out_name, uint16_t *out_name_len,
                          const uint8_t **out_value, uint16_t *out_value_len)
{
	if (p == NULL) return TESSERA_EINVAL;
	if (p->header.manifest_kind != TESSERA_MFT_XATTR_STORE)
		return TESSERA_EINVAL;
	size_t pos = 0;
	for (uint32_t i = 0; pos < p->body_len; i++) {
		if (pos + 4 > p->body_len) return TESSERA_ECORRUPT;
		uint16_t nl, vl;
		memcpy(&nl, p->body + pos, 2);
		memcpy(&vl, p->body + pos + 2, 2);
		if (pos + 4 + (size_t)nl + (size_t)vl > p->body_len)
			return TESSERA_ECORRUPT;
		if (i == idx) {
			if (out_name)      *out_name = (const char *)(p->body + pos + 4);
			if (out_name_len)  *out_name_len = nl;
			if (out_value)     *out_value = p->body + pos + 4 + nl;
			if (out_value_len) *out_value_len = vl;
			return TESSERA_OK;
		}
		pos += 4 + (size_t)nl + (size_t)vl;
	}
	return TESSERA_ENOENT;
}

/* DIRECTORY_BTREE leaf flag: 1 = leaf (name→inode records), 0 = inner
 * (max_name_hash→child-manifest records). -1 if not a DIRECTORY_BTREE or
 * the body is too short to hold the 8-byte header. */
int
tessera_manifest_dir_btree_is_leaf(const tessera_manifest_parser_t *p)
{
	if (p == NULL || p->header.manifest_kind != TESSERA_MFT_DIRECTORY_BTREE)
		return -1;
	if (p->body_len < 8) return -1;
	return p->body[0] ? 1 : 0;
}

/* Read the idx-th child manifest hash from an INNER DIRECTORY_BTREE node.
 * Inner records are [u64 max_name_hash][32B child_hash] after the 8-byte
 * body header. TESSERA_ENOENT past the end; TESSERA_EINVAL if this isn't an
 * inner DIRECTORY_BTREE node. (Leaf nodes hold name→inode entries, not blob
 * refs — iterate those with a future leaf accessor.) */
int
tessera_manifest_dir_btree_inner_at(const tessera_manifest_parser_t *p,
                                    uint32_t idx, tessera_hash_t out_child)
{
	if (p == NULL || out_child == NULL) return TESSERA_EINVAL;
	if (p->header.manifest_kind != TESSERA_MFT_DIRECTORY_BTREE)
		return TESSERA_EINVAL;
	if (p->body_len < 8 || p->body[0] != 0) return TESSERA_EINVAL;
	size_t off = 8 + (size_t)idx * (8 + TESSERA_HASH_SIZE);
	if (off + 8 + TESSERA_HASH_SIZE > p->body_len) return TESSERA_ENOENT;
	memcpy(out_child, p->body + off + 8, TESSERA_HASH_SIZE);
	return TESSERA_OK;
}

void
tessera_manifest_parser_free(tessera_manifest_parser_t *p)
{
	tessera_free(p);
}
