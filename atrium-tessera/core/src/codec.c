/*
 * tessera-core: encode / decode of on-disk structs.
 *
 * Encoding strategy:
 *   - Every on-disk struct in <tessera/format.h> is __attribute__((packed))
 *     with field types whose widths and ordering already match the spec.
 *   - The host is little-endian (compile-time-checked in src/crc.c), so
 *     the in-memory representation IS the on-disk byte representation.
 *     Encode = memcpy(out, in, sizeof(*in)) + patch CRC fields the
 *     encoder owns. Decode = magic check + CRC verify + memcpy.
 *   - The caller is responsible for zeroing the input struct (memset 0)
 *     before populating fields, so reserved bytes and unset members are
 *     deterministic — encode does not zero on the caller's behalf.
 *
 * CRC ownership:
 *   - encode_superblock        fills .crc32           (over bytes 0..offsetof(crc32))
 *   - encode_journal_header    fills .crc32           (over bytes 0..48)
 *   - encode_record_header     fills .crc32_header    (over bytes 0..28)
 *   - encode_pack_header       fills .crc32_header    (over bytes 0..92)
 *   - encode_btree_node_header fills .crc32           (over bytes 0..24)
 *   - encode_pack_footer       passes .crc32_pack through unchanged
 *                              (caller computes it over the whole pack)
 *   - All other encoders are simple memcpy (no CRC field).
 */

#include "tessera/codec.h"
#include "tessera/error.h"
#include "tessera/crc.h"
#include "tessera/format.h"
#include "tessera_compat.h"

#ifndef _KERNEL
#  include <stddef.h>
#endif

/* ── helpers ─────────────────────────────────────────────────────── */

static int
check_magic(const uint8_t *in, const char *magic, size_t n)
{
	return memcmp(in, magic, n) == 0;
}

static void
write_u32_at(uint8_t *p, size_t off, uint32_t v)
{
	memcpy(p + off, &v, 4);
}

static uint32_t
read_u32_at(const uint8_t *p, size_t off)
{
	uint32_t v;
	memcpy(&v, p + off, 4);
	return v;
}

/* ── superblock ──────────────────────────────────────────────────── */

int
tessera_encode_superblock(const tessera_superblock_t *in,
                          uint8_t out[TESSERA_SECTOR_SIZE])
{
	if (in == NULL || out == NULL) return TESSERA_EINVAL;
	memcpy(out, in, sizeof(*in));
	const size_t crc_off = offsetof(tessera_superblock_t, crc32);
	write_u32_at(out, crc_off, tessera_crc32(out, crc_off));
	return TESSERA_OK;
}

int
tessera_decode_superblock(const uint8_t in[TESSERA_SECTOR_SIZE],
                          tessera_superblock_t *out)
{
	if (in == NULL || out == NULL) return TESSERA_EINVAL;
	if (!check_magic(in, TESSERA_MAGIC_SUPERBLOCK, 8))
		return TESSERA_ECORRUPT;
	const size_t crc_off = offsetof(tessera_superblock_t, crc32);
	if (read_u32_at(in, crc_off) != tessera_crc32(in, crc_off))
		return TESSERA_ECORRUPT;
	memcpy(out, in, sizeof(*out));
	return TESSERA_OK;
}

/* ── journal header ──────────────────────────────────────────────── */

int
tessera_encode_journal_header(const tessera_journal_header_t *in,
                              uint8_t out[TESSERA_SECTOR_SIZE])
{
	if (in == NULL || out == NULL) return TESSERA_EINVAL;
	memcpy(out, in, sizeof(*in));
	const size_t crc_off = offsetof(tessera_journal_header_t, crc32);
	write_u32_at(out, crc_off, tessera_crc32(out, crc_off));
	return TESSERA_OK;
}

int
tessera_decode_journal_header(const uint8_t in[TESSERA_SECTOR_SIZE],
                              tessera_journal_header_t *out)
{
	if (in == NULL || out == NULL) return TESSERA_EINVAL;
	if (!check_magic(in, TESSERA_MAGIC_JOURNAL, 8))
		return TESSERA_ECORRUPT;
	const size_t crc_off = offsetof(tessera_journal_header_t, crc32);
	if (read_u32_at(in, crc_off) != tessera_crc32(in, crc_off))
		return TESSERA_ECORRUPT;
	memcpy(out, in, sizeof(*out));
	return TESSERA_OK;
}

