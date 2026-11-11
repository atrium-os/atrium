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
/* The pack_id recorded in the header of the bytes this reader was opened on.
 * A consumer that reached these bytes via a registry entry must check it
 * against the entry's pack_id: a mismatch is a stale or foreign read. */
const uint8_t *tessera_pack_pack_id(const tessera_pack_reader_t *);
int      tessera_pack_lookup(const tessera_pack_reader_t *,
                             const tessera_hash_t blob_hash,
                             const uint8_t **out_bytes,
                             uint32_t *out_len);

/* Read the i-th blob's hash from the index. Used by GC to enumerate
 * what each pack contains without an O(N) full scan. */
int      tessera_pack_blob_hash_at(const tessera_pack_reader_t *,
                                   uint32_t index, tessera_hash_t out);

/* Bloom filter quick check. Returns 1 if hash *might* be present,
 * 0 if definitely absent. Lookup still required to confirm. */
int tessera_pack_bloom_might_contain(const tessera_pack_reader_t *,
                                     const tessera_hash_t blob_hash);

void tessera_pack_close(tessera_pack_reader_t *);

/* ── Partial reads (the kernel module) ───────────────────────────────
 *
 * The kernel reads a pack's header sector and index without reading the
 * whole pack, so it cannot use tessera_pack_open. These two give it the same
 * checks over what it did read. All offsets are pack-relative bytes.
 */
typedef struct {
	uint64_t index_off;
	uint64_t bloom_off;
	uint64_t bloom_bytes;
	uint64_t data_off;
	uint64_t data_len;
	uint32_t blob_count;
	uint32_t k_hashes;
} tessera_pack_geom_t;

/* Header magic and CRC, then tessera_pack_open's region containment against
 * the size the header claims, which must be at least two sectors and at most
 * pack_len (the bytes the caller has on disk). No footer check. */
int tessera_pack_geom_from_header(const uint8_t hdr[TESSERA_SECTOR_SIZE],
                                  uint64_t pack_len,
                                  tessera_pack_geom_t *out);

/* Binary search of an index the caller read itself: idx holds idx_len bytes
 * starting at geom->index_off. On a hit, the blob's descriptor offset and
 * its data size, both checked to lie inside the data region, so the caller
 * can allocate and read them. TESSERA_ENOENT if absent, TESSERA_ECORRUPT if
 * the index or the entry does not fit. */
int tessera_pack_index_find(const uint8_t *idx, size_t idx_len,
                            const tessera_pack_geom_t *geom,
                            const tessera_hash_t hash,
                            uint64_t *out_desc_off, uint32_t *out_size);

#ifdef __cplusplus
}
#endif

#endif /* TESSERA_PACK_H_ */
