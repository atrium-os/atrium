/*
 * tessera-core: encode/decode of on-disk structs.
 * Phase 1 will fill these in. Phase 0 stubs return TESSERA_ENOTIMPL.
 */

#include "tessera/codec.h"
#include "tessera/error.h"

#define UNUSED(x) ((void)(x))

int
tessera_encode_superblock(const tessera_superblock_t *in,
                          uint8_t out[TESSERA_SECTOR_SIZE])
{
	UNUSED(in); UNUSED(out);
	return TESSERA_ENOTIMPL;
}

int
tessera_decode_superblock(const uint8_t in[TESSERA_SECTOR_SIZE],
                          tessera_superblock_t *out)
{
	UNUSED(in); UNUSED(out);
	return TESSERA_ENOTIMPL;
}

int
tessera_encode_journal_header(const tessera_journal_header_t *in,
                              uint8_t out[TESSERA_SECTOR_SIZE])
{
	UNUSED(in); UNUSED(out);
	return TESSERA_ENOTIMPL;
}

int
tessera_decode_journal_header(const uint8_t in[TESSERA_SECTOR_SIZE],
                              tessera_journal_header_t *out)
{
	UNUSED(in); UNUSED(out);
	return TESSERA_ENOTIMPL;
}

int
tessera_encode_record_header(const tessera_record_header_t *in,
                             uint8_t out[32])
{
	UNUSED(in); UNUSED(out);
	return TESSERA_ENOTIMPL;
}

int
tessera_decode_record_header(const uint8_t in[32],
                             tessera_record_header_t *out)
{
	UNUSED(in); UNUSED(out);
	return TESSERA_ENOTIMPL;
}

int
tessera_encode_pack_header(const tessera_pack_header_t *in,
                           uint8_t out[TESSERA_SECTOR_SIZE])
{
	UNUSED(in); UNUSED(out);
	return TESSERA_ENOTIMPL;
}

int
tessera_decode_pack_header(const uint8_t in[TESSERA_SECTOR_SIZE],
                           tessera_pack_header_t *out)
{
	UNUSED(in); UNUSED(out);
	return TESSERA_ENOTIMPL;
}

int
tessera_encode_pack_footer(const tessera_pack_footer_t *in,
                           uint8_t out[TESSERA_SECTOR_SIZE])
{
	UNUSED(in); UNUSED(out);
	return TESSERA_ENOTIMPL;
}

int
tessera_decode_pack_footer(const uint8_t in[TESSERA_SECTOR_SIZE],
                           tessera_pack_footer_t *out)
{
	UNUSED(in); UNUSED(out);
	return TESSERA_ENOTIMPL;
}

int
tessera_encode_pack_index_entry(const tessera_pack_index_entry_t *in,
                                uint8_t out[TESSERA_PACK_INDEX_ENTRY_SIZE])
{
	UNUSED(in); UNUSED(out);
	return TESSERA_ENOTIMPL;
}

int
tessera_decode_pack_index_entry(const uint8_t in[TESSERA_PACK_INDEX_ENTRY_SIZE],
                                tessera_pack_index_entry_t *out)
{
	UNUSED(in); UNUSED(out);
	return TESSERA_ENOTIMPL;
}

int
tessera_encode_blob_descriptor(const tessera_blob_descriptor_t *in,
                               uint8_t out[16])
{
	UNUSED(in); UNUSED(out);
	return TESSERA_ENOTIMPL;
}

int
tessera_decode_blob_descriptor(const uint8_t in[16],
                               tessera_blob_descriptor_t *out)
{
	UNUSED(in); UNUSED(out);
	return TESSERA_ENOTIMPL;
}

int
tessera_encode_manifest_header(const tessera_manifest_header_t *in,
                               uint8_t out[32])
{
	UNUSED(in); UNUSED(out);
	return TESSERA_ENOTIMPL;
}

int
tessera_decode_manifest_header(const uint8_t in[32],
                               tessera_manifest_header_t *out)
{
	UNUSED(in); UNUSED(out);
	return TESSERA_ENOTIMPL;
}

int
tessera_encode_inode(const tessera_inode_record_t *in,
                     uint8_t out[TESSERA_INODE_RECORD_SIZE])
{
	UNUSED(in); UNUSED(out);
	return TESSERA_ENOTIMPL;
}

int
tessera_decode_inode(const uint8_t in[TESSERA_INODE_RECORD_SIZE],
                     tessera_inode_record_t *out)
{
	UNUSED(in); UNUSED(out);
	return TESSERA_ENOTIMPL;
}

int
tessera_encode_btree_node_header(const tessera_btree_node_header_t *in,
                                 uint8_t out[32])
{
	UNUSED(in); UNUSED(out);
	return TESSERA_ENOTIMPL;
}

int
tessera_decode_btree_node_header(const uint8_t in[32],
                                 tessera_btree_node_header_t *out)
{
	UNUSED(in); UNUSED(out);
	return TESSERA_ENOTIMPL;
}

int
tessera_encode_registry_entry(const tessera_registry_entry_t *in,
                              uint8_t out[TESSERA_REGISTRY_ENTRY_SIZE])
{
	UNUSED(in); UNUSED(out);
	return TESSERA_ENOTIMPL;
}

int
tessera_decode_registry_entry(const uint8_t in[TESSERA_REGISTRY_ENTRY_SIZE],
                              tessera_registry_entry_t *out)
{
	UNUSED(in); UNUSED(out);
	return TESSERA_ENOTIMPL;
}

int
tessera_encode_free_extent(const tessera_free_extent_t *in,
                           uint8_t out[TESSERA_EXTENT_ENTRY_SIZE])
{
	UNUSED(in); UNUSED(out);
	return TESSERA_ENOTIMPL;
}

int
tessera_decode_free_extent(const uint8_t in[TESSERA_EXTENT_ENTRY_SIZE],
                           tessera_free_extent_t *out)
{
	UNUSED(in); UNUSED(out);
	return TESSERA_ENOTIMPL;
}