/* ── record header ───────────────────────────────────────────────── */

int
tessera_encode_record_header(const tessera_record_header_t *in,
                             uint8_t out[32])
{
	if (in == NULL || out == NULL) return TESSERA_EINVAL;
	memcpy(out, in, sizeof(*in));
	const size_t crc_off = offsetof(tessera_record_header_t, crc32_header);
	write_u32_at(out, crc_off, tessera_crc32(out, crc_off));
	return TESSERA_OK;
}

int
tessera_decode_record_header(const uint8_t in[32],
                             tessera_record_header_t *out)
{
	if (in == NULL || out == NULL) return TESSERA_EINVAL;
	if (!check_magic(in, TESSERA_MAGIC_TXR, 4))
		return TESSERA_ECORRUPT;
	const size_t crc_off = offsetof(tessera_record_header_t, crc32_header);
	if (read_u32_at(in, crc_off) != tessera_crc32(in, crc_off))
		return TESSERA_ECORRUPT;
	memcpy(out, in, sizeof(*out));
	return TESSERA_OK;
}

/* ── pack header / footer ────────────────────────────────────────── */

int
tessera_encode_pack_header(const tessera_pack_header_t *in,
                           uint8_t out[TESSERA_SECTOR_SIZE])
{
	if (in == NULL || out == NULL) return TESSERA_EINVAL;
	memcpy(out, in, sizeof(*in));
	const size_t crc_off = offsetof(tessera_pack_header_t, crc32_header);
	write_u32_at(out, crc_off, tessera_crc32(out, crc_off));
	return TESSERA_OK;
}

int
tessera_decode_pack_header(const uint8_t in[TESSERA_SECTOR_SIZE],
                           tessera_pack_header_t *out)
{
	if (in == NULL || out == NULL) return TESSERA_EINVAL;
	if (!check_magic(in, TESSERA_MAGIC_PACK, 8))
		return TESSERA_ECORRUPT;
	const size_t crc_off = offsetof(tessera_pack_header_t, crc32_header);
	if (read_u32_at(in, crc_off) != tessera_crc32(in, crc_off))
		return TESSERA_ECORRUPT;
	memcpy(out, in, sizeof(*out));
	return TESSERA_OK;
}

int
tessera_encode_pack_footer(const tessera_pack_footer_t *in,
                           uint8_t out[TESSERA_SECTOR_SIZE])
{
	if (in == NULL || out == NULL) return TESSERA_EINVAL;
	memcpy(out, in, sizeof(*in));
	return TESSERA_OK;  /* caller-owned crc32_pack */
}

int
tessera_decode_pack_footer(const uint8_t in[TESSERA_SECTOR_SIZE],
                           tessera_pack_footer_t *out)
{
	if (in == NULL || out == NULL) return TESSERA_EINVAL;
	if (!check_magic(in, TESSERA_MAGIC_PACK_END, 8))
		return TESSERA_ECORRUPT;
	memcpy(out, in, sizeof(*out));
	return TESSERA_OK;
}

/* ── pack index entry / blob descriptor ──────────────────────────── */

int
tessera_encode_pack_index_entry(const tessera_pack_index_entry_t *in,
                                uint8_t out[TESSERA_PACK_INDEX_ENTRY_SIZE])
{
	if (in == NULL || out == NULL) return TESSERA_EINVAL;
	memcpy(out, in, sizeof(*in));
	return TESSERA_OK;
}

int
tessera_decode_pack_index_entry(const uint8_t in[TESSERA_PACK_INDEX_ENTRY_SIZE],
                                tessera_pack_index_entry_t *out)
{
	if (in == NULL || out == NULL) return TESSERA_EINVAL;
	memcpy(out, in, sizeof(*out));
	return TESSERA_OK;
}

int
tessera_encode_blob_descriptor(const tessera_blob_descriptor_t *in,
                               uint8_t out[16])
{
	if (in == NULL || out == NULL) return TESSERA_EINVAL;
	memcpy(out, in, sizeof(*in));
	return TESSERA_OK;
}

