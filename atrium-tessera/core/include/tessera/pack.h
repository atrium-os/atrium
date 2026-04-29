/*
 * tessera/pack.h — pack-file builder and reader.
 *
 * Per tessera-fs §5: header + sorted blob index + bloom filter +
 * blob data area + footer. One-shot writer (used by transactions
 * that publish a new pack); random-access reader (used by lookups).
 */

#ifndef TESSERA_PACK_H_
#define TESSERA_PACK_H_

#include "tessera/format.h"

#ifdef __cplusplus
extern "C" {
#endif

/* ── Pack builder ────────────────────────────────────────────────── */

typedef struct tessera_pack_builder tessera_pack_builder_t;

tessera_pack_builder_t *tessera_pack_begin(uint32_t pack_kind,
                                            const uint8_t pack_id[16],
                                            uint64_t creator_tx_id);

/* Add a blob. Caller-provided bytes are copied into the builder's
 * staging buffer. blob_hash MUST be the correct SHA-256 of bytes;
 * builder does not recompute. */
int tessera_pack_add_blob(tessera_pack_builder_t *,
                          const tessera_hash_t blob_hash,
                          const uint8_t *bytes, uint32_t len,
                          uint32_t flags);

/* Encode the entire pack into `out_buffer`. Returns the total bytes
 * in *out_size, or TESSERA_ETOOBIG if buffer too small. */
int tessera_pack_finalize(tessera_pack_builder_t *,
                          uint8_t *out_buffer, size_t buffer_len,
                          size_t *out_size);

void tessera_pack_free(tessera_pack_builder_t *);

/* ── Pack reader ─────────────────────────────────────────────────── */

typedef struct tessera_pack_reader tessera_pack_reader_t;

tessera_pack_reader_t *tessera_pack_open(const uint8_t *data, size_t len);

uint32_t tessera_pack_blob_count(const tessera_pack_reader_t *);
int      tessera_pack_lookup(const tessera_pack_reader_t *,
                             const tessera_hash_t blob_hash,
                             const uint8_t **out_bytes,
                             uint32_t *out_len);

/* Bloom filter quick check. Returns 1 if hash *might* be present,
 * 0 if definitely absent. Lookup still required to confirm. */
int tessera_pack_bloom_might_contain(const tessera_pack_reader_t *,
                                     const tessera_hash_t blob_hash);

void tessera_pack_close(tessera_pack_reader_t *);

#ifdef __cplusplus
}
#endif

#endif /* TESSERA_PACK_H_ */
