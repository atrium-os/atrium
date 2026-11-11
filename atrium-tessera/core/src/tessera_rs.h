/*
 * tessera_rs.h — the Rust on-disk decoders (rs/tessera-decode, src/ffi.rs).
 *
 * Built with -DTESSERA_RUST_DECODE (the kernel module always is), the C
 * entry points in codec.c, manifest.c, pack.c, btree.c and journal.c keep
 * their names and signatures but hand every byte of on-disk input to these
 * functions. Without it they keep their C bodies: that is the loader's build
 * (libsa cannot link Rust yet) and the reference side of the differential
 * test (fuzz/fuzz_diff.c). Kernel security review 2026-11-07, decision D3.
 *
 * Contract for every function: input pointers are NULL or point to the
 * stated number of readable bytes; out-pointers are NULL or writable.
 * Internal-only, like tessera_compat.h.
 *
 * SPDX-License-Identifier: BSD-2-Clause
 */

#ifndef TESSERA_RS_H_
#define TESSERA_RS_H_

#ifdef TESSERA_KERNEL
#  include <sys/types.h>
#else
#  include <stddef.h>
#  include <stdint.h>
#endif

/* codec.h: validate, then copy into the packed struct. Same return codes. */
int tessera_rs_decode_superblock(const uint8_t *in, void *out);
int tessera_rs_decode_journal_header(const uint8_t *in, void *out);
int tessera_rs_decode_record_header(const uint8_t *in, void *out);
int tessera_rs_decode_pack_header(const uint8_t *in, void *out);
int tessera_rs_decode_pack_footer(const uint8_t *in, void *out);
int tessera_rs_decode_pack_index_entry(const uint8_t *in, void *out);
int tessera_rs_decode_blob_descriptor(const uint8_t *in, void *out);
int tessera_rs_decode_manifest_header(const uint8_t *in, void *out);
int tessera_rs_decode_inode(const uint8_t *in, void *out);
int tessera_rs_decode_quota_domain(const uint8_t *in, void *out);
int tessera_rs_decode_btree_node_header(const uint8_t *in, void *out);
int tessera_rs_decode_registry_entry(const uint8_t *in, void *out);
int tessera_rs_decode_pack_extent_list(const uint8_t *in, void *out);
int tessera_rs_decode_free_extent(const uint8_t *in, void *out);

/* Field offsets and sizes the Rust side assumes, in codec::LAYOUT order;
 * SIZE_MAX past the end. Checked against offsetof() by fuzz_diff.c. */
size_t tessera_rs_layout(size_t i);

/* manifest.h. The parser object keeps the 32-byte header and the body;
 * every accessor takes them back. */
int tessera_rs_manifest_parse(const uint8_t *data, size_t len, void *out_hdr);
int tessera_rs_manifest_chunk_at(const void *hdr, const uint8_t *body,
    size_t body_len, uint32_t index, void *out);
int tessera_rs_manifest_tree_at(const void *hdr, const uint8_t *body,
    size_t body_len, uint32_t index, void *out);
int tessera_rs_manifest_dir_bucket_at(const void *hdr, const uint8_t *body,
    size_t body_len, uint32_t index, void *out);
int tessera_rs_manifest_dir_btree_inner_at(const void *hdr,
    const uint8_t *body, size_t body_len, uint32_t index, uint8_t *out);
int tessera_rs_manifest_inline_data(const void *hdr, const uint8_t *body,
    size_t body_len, const uint8_t **out_data, size_t *out_len);
int tessera_rs_manifest_xattr_at(const void *hdr, const uint8_t *body,
    size_t body_len, uint32_t idx, const char **out_name,
    uint16_t *out_name_len, const uint8_t **out_value,
    uint16_t *out_value_len);
int tessera_rs_manifest_dir_btree_is_leaf(const void *hdr,
    const uint8_t *body, size_t body_len);
int tessera_rs_manifest_dirent_at(const void *hdr, const uint8_t *body,
    size_t body_len, uint32_t idx, uint64_t *out_inode,
    const char **out_name, uint16_t *out_name_len);
int tessera_rs_manifest_dir_btree_leaf_at(const void *hdr,
    const uint8_t *body, size_t body_len, uint32_t idx, uint64_t *out_inode,
    const char **out_name, uint16_t *out_name_len);
/*
 * Whole-node check for a directory B-tree node blob (manifest header + body).
 * A leaf must hold all `count` entries inside the body; an inner node needs
 * count >= 1 and count * 40 <= body length. On success *out_entries and *out_len
 * describe the body and *out_count the entry count.
 */
int tessera_rs_dir_btree_node(const uint8_t *blob, size_t len, int *out_leaf,
    const uint8_t **out_entries, size_t *out_len, uint32_t *out_count);

/* pack.h. Offsets of the validated regions (pack::Geometry, #[repr(C)]). */
typedef struct {
	uint64_t index_off;
	uint64_t bloom_off;
	uint64_t bloom_bytes;
	uint64_t data_off;
	uint64_t data_len;
	uint32_t blob_count;
	uint32_t k_hashes;
} tessera_rs_pack_geom_t;

int tessera_rs_pack_open(const uint8_t *data, size_t len,
    tessera_rs_pack_geom_t *out_geom, void *out_hdr);
int tessera_rs_pack_geom_from_header(const uint8_t *hdr, uint64_t pack_len,
    tessera_rs_pack_geom_t *out_geom);
int tessera_rs_pack_index_find(const uint8_t *idx, size_t idx_len,
    const tessera_rs_pack_geom_t *geom, const uint8_t *hash,
    uint64_t *out_desc_off, uint32_t *out_size);
int tessera_rs_pack_bloom_might_contain(const uint8_t *data, size_t len,
    const tessera_rs_pack_geom_t *geom, const uint8_t *hash);
int tessera_rs_pack_lookup(const uint8_t *data, size_t len,
    const tessera_rs_pack_geom_t *geom, const uint8_t *hash,
    const uint8_t **out_bytes, uint32_t *out_len);
int tessera_rs_pack_blob_hash_at(const uint8_t *data, size_t len,
    const tessera_rs_pack_geom_t *geom, uint32_t index, uint8_t *out);

/* btree.c: load_node's checks (OK or ECORRUPT, *out_fail a
 * tessera_btree_fail_t) and search_leaf. */
int tessera_rs_btree_node_check(const uint8_t *block, size_t block_len,
    uint8_t tree_kind, uint32_t key_size, uint32_t value_size,
    uint32_t leaf_fanout, uint32_t inner_fanout,
    void *out_hdr, int *out_fail, uint8_t *out_found_kind);
int tessera_rs_btree_search(const uint8_t *block, size_t block_len,
    uint32_t entry_count, uint32_t key_size, uint32_t entry_size,
    const void *key, int *out_exact);

/* journal.c: read_record's header and geometry checks, and the ring step. */
int tessera_rs_journal_record_check(const uint8_t *blk, uint64_t cap,
    void *out_hdr);
uint64_t tessera_rs_journal_advance(uint64_t block, uint32_t block_count,
    uint64_t cap);

#endif /* TESSERA_RS_H_ */