int
tessera_decode_blob_descriptor(const uint8_t in[16],
                               tessera_blob_descriptor_t *out)
{
	if (in == NULL || out == NULL) return TESSERA_EINVAL;
	if (!check_magic(in, TESSERA_MAGIC_BLOB, 4))
		return TESSERA_ECORRUPT;
	memcpy(out, in, sizeof(*out));
	return TESSERA_OK;
}

/* ── manifest header ─────────────────────────────────────────────── */

int
tessera_encode_manifest_header(const tessera_manifest_header_t *in,
                               uint8_t out[32])
{
	if (in == NULL || out == NULL) return TESSERA_EINVAL;
	memcpy(out, in, sizeof(*in));
	return TESSERA_OK;
}

int
tessera_decode_manifest_header(const uint8_t in[32],
                               tessera_manifest_header_t *out)
{
	if (in == NULL || out == NULL) return TESSERA_EINVAL;
	if (!check_magic(in, TESSERA_MAGIC_MANIFEST, 4))
		return TESSERA_ECORRUPT;
	memcpy(out, in, sizeof(*out));
	return TESSERA_OK;
}

/* ── inode record ────────────────────────────────────────────────── */

int
tessera_encode_inode(const tessera_inode_record_t *in,
                     uint8_t out[TESSERA_INODE_RECORD_SIZE])
{
	if (in == NULL || out == NULL) return TESSERA_EINVAL;
	memcpy(out, in, sizeof(*in));
	return TESSERA_OK;
}

int
tessera_decode_inode(const uint8_t in[TESSERA_INODE_RECORD_SIZE],
                     tessera_inode_record_t *out)
{
	if (in == NULL || out == NULL) return TESSERA_EINVAL;
	memcpy(out, in, sizeof(*out));
	return TESSERA_OK;
}

/* ── B+tree node header ──────────────────────────────────────────── */

int
tessera_encode_btree_node_header(const tessera_btree_node_header_t *in,
                                 uint8_t out[32])
{
	if (in == NULL || out == NULL) return TESSERA_EINVAL;
	memcpy(out, in, sizeof(*in));
	const size_t crc_off = offsetof(tessera_btree_node_header_t, crc32);
	write_u32_at(out, crc_off, tessera_crc32(out, crc_off));
	return TESSERA_OK;
}

int
tessera_decode_btree_node_header(const uint8_t in[32],
                                 tessera_btree_node_header_t *out)
{
	if (in == NULL || out == NULL) return TESSERA_EINVAL;
	if (!check_magic(in, TESSERA_MAGIC_BTREE_NODE, 4))
		return TESSERA_ECORRUPT;
	const size_t crc_off = offsetof(tessera_btree_node_header_t, crc32);
	if (read_u32_at(in, crc_off) != tessera_crc32(in, crc_off))
		return TESSERA_ECORRUPT;
	memcpy(out, in, sizeof(*out));
	return TESSERA_OK;
}

/* ── pack registry entry / free extent ───────────────────────────── */

int
tessera_encode_registry_entry(const tessera_registry_entry_t *in,
                              uint8_t out[TESSERA_REGISTRY_ENTRY_SIZE])
{
	if (in == NULL || out == NULL) return TESSERA_EINVAL;
	memcpy(out, in, sizeof(*in));
	return TESSERA_OK;
}

int
tessera_decode_registry_entry(const uint8_t in[TESSERA_REGISTRY_ENTRY_SIZE],
                              tessera_registry_entry_t *out)
{
	if (in == NULL || out == NULL) return TESSERA_EINVAL;
	memcpy(out, in, sizeof(*out));
	return TESSERA_OK;
}

int
tessera_encode_free_extent(const tessera_free_extent_t *in,
                           uint8_t out[TESSERA_EXTENT_ENTRY_SIZE])
{
	if (in == NULL || out == NULL) return TESSERA_EINVAL;
	memcpy(out, in, sizeof(*in));
	return TESSERA_OK;
}

int
tessera_decode_free_extent(const uint8_t in[TESSERA_EXTENT_ENTRY_SIZE],
                           tessera_free_extent_t *out)
{
	if (in == NULL || out == NULL) return TESSERA_EINVAL;
	memcpy(out, in, sizeof(*out));
	return TESSERA_OK;
}
