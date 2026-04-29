/*
 * Round-trip tests for every encode / decode pair in tessera/codec.h.
 *
 * For each struct family:
 *   1. Populate an in-memory struct with non-zero deterministic values.
 *   2. encode() into a byte buffer.
 *   3. decode() into a fresh struct.
 *   4. Assert byte-for-byte equality of the original vs decoded struct
 *      (modulo encoder-owned fields like CRCs).
 *   5. Flip a byte in the encoded buffer; assert decode() returns
 *      TESSERA_ECORRUPT for structs that carry a CRC or magic.
 *
 * No SHA-256 dependency.
 */

#include "tessera/codec.h"
#include "tessera/error.h"
#include "tessera/format.h"

#include <stdint.h>
#include <stdio.h>
#include <string.h>

static int failures = 0;

#define CHECK(cond) do {                                                 \
	if (!(cond)) {                                                   \
		fprintf(stderr, "FAIL %s:%d: %s\n",                      \
		    __FILE__, __LINE__, #cond);                          \
		failures++;                                              \
	}                                                                \
} while (0)

static void
fill_pattern(void *p, size_t n, uint8_t seed)
{
	uint8_t *b = p;
	for (size_t i = 0; i < n; i++)
		b[i] = (uint8_t)(seed + i * 7);
}

/* ── superblock ──────────────────────────────────────────────────── */

static void
test_superblock(void)
{
	tessera_superblock_t sb;
	memset(&sb, 0, sizeof sb);
	memcpy(sb.magic, TESSERA_MAGIC_SUPERBLOCK, 8);
	sb.version_major = 1;
	sb.version_minor = 0;
	sb.feature_flags = TESSERA_FEATURE_MULTI_LEVEL_MFT;
	sb.generation = 42;
	fill_pattern(sb.volume_uuid, sizeof sb.volume_uuid, 0xa1);
	sb.total_sectors = 1u << 20;
	sb.sector_size = TESSERA_SECTOR_SIZE;
	sb.journal_start = 4;
	sb.journal_length = 256;
	sb.inode_root = 8;
	sb.inode_root_generation = 1;
	sb.pack_zone_start = 1024;
	sb.pack_zone_length = 1u << 18;
	sb.next_inode_no = TESSERA_INODE_FIRST_USER;
	sb.format_time = 0x1234567890abcdefull;

	uint8_t buf[TESSERA_SECTOR_SIZE];
	memset(buf, 0xee, sizeof buf);
	CHECK(tessera_encode_superblock(&sb, buf) == TESSERA_OK);

	tessera_superblock_t sb2;
	memset(&sb2, 0, sizeof sb2);
	CHECK(tessera_decode_superblock(buf, &sb2) == TESSERA_OK);

	/* CRC was filled in by encode; reflect that in the source. */
	uint32_t enc_crc;
	memcpy(&enc_crc, buf + offsetof(tessera_superblock_t, crc32), 4);
	sb.crc32 = enc_crc;
	CHECK(memcmp(&sb, &sb2, sizeof sb) == 0);

	/* Corruption detection: flip a byte before the CRC field. */
	buf[40] ^= 0x01;
	CHECK(tessera_decode_superblock(buf, &sb2) == TESSERA_ECORRUPT);
	buf[40] ^= 0x01;

	/* Bad magic. */
	buf[0] = 'X';
	CHECK(tessera_decode_superblock(buf, &sb2) == TESSERA_ECORRUPT);
}

/* ── journal header ──────────────────────────────────────────────── */

