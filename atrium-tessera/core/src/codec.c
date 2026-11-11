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
 * Decoding in Rust (D3): built with TESSERA_RUST_DECODE, every decoder
 * below is a call into rs/tessera-decode (tessera_rs.h); the C bodies are
 * the reference the differential test compares it with, and what the
 * loader still uses.
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
#include "tessera_rs.h"

#ifndef _KERNEL
#  include <stddef.h>
#endif

/* ── helpers ─────────────────────────────────────────────────────── */

#ifndef TESSERA_RUST_DECODE
static int
check_magic(const uint8_t *in, const char *magic, size_t n)
{
	return memcmp(in, magic, n) == 0;
}
#endif

static void
write_u32_at(uint8_t *p, size_t off, uint32_t v)
{
	memcpy(p + off, &v, 4);
}

#ifndef TESSERA_RUST_DECODE
static uint32_t
read_u32_at(const uint8_t *p, size_t off)
{
	uint32_t v;
	memcpy(&v, p + off, 4);
	return v;
}
#endif

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
#ifdef TESSERA_RUST_DECODE
	return tessera_rs_decode_superblock(in, out);
#else
	if (in == NULL || out == NULL) return TESSERA_EINVAL;
	if (!check_magic(in, TESSERA_MAGIC_SUPERBLOCK, 8))
		return TESSERA_ECORRUPT;
	const size_t crc_off = offsetof(tessera_superblock_t, crc32);
	if (read_u32_at(in, crc_off) != tessera_crc32(in, crc_off))
		return TESSERA_ECORRUPT;
	memcpy(out, in, sizeof(*out));
	return TESSERA_OK;
#endif
}

/* ── superblock layout ───────────────────────────────────────────── */

int
tessera_superblock_layout(const tessera_superblock_t *sb,
                          uint64_t device_sectors,
                          uint64_t *out_lo, uint64_t *out_hi)
{
	if (sb == NULL) return TESSERA_EINVAL;
#ifdef TESSERA_RUST_DECODE
	return tessera_rs_superblock_layout(sb, device_sectors, out_lo, out_hi);
#else
	const uint64_t total = sb->total_sectors;
	if (device_sectors != 0 && total > device_sectors)
		return TESSERA_ECORRUPT;
	const uint64_t js = sb->journal_start, jl = sb->journal_length;
	if (js < 2 || jl < 2 || js > UINT64_MAX - jl || js + jl > total)
		return TESSERA_ECORRUPT;
	uint64_t floor = js + jl;
	const uint64_t ms = sb->meta_reserve_start;
	const uint64_t ml = sb->meta_reserve_length;
	if (ml != 0) {
		if (ms > UINT64_MAX - ml) return TESSERA_ECORRUPT;
		const uint64_t me = ms + ml;
		if (ms < floor || me > total ||
		    sb->meta_reserve_bump < ms || sb->meta_reserve_bump > me)
			return TESSERA_ECORRUPT;
		floor = me;
	}
	uint64_t lo, hi;
	const uint64_t ps = sb->pack_zone_start, pl = sb->pack_zone_length;
	if (pl == 0) {
		if (floor >= total) return TESSERA_ECORRUPT;
		lo = floor; hi = total;
	} else {
		if (ps > UINT64_MAX - pl) return TESSERA_ECORRUPT;
		if (ps < floor || ps + pl > total) return TESSERA_ECORRUPT;
		lo = ps; hi = ps + pl;
	}
	if (out_lo) *out_lo = lo;
	if (out_hi) *out_hi = hi;
	return TESSERA_OK;
#endif
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
#ifdef TESSERA_RUST_DECODE
	return tessera_rs_decode_journal_header(in, out);
#else
	if (in == NULL || out == NULL) return TESSERA_EINVAL;
	if (!check_magic(in, TESSERA_MAGIC_JOURNAL, 8))
		return TESSERA_ECORRUPT;
	const size_t crc_off = offsetof(tessera_journal_header_t, crc32);
	if (read_u32_at(in, crc_off) != tessera_crc32(in, crc_off))
		return TESSERA_ECORRUPT;
	memcpy(out, in, sizeof(*out));
	return TESSERA_OK;
#endif
}

/* ── record header ───────────────────────────────────────────────── */

int
tessera_encode_record_header(const tessera_record_header_t *in,
                             uint8_t out[TESSERA_RECORD_HEADER_SIZE])
{
	if (in == NULL || out == NULL) return TESSERA_EINVAL;
	memcpy(out, in, sizeof(*in));
	const size_t crc_off = offsetof(tessera_record_header_t, crc32_header);
	write_u32_at(out, crc_off, tessera_crc32(out, crc_off));
	return TESSERA_OK;
}

