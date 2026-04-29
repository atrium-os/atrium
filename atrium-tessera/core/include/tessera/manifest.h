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

/* Read inline content from an INLINE manifest. */
int tessera_manifest_inline_data(const tessera_manifest_parser_t *,
                                 const uint8_t **out_data, size_t *out_len);

void tessera_manifest_parser_free(tessera_manifest_parser_t *);

#ifdef __cplusplus
}
#endif

#endif /* TESSERA_MANIFEST_H_ */