static void
test_journal_header(void)
{
	tessera_journal_header_t jh;
	memset(&jh, 0, sizeof jh);
	memcpy(jh.magic, TESSERA_MAGIC_JOURNAL, 8);
	jh.version = 1;
	jh.head_seq = 0xdeadbeef;
	jh.tail_seq = 0xfeed0001;
	jh.head_block = 12;
	jh.tail_block = 34;

	uint8_t buf[TESSERA_SECTOR_SIZE];
	CHECK(tessera_encode_journal_header(&jh, buf) == TESSERA_OK);

	tessera_journal_header_t jh2;
	CHECK(tessera_decode_journal_header(buf, &jh2) == TESSERA_OK);
	memcpy(&jh.crc32, buf + offsetof(tessera_journal_header_t, crc32), 4);
	CHECK(memcmp(&jh, &jh2, sizeof jh) == 0);

	buf[16] ^= 0xff;
	CHECK(tessera_decode_journal_header(buf, &jh2) == TESSERA_ECORRUPT);
}

/* ── record header ───────────────────────────────────────────────── */

static void
test_record_header(void)
{
	tessera_record_header_t rh;
	memset(&rh, 0, sizeof rh);
	memcpy(rh.magic, TESSERA_MAGIC_TXR, 4);
	rh.record_type = TESSERA_INODE_WRITE;
	rh.sequence = 7;
	rh.body_length = 512;
	rh.block_count = 1;
	rh.crc32_body = 0xa5a5a5a5;

	uint8_t buf[32];
	CHECK(tessera_encode_record_header(&rh, buf) == TESSERA_OK);

	tessera_record_header_t rh2;
	CHECK(tessera_decode_record_header(buf, &rh2) == TESSERA_OK);
	memcpy(&rh.crc32_header,
	    buf + offsetof(tessera_record_header_t, crc32_header), 4);
	CHECK(memcmp(&rh, &rh2, sizeof rh) == 0);

	/* crc32_body lives inside the CRC-protected range — flipping it
	 * must trip the header CRC. */
	buf[24] ^= 0x80;
	CHECK(tessera_decode_record_header(buf, &rh2) == TESSERA_ECORRUPT);
}

/* ── pack header / footer ────────────────────────────────────────── */

static void
test_pack_header(void)
{
	tessera_pack_header_t ph;
	memset(&ph, 0, sizeof ph);
	memcpy(ph.magic, TESSERA_MAGIC_PACK, 8);
	ph.version = 1;
	ph.pack_kind = 1;
	fill_pattern(ph.pack_id, 16, 0x55);
	ph.create_time = 1234;
	ph.creator_tx_id = 99;
	ph.blob_count = 1024;
	ph.index_blocks = 16;
	ph.bloom_bytes = 4096;
	ph.bloom_hash_count = 7;
	ph.data_offset = 0x10000;
	ph.data_length = 0x40000;
	ph.total_pack_bytes = 0x50000;

	uint8_t buf[TESSERA_SECTOR_SIZE];
	CHECK(tessera_encode_pack_header(&ph, buf) == TESSERA_OK);

	tessera_pack_header_t ph2;
	CHECK(tessera_decode_pack_header(buf, &ph2) == TESSERA_OK);
	memcpy(&ph.crc32_header,
	    buf + offsetof(tessera_pack_header_t, crc32_header), 4);
	CHECK(memcmp(&ph, &ph2, sizeof ph) == 0);

	buf[60] ^= 1;
	CHECK(tessera_decode_pack_header(buf, &ph2) == TESSERA_ECORRUPT);
}

static void
test_pack_footer(void)
{
	tessera_pack_footer_t pf;
	memset(&pf, 0, sizeof pf);
	memcpy(pf.magic, TESSERA_MAGIC_PACK_END, 8);
	pf.blob_count_check = 1024;
	pf.crc32_pack = 0x12345678; /* caller-computed */

	uint8_t buf[TESSERA_SECTOR_SIZE];
	CHECK(tessera_encode_pack_footer(&pf, buf) == TESSERA_OK);

	tessera_pack_footer_t pf2;
	CHECK(tessera_decode_pack_footer(buf, &pf2) == TESSERA_OK);
	CHECK(memcmp(&pf, &pf2, sizeof pf) == 0);

	buf[0] = 'X';
	CHECK(tessera_decode_pack_footer(buf, &pf2) == TESSERA_ECORRUPT);
}