int
tessera_decode_record_header(const uint8_t in[TESSERA_RECORD_HEADER_SIZE],
                             tessera_record_header_t *out)
{
#ifdef TESSERA_RUST_DECODE
	return tessera_rs_decode_record_header(in, out);
#else
	if (in == NULL || out == NULL) return TESSERA_EINVAL;
	if (!check_magic(in, TESSERA_MAGIC_TXR, 4))
		return TESSERA_ECORRUPT;
	const size_t crc_off = offsetof(tessera_record_header_t, crc32_header);
	if (read_u32_at(in, crc_off) != tessera_crc32(in, crc_off))
		return TESSERA_ECORRUPT;
	memcpy(out, in, sizeof(*out));
	return TESSERA_OK;
#endif
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
#ifdef TESSERA_RUST_DECODE
	return tessera_rs_decode_pack_header(in, out);
#else
	if (in == NULL || out == NULL) return TESSERA_EINVAL;
	if (!check_magic(in, TESSERA_MAGIC_PACK, 8))
		return TESSERA_ECORRUPT;
	const size_t crc_off = offsetof(tessera_pack_header_t, crc32_header);
	if (read_u32_at(in, crc_off) != tessera_crc32(in, crc_off))
		return TESSERA_ECORRUPT;
	memcpy(out, in, sizeof(*out));
	return TESSERA_OK;
#endif
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
#ifdef TESSERA_RUST_DECODE
	return tessera_rs_decode_pack_footer(in, out);
#else
	if (in == NULL || out == NULL) return TESSERA_EINVAL;
	if (!check_magic(in, TESSERA_MAGIC_PACK_END, 8))
		return TESSERA_ECORRUPT;
	memcpy(out, in, sizeof(*out));
	return TESSERA_OK;
#endif
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
#ifdef TESSERA_RUST_DECODE
	return tessera_rs_decode_pack_index_entry(in, out);
#else
	if (in == NULL || out == NULL) return TESSERA_EINVAL;
	memcpy(out, in, sizeof(*out));
	return TESSERA_OK;
#endif
}

int
tessera_encode_blob_descriptor(const tessera_blob_descriptor_t *in,
                               uint8_t out[TESSERA_BLOB_DESCRIPTOR_SIZE])
{
	if (in == NULL || out == NULL) return TESSERA_EINVAL;
	memcpy(out, in, sizeof(*in));
	return TESSERA_OK;
}

int
tessera_decode_blob_descriptor(const uint8_t in[TESSERA_BLOB_DESCRIPTOR_SIZE],
                               tessera_blob_descriptor_t *out)
{
#ifdef TESSERA_RUST_DECODE
	return tessera_rs_decode_blob_descriptor(in, out);
#else
	if (in == NULL || out == NULL) return TESSERA_EINVAL;
	if (!check_magic(in, TESSERA_MAGIC_BLOB, 4))
		return TESSERA_ECORRUPT;
	memcpy(out, in, sizeof(*out));
	return TESSERA_OK;
#endif
}

/* ── manifest header ─────────────────────────────────────────────── */

int
tessera_encode_manifest_header(const tessera_manifest_header_t *in,
                               uint8_t out[TESSERA_MANIFEST_HEADER_SIZE])
{
	if (in == NULL || out == NULL) return TESSERA_EINVAL;
	memcpy(out, in, sizeof(*in));
	return TESSERA_OK;
}

int
tessera_decode_manifest_header(const uint8_t in[TESSERA_MANIFEST_HEADER_SIZE],
                               tessera_manifest_header_t *out)
{
#ifdef TESSERA_RUST_DECODE
	return tessera_rs_decode_manifest_header(in, out);
#else
	if (in == NULL || out == NULL) return TESSERA_EINVAL;
	if (!check_magic(in, TESSERA_MAGIC_MANIFEST, 4))
		return TESSERA_ECORRUPT;
	memcpy(out, in, sizeof(*out));
	return TESSERA_OK;
#endif
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
#ifdef TESSERA_RUST_DECODE
	return tessera_rs_decode_inode(in, out);
#else
	if (in == NULL || out == NULL) return TESSERA_EINVAL;
	memcpy(out, in, sizeof(*out));
	return TESSERA_OK;
#endif
}

/* ── quota domain record ─────────────────────────────────────────── */

