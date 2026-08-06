/*
 * tessera/codec.h — encode/decode of on-disk structs.
 *
 * Pure functions over byte buffers. No I/O. The encode functions
 * produce little-endian bytes from the in-memory struct; the decode
 * functions parse and validate. Validation includes magic-string
 * checks and CRC verification where applicable.
 */

#ifndef TESSERA_CODEC_H_
#define TESSERA_CODEC_H_

#include "tessera/format.h"

#ifdef __cplusplus
extern "C" {
#endif

/* Superblock: encode + decode + verify. */
int tessera_encode_superblock(const tessera_superblock_t *in,
                              uint8_t out[TESSERA_SECTOR_SIZE]);
int tessera_decode_superblock(const uint8_t in[TESSERA_SECTOR_SIZE],
                              tessera_superblock_t *out);

/* Journal header. */
int tessera_encode_journal_header(const tessera_journal_header_t *in,
                                  uint8_t out[TESSERA_SECTOR_SIZE]);
int tessera_decode_journal_header(const uint8_t in[TESSERA_SECTOR_SIZE],
                                  tessera_journal_header_t *out);

/* Journal record header (32 bytes). */
int tessera_encode_record_header(const tessera_record_header_t *in,
                                 uint8_t out[TESSERA_RECORD_HEADER_SIZE]);
int tessera_decode_record_header(const uint8_t in[TESSERA_RECORD_HEADER_SIZE],
                                 tessera_record_header_t *out);

/* Pack header / footer / index entry / blob descriptor. */
int tessera_encode_pack_header(const tessera_pack_header_t *in,
                               uint8_t out[TESSERA_SECTOR_SIZE]);
int tessera_decode_pack_header(const uint8_t in[TESSERA_SECTOR_SIZE],
                               tessera_pack_header_t *out);

int tessera_encode_pack_footer(const tessera_pack_footer_t *in,
                               uint8_t out[TESSERA_SECTOR_SIZE]);
int tessera_decode_pack_footer(const uint8_t in[TESSERA_SECTOR_SIZE],
                               tessera_pack_footer_t *out);

int tessera_encode_pack_index_entry(const tessera_pack_index_entry_t *in,
                                    uint8_t out[TESSERA_PACK_INDEX_ENTRY_SIZE]);
int tessera_decode_pack_index_entry(const uint8_t in[TESSERA_PACK_INDEX_ENTRY_SIZE],
                                    tessera_pack_index_entry_t *out);

int tessera_encode_blob_descriptor(const tessera_blob_descriptor_t *in,
                                   uint8_t out[TESSERA_BLOB_DESCRIPTOR_SIZE]);
int tessera_decode_blob_descriptor(const uint8_t in[TESSERA_BLOB_DESCRIPTOR_SIZE],
                                   tessera_blob_descriptor_t *out);

/* Manifest header (32 bytes; payload follows). */
int tessera_encode_manifest_header(const tessera_manifest_header_t *in,
                                   uint8_t out[TESSERA_MANIFEST_HEADER_SIZE]);
int tessera_decode_manifest_header(const uint8_t in[TESSERA_MANIFEST_HEADER_SIZE],
                                   tessera_manifest_header_t *out);

/* Inode record. */
int tessera_encode_inode(const tessera_inode_record_t *in,
                         uint8_t out[TESSERA_INODE_RECORD_SIZE]);
int tessera_decode_inode(const uint8_t in[TESSERA_INODE_RECORD_SIZE],
                         tessera_inode_record_t *out);

/* Quota domain record (128 bytes; tessera-quotas.md §4.2). */
int tessera_encode_quota_domain(const tessera_quota_domain_t *in,
                                uint8_t out[TESSERA_QUOTA_DOMAIN_SIZE]);
int tessera_decode_quota_domain(const uint8_t in[TESSERA_QUOTA_DOMAIN_SIZE],
                                tessera_quota_domain_t *out);

/* B+tree node header (64 bytes; entries follow). */
int tessera_encode_btree_node_header(const tessera_btree_node_header_t *in,
                                     uint8_t out[TESSERA_BTREE_NODE_HEADER_SIZE]);
int tessera_decode_btree_node_header(const uint8_t in[TESSERA_BTREE_NODE_HEADER_SIZE],
                                     tessera_btree_node_header_t *out);

/* Pack registry entry. */
int tessera_encode_registry_entry(const tessera_registry_entry_t *in,
                                  uint8_t out[TESSERA_REGISTRY_ENTRY_SIZE]);
int tessera_decode_registry_entry(const uint8_t in[TESSERA_REGISTRY_ENTRY_SIZE],
                                  tessera_registry_entry_t *out);

/* Pack extent list (PEL) — one sector. Used by registry entries with
 * the MULTI_EXTENT flag set. */
int tessera_encode_pack_extent_list(const tessera_pack_extent_list_t *in,
                                    uint8_t out[TESSERA_SECTOR_SIZE]);
int tessera_decode_pack_extent_list(const uint8_t in[TESSERA_SECTOR_SIZE],
                                    tessera_pack_extent_list_t *out);

/* Free-extent record. */
int tessera_encode_free_extent(const tessera_free_extent_t *in,
                               uint8_t out[TESSERA_EXTENT_ENTRY_SIZE]);
int tessera_decode_free_extent(const uint8_t in[TESSERA_EXTENT_ENTRY_SIZE],
                               tessera_free_extent_t *out);

#ifdef __cplusplus
}
#endif

#endif /* TESSERA_CODEC_H_ */
