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

#include <stdlib.h>
#include <string.h>

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
	uint8_t *p = realloc(b->body, cap);
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
	tessera_manifest_builder_t *b = calloc(1, sizeof *b);
	if (b == NULL) return NULL;
	b->kind = kind;
	return b;
}

void
tessera_manifest_free(tessera_manifest_builder_t *b)
{
	if (b == NULL) return;
	free(b->body);
	free(b);
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
	tessera_manifest_parser_t *p = calloc(1, sizeof *p);
	if (p == NULL) return NULL;
	if (tessera_decode_manifest_header(data, &p->header) != TESSERA_OK) {
		free(p);
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

void
tessera_manifest_parser_free(tessera_manifest_parser_t *p)
{
	free(p);
}