int
tessera_encode_quota_domain(const tessera_quota_domain_t *in,
                            uint8_t out[TESSERA_QUOTA_DOMAIN_SIZE])
{
	if (in == NULL || out == NULL) return TESSERA_EINVAL;
	memcpy(out, in, sizeof(*in));
	return TESSERA_OK;
}

int
tessera_decode_quota_domain(const uint8_t in[TESSERA_QUOTA_DOMAIN_SIZE],
                            tessera_quota_domain_t *out)
{
#ifdef TESSERA_RUST_DECODE
	return tessera_rs_decode_quota_domain(in, out);
#else
	if (in == NULL || out == NULL) return TESSERA_EINVAL;
	memcpy(out, in, sizeof(*out));
	return TESSERA_OK;
#endif
}

/* ── B+tree node header ──────────────────────────────────────────── */

int
tessera_encode_btree_node_header(const tessera_btree_node_header_t *in,
                                 uint8_t out[TESSERA_BTREE_NODE_HEADER_SIZE])
{
	if (in == NULL || out == NULL) return TESSERA_EINVAL;
	memcpy(out, in, sizeof(*in));
	const size_t crc_off = offsetof(tessera_btree_node_header_t, crc32);
	write_u32_at(out, crc_off, tessera_crc32(out, crc_off));
	return TESSERA_OK;
}

int
tessera_decode_btree_node_header(const uint8_t in[TESSERA_BTREE_NODE_HEADER_SIZE],
                                 tessera_btree_node_header_t *out)
{
#ifdef TESSERA_RUST_DECODE
	return tessera_rs_decode_btree_node_header(in, out);
#else
	if (in == NULL || out == NULL) return TESSERA_EINVAL;
	if (!check_magic(in, TESSERA_MAGIC_BTREE_NODE, 4))
		return TESSERA_ECORRUPT;
	const size_t crc_off = offsetof(tessera_btree_node_header_t, crc32);
	if (read_u32_at(in, crc_off) != tessera_crc32(in, crc_off))
		return TESSERA_ECORRUPT;
	memcpy(out, in, sizeof(*out));
	return TESSERA_OK;
#endif
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
#ifdef TESSERA_RUST_DECODE
	return tessera_rs_decode_registry_entry(in, out);
#else
	if (in == NULL || out == NULL) return TESSERA_EINVAL;
	memcpy(out, in, sizeof(*out));
	return TESSERA_OK;
#endif
}

int
tessera_encode_pack_extent_list(const tessera_pack_extent_list_t *in,
                                uint8_t out[TESSERA_SECTOR_SIZE])
{
	if (in == NULL || out == NULL) return TESSERA_EINVAL;
	memcpy(out, in, sizeof(*in));
	const size_t crc_off = offsetof(tessera_pack_extent_list_t, crc32);
	write_u32_at(out, crc_off, tessera_crc32(out, crc_off));
	return TESSERA_OK;
}

int
tessera_decode_pack_extent_list(const uint8_t in[TESSERA_SECTOR_SIZE],
                                tessera_pack_extent_list_t *out)
{
#ifdef TESSERA_RUST_DECODE
	return tessera_rs_decode_pack_extent_list(in, out);
#else
	if (in == NULL || out == NULL) return TESSERA_EINVAL;
	tessera_pack_extent_list_t pel;
	memcpy(&pel, in, sizeof pel);
	if (pel.magic != TESSERA_PEL_MAGIC) return TESSERA_EBADMAGIC;
	const size_t crc_off = offsetof(tessera_pack_extent_list_t, crc32);
	if (read_u32_at(in, crc_off) != tessera_crc32(in, crc_off))
		return TESSERA_EBADCRC;
	if (pel.extent_count == 0 ||
	    pel.extent_count > TESSERA_PEL_MAX_EXTENTS)
		return TESSERA_ECORRUPT;
	uint64_t sum = 0;
	for (uint32_t i = 0; i < pel.extent_count; i++) {
		if (pel.extents[i].length_sectors == 0) return TESSERA_ECORRUPT;
		sum += pel.extents[i].length_sectors;
	}
	if (sum != pel.total_length) return TESSERA_ECORRUPT;
	*out = pel;
	return TESSERA_OK;
#endif
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
#ifdef TESSERA_RUST_DECODE
	return tessera_rs_decode_free_extent(in, out);
#else
	if (in == NULL || out == NULL) return TESSERA_EINVAL;
	memcpy(out, in, sizeof(*out));
	return TESSERA_OK;
#endif
}
