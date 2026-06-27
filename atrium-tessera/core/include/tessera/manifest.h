/*
 * tessera/manifest.h — manifest building and parsing.
 *
 * Builds (kind, body) → bytes; parses bytes → kind + iterator.
 * Manifest hash is SHA-256 over the encoded bytes.
 */

#ifndef TESSERA_MANIFEST_H_
#define TESSERA_MANIFEST_H_

#include "tessera/format.h"

#ifdef __cplusplus
extern "C" {
#endif

typedef struct tessera_manifest_builder tessera_manifest_builder_t;

tessera_manifest_builder_t *tessera_manifest_begin(tessera_manifest_kind_t kind);

/* CHUNK_LIST entries. */
int tessera_manifest_add_chunk(tessera_manifest_builder_t *,
                               const tessera_hash_t chunk_hash,
                               uint64_t logical_offset,
                               uint32_t size,
                               uint32_t flags);

/* CHUNK_TREE entries. */
int tessera_manifest_add_tree_child(tessera_manifest_builder_t *,
                                    const tessera_hash_t child_hash,
                                    uint64_t logical_offset);

/* Override the builder's logical_size. Required for CHUNK_TREE so the
 * read path can compute the last child's exclusive upper bound — the
 * builder otherwise advances logical_size only to the last child's
 * offset (it has no inherent knowledge of the child's own extent).
 * No-op for kinds that derive logical_size from contents (INLINE,
 * SYMLINK, CHUNK_LIST). */
int tessera_manifest_set_logical_size(tessera_manifest_builder_t *,
                                      uint64_t logical_size);

/* INLINE body. */
int tessera_manifest_set_inline(tessera_manifest_builder_t *,
                                const uint8_t *data, size_t len);

/* SYMLINK target. */
int tessera_manifest_set_symlink(tessera_manifest_builder_t *,
                                 const char *target);

/* DIRECTORY entries. */
int tessera_manifest_add_dirent(tessera_manifest_builder_t *,
                                uint64_t child_inode,
                                const char *name, size_t name_len);

/* XATTR_STORE entries (tessera-vfs §6.1). Sorted by name; add replaces a
 * same-named entry. Values are inline (≤4096 in v1). */
int tessera_manifest_add_xattr(tessera_manifest_builder_t *,
                               const char *name, size_t name_len,
                               const uint8_t *value, size_t value_len);

/* DIRECTORY_2L entries — outer-manifest bucket descriptors. */
int tessera_manifest_add_dir_bucket(tessera_manifest_builder_t *,
                                    uint64_t first_name_hash,
                                    const tessera_hash_t bucket_manifest_hash);

/* DIRECTORY_BTREE — append-the-prefix to set the leaf flag, then
 * append records. Records MUST be added in ascending key order; the
 * caller is expected to have sorted entries by name_hash before
 * calling these. (Builder doesn't sort to keep the per-op cost low
 * for the common-case bulk-build during migration / split.) */
int tessera_manifest_dir_btree_set_leaf(tessera_manifest_builder_t *,
                                        int leaf_flag);
int tessera_manifest_dir_btree_add_leaf(tessera_manifest_builder_t *,
                                        uint64_t name_hash,
                                        uint64_t inode_no,
                                        const char *name, size_t name_len);
int tessera_manifest_dir_btree_add_inner(tessera_manifest_builder_t *,
                                         uint64_t max_name_hash,
                                         const tessera_hash_t child_hash);

/* SHA-256 over `name`, return first 8 bytes as a u64 — the hash key
 * used by DIRECTORY_2L bucket selection. Stable + uniformly
 * distributed; cost is microseconds even for long names. */
uint64_t tessera_dir_name_hash(const char *name, size_t name_len);

/*
 * Encode + hash. Caller provides `out_buffer`; returns the encoded
 * size in *out_size and the manifest hash in `out_hash`.
 *
 * Returns TESSERA_ETOOBIG if buffer is too small (re-call with the
 * suggested size from *out_size).
 */
int tessera_manifest_finalize(tessera_manifest_builder_t *,
                              uint8_t *out_buffer, size_t buffer_len,
                              size_t *out_size, tessera_hash_t out_hash);

void tessera_manifest_free(tessera_manifest_builder_t *);

/* ── Parsing ─────────────────────────────────────────────────────── */

typedef struct tessera_manifest_parser tessera_manifest_parser_t;

tessera_manifest_parser_t *tessera_manifest_parse(const uint8_t *data, size_t len);
tessera_manifest_kind_t   tessera_manifest_parser_kind(const tessera_manifest_parser_t *);
uint64_t                  tessera_manifest_parser_size(const tessera_manifest_parser_t *);
uint32_t                  tessera_manifest_parser_count(const tessera_manifest_parser_t *);

/* Iterate chunk records from a CHUNK_LIST manifest. */
int tessera_manifest_chunk_at(const tessera_manifest_parser_t *,
                              uint32_t index,
                              tessera_chunk_record_t *out);

/* Iterate tree records from a CHUNK_TREE manifest. */
int tessera_manifest_tree_at(const tessera_manifest_parser_t *,
                             uint32_t index,
                             tessera_tree_record_t *out);

/* Iterate bucket records from a DIRECTORY_2L manifest. */
int tessera_manifest_dir_bucket_at(const tessera_manifest_parser_t *,
                                   uint32_t index,
                                   tessera_dir_bucket_record_t *out);

/* Read inline content from an INLINE manifest. */
int tessera_manifest_inline_data(const tessera_manifest_parser_t *,
                                 const uint8_t **out_data, size_t *out_len);

/* Read the idx-th xattr entry from an XATTR_STORE manifest (sorted by name);
 * out pointers reference the parser body. ENOENT past the end. */
int tessera_manifest_xattr_at(const tessera_manifest_parser_t *, uint32_t idx,
                              const char **out_name, uint16_t *out_name_len,
                              const uint8_t **out_value, uint16_t *out_value_len);

/* DIRECTORY_BTREE: 1 = leaf, 0 = inner, -1 if not a DIRECTORY_BTREE. */
int tessera_manifest_dir_btree_is_leaf(const tessera_manifest_parser_t *);

/* Read the idx-th child manifest hash from an INNER DIRECTORY_BTREE node.
 * ENOENT past the end; EINVAL if not an inner DIRECTORY_BTREE node. */
int tessera_manifest_dir_btree_inner_at(const tessera_manifest_parser_t *,
                                        uint32_t idx, tessera_hash_t out_child);

void tessera_manifest_parser_free(tessera_manifest_parser_t *);

#ifdef __cplusplus
}
#endif

#endif /* TESSERA_MANIFEST_H_ */