/* ── pack index entry / blob descriptor ──────────────────────────── */

static void
test_pack_index_entry(void)
{
	tessera_pack_index_entry_t e;
	memset(&e, 0, sizeof e);
	fill_pattern(e.blob_hash, 32, 0x33);
	e.data_offset = 0xabcd;
	e.data_size = 4096;
	e.flags = TESSERA_BLOB_FLAG_CHUNK;

	uint8_t buf[TESSERA_PACK_INDEX_ENTRY_SIZE];
	CHECK(tessera_encode_pack_index_entry(&e, buf) == TESSERA_OK);

	tessera_pack_index_entry_t e2;
	CHECK(tessera_decode_pack_index_entry(buf, &e2) == TESSERA_OK);
	CHECK(memcmp(&e, &e2, sizeof e) == 0);
}

static void
test_blob_descriptor(void)
{
	tessera_blob_descriptor_t bd;
	memset(&bd, 0, sizeof bd);
	memcpy(bd.magic, TESSERA_MAGIC_BLOB, 4);
	bd.uncompressed_size = 8192;
	bd.compressed_size = 0;

	uint8_t buf[16];
	CHECK(tessera_encode_blob_descriptor(&bd, buf) == TESSERA_OK);

	tessera_blob_descriptor_t bd2;
	CHECK(tessera_decode_blob_descriptor(buf, &bd2) == TESSERA_OK);
	CHECK(memcmp(&bd, &bd2, sizeof bd) == 0);

	buf[0] = 'X';
	CHECK(tessera_decode_blob_descriptor(buf, &bd2) == TESSERA_ECORRUPT);
}

/* ── manifest header ─────────────────────────────────────────────── */

static void
test_manifest_header(void)
{
	tessera_manifest_header_t mh;
	memset(&mh, 0, sizeof mh);
	memcpy(mh.magic, TESSERA_MAGIC_MANIFEST, 4);
	mh.version = 1;
	mh.manifest_kind = TESSERA_MFT_CHUNK_LIST;
	mh.level = 0;
	mh.logical_size = 1u << 20;
	mh.chunk_size_avg = 64 * 1024;
	mh.entry_count = 16;

	uint8_t buf[32];
	CHECK(tessera_encode_manifest_header(&mh, buf) == TESSERA_OK);

	tessera_manifest_header_t mh2;
	CHECK(tessera_decode_manifest_header(buf, &mh2) == TESSERA_OK);
	CHECK(memcmp(&mh, &mh2, sizeof mh) == 0);

	buf[1] = 'X';
	CHECK(tessera_decode_manifest_header(buf, &mh2) == TESSERA_ECORRUPT);
}

/* ── inode record ────────────────────────────────────────────────── */

static void
test_inode(void)
{
	tessera_inode_record_t ino;
	memset(&ino, 0, sizeof ino);
	ino.inode_no = 1234;
	ino.gen = 5;
	ino.mode = 0100644;  /* regular file, 0644 */
	ino.uid = 1000;
	ino.gid = 1000;
	ino.atime_ns = 1700000000ull * 1000000000ull;
	ino.mtime_ns = 1700000001ull * 1000000000ull;
	ino.ctime_ns = 1700000002ull * 1000000000ull;
	ino.btime_ns = 1700000003ull * 1000000000ull;
	ino.size = 4096;
	ino.nlink = 1;
	ino.flags = TESSERA_INODE_FLAG_NODUMP;
	fill_pattern(ino.manifest_hash, 32, 0x77);
	fill_pattern(ino.xattr_hash, 32, 0x88);

	uint8_t buf[TESSERA_INODE_RECORD_SIZE];
	CHECK(tessera_encode_inode(&ino, buf) == TESSERA_OK);

	tessera_inode_record_t ino2;
	CHECK(tessera_decode_inode(buf, &ino2) == TESSERA_OK);
	CHECK(memcmp(&ino, &ino2, sizeof ino) == 0);
}

/* ── B+tree node header ──────────────────────────────────────────── */

static void
test_btree_node_header(void)
{
	tessera_btree_node_header_t nh;
	memset(&nh, 0, sizeof nh);
	memcpy(nh.magic, TESSERA_MAGIC_BTREE_NODE, 4);
	nh.version = 1;
	nh.node_kind = 0;     /* leaf */
	nh.tree_kind = 0;     /* inode */
	nh.entry_count = 26;
	nh.key_size = 4;      /* inode_no */
	nh.value_size = TESSERA_INODE_RECORD_SIZE;

	uint8_t buf[32];
	CHECK(tessera_encode_btree_node_header(&nh, buf) == TESSERA_OK);

	tessera_btree_node_header_t nh2;
	CHECK(tessera_decode_btree_node_header(buf, &nh2) == TESSERA_OK);
	memcpy(&nh.crc32,
	    buf + offsetof(tessera_btree_node_header_t, crc32), 4);
	CHECK(memcmp(&nh, &nh2, sizeof nh) == 0);

	buf[10] ^= 0xff;
	CHECK(tessera_decode_btree_node_header(buf, &nh2) == TESSERA_ECORRUPT);
}

/* ── pack registry entry / free extent ───────────────────────────── */

static void
test_registry_entry(void)
{
	tessera_registry_entry_t re;
	memset(&re, 0, sizeof re);
	fill_pattern(re.pack_id, 16, 0x44);
	re.start_sector = 100;
	re.length_sectors = 64;
	re.blob_count = 12;
	re.pack_kind = 1;
	re.total_bytes = 0x40000;
	re.create_time = 9999;
	re.reachable_blobs = 8;
	re.flags = TESSERA_REGISTRY_FLAG_SEALED;

	uint8_t buf[TESSERA_REGISTRY_ENTRY_SIZE];
	CHECK(tessera_encode_registry_entry(&re, buf) == TESSERA_OK);

	tessera_registry_entry_t re2;
	CHECK(tessera_decode_registry_entry(buf, &re2) == TESSERA_OK);
	CHECK(memcmp(&re, &re2, sizeof re) == 0);
}

static void
test_free_extent(void)
{
	tessera_free_extent_t fe = { .start_sector = 200, .length_sectors = 50 };
	uint8_t buf[TESSERA_EXTENT_ENTRY_SIZE];
	CHECK(tessera_encode_free_extent(&fe, buf) == TESSERA_OK);

	tessera_free_extent_t fe2;
	CHECK(tessera_decode_free_extent(buf, &fe2) == TESSERA_OK);
	CHECK(memcmp(&fe, &fe2, sizeof fe) == 0);
}

/* ── EINVAL on NULLs ─────────────────────────────────────────────── */

static void
test_null_inputs(void)
{
	tessera_superblock_t sb;
	memset(&sb, 0, sizeof sb);
	uint8_t buf[TESSERA_SECTOR_SIZE];
	CHECK(tessera_encode_superblock(NULL, buf) == TESSERA_EINVAL);
	CHECK(tessera_encode_superblock(&sb, NULL) == TESSERA_EINVAL);
	CHECK(tessera_decode_superblock(NULL, &sb) == TESSERA_EINVAL);
	CHECK(tessera_decode_superblock(buf, NULL) == TESSERA_EINVAL);
}

int
main(void)
{
	printf("test_codec: round-trip + corruption detection for all on-disk structs\n");
	test_superblock();
	test_journal_header();
	test_record_header();
	test_pack_header();
	test_pack_footer();
	test_pack_index_entry();
	test_blob_descriptor();
	test_manifest_header();
	test_inode();
	test_btree_node_header();
	test_registry_entry();
	test_free_extent();
	test_null_inputs();
	if (failures > 0) {
		fprintf(stderr, "%d failure(s)\n", failures);
		return 1;
	}
	printf("ok\n");
	return 0;
}
