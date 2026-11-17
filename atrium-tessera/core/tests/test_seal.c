/*
 * Tests for tessera/seal.h (docs/spec/tessera-encryption.md §5.2–§5.5) on
 * the OpenSSL backend, both suites:
 *
 *   1. Superblock: seal then verify; the clear counters are zero and the
 *      sealed ones come back; a flip of any byte the HMAC covers is EAUTH
 *      (or ENOKEY / EINCOMPAT when it hits a field checked first); a wrong
 *      key is ENOKEY; it survives the codec's encode/decode.
 *   2. Metadata blocks: round trip; a flip of any one byte of the 4 KiB
 *      block is refused; the wrong sector, kind, ctx or prev tag is refused;
 *      an older copy of the block (resealed, so a different tag) is refused
 *      against the parent's recorded tag, before decryption.
 *   3. Journal chain: records chained by prev tag open only in order.
 *   4. Units and blobs: round trip; another id, group, kind or volume is
 *      refused; truncation is refused.
 */
#include "tessera/codec.h"
#include "tessera/crypt.h"
#include "tessera/error.h"
#include "tessera/seal.h"

#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

void tessera_crypt_openssl_install(void);

static int failures = 0;
#define CHECK(cond) do {                                                 \
	if (!(cond)) {                                                   \
		fprintf(stderr, "FAIL %s:%d: %s\n",                      \
		    __FILE__, __LINE__, #cond);                          \
		failures++;                                              \
	}                                                                \
} while (0)

static const uint8_t VK[32] = {
	1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16,
	17, 18, 19, 20, 21, 22, 23, 24, 25, 26, 27, 28, 29, 30, 31, 32 };
static const uint8_t UUID[16] = {
	0xa0, 0xa1, 0xa2, 0xa3, 0xa4, 0xa5, 0xa6, 0xa7,
	0xa8, 0xa9, 0xaa, 0xab, 0xac, 0xad, 0xae, 0xaf };

static void
make_sb(tessera_superblock_t *sb)
{
	memset(sb, 0, sizeof(*sb));
	memcpy(sb->magic, TESSERA_MAGIC_SUPERBLOCK, 8);
	sb->version_major = 1;
	sb->sector_size = TESSERA_SECTOR_SIZE;
	sb->generation = 42;
	memcpy(sb->volume_uuid, UUID, 16);
	sb->total_sectors = 1u << 20;
	sb->inode_root = 100;
	sb->total_blob_count = 7;
	sb->total_pack_count = 3;
	sb->format_time = 1763390000;
	sb->last_mount_time = 1763391111;
	for (unsigned i = 0; i < TESSERA_SB_ROOT_TAGS; i++)
		memset(sb->root_tags[i], 0x10 + i, 16);
}

static void
test_superblock(uint32_t suite)
{
	tessera_seal_keys_t k, other;
	tessera_superblock_t sb, t, d;
	tessera_sb_stats_t st;
	uint8_t vk2[32], raw[TESSERA_SECTOR_SIZE];
	size_t off, crc_off = offsetof(tessera_superblock_t, crc32);
	int refused = 0;

	CHECK(tessera_seal_keys_init(&k, suite, VK, UUID) == TESSERA_OK);
	make_sb(&sb);
	CHECK(tessera_sb_seal(&k, &sb, NULL) == TESSERA_OK);
	CHECK(sb.incompat_flags & TESSERA_INCOMPAT_SEALED);
	CHECK(sb.encryption_flags & TESSERA_ENC_SEALED);
	CHECK(sb.enc_suite == suite);
	CHECK(sb.total_blob_count == 0 && sb.total_pack_count == 0);
	CHECK(sb.format_time == 0 && sb.last_mount_time == 0);

	memset(&st, 0xee, sizeof(st));
	CHECK(tessera_sb_verify(&k, &sb, &st) == TESSERA_OK);
	CHECK(st.total_blob_count == 7 && st.total_pack_count == 3);
	CHECK(st.format_time == 1763390000 && st.last_mount_time == 1763391111);

	/* Through the codec: the CRC is added on encode, the HMAC still holds. */
	CHECK(tessera_encode_superblock(&sb, raw) == TESSERA_OK);
	CHECK(tessera_decode_superblock(raw, &d) == TESSERA_OK);
	CHECK(tessera_sb_verify(&k, &d, NULL) == TESSERA_OK);

	/* Every byte before crc32, and every byte of hmac. The CRC itself is
	 * not under the HMAC (tools check it without a key). */
	for (off = 0; off < sizeof(sb); off++) {
		if (off >= crc_off && off < crc_off + 4)
			continue;
		t = sb;
		((uint8_t *)&t)[off] ^= 0x01;
		if (tessera_sb_verify(&k, &t, NULL) != TESSERA_OK)
			refused++;
		else
			fprintf(stderr, "  sb flip at %zu accepted\n", off);
	}
	CHECK(refused == (int)(sizeof(sb) - 4));

	/* Sealed counters bound to the generation. */
	t = sb;
	t.generation++;
	CHECK(tessera_sb_verify(&k, &t, NULL) == TESSERA_EAUTH);

	/* Wrong key: refused as not-this-key, before the HMAC. */
	memcpy(vk2, VK, 32);
	vk2[0] ^= 1;
	CHECK(tessera_seal_keys_init(&other, suite, vk2, UUID) == TESSERA_OK);
	CHECK(tessera_sb_verify(&other, &sb, NULL) == TESSERA_ENOKEY);
	tessera_seal_keys_clear(&other);

	/* Unsealed superblock. */
	make_sb(&t);
	CHECK(tessera_sb_verify(&k, &t, NULL) == TESSERA_EINCOMPAT);

	/* Mismatched uuid at seal time. */
	make_sb(&t);
	t.volume_uuid[0] ^= 1;
	CHECK(tessera_sb_seal(&k, &t, NULL) == TESSERA_EINVAL);
	tessera_seal_keys_clear(&k);
}

static void
fill(uint8_t *b, size_t n, uint32_t seed)
{
	for (size_t i = 0; i < n; i++) {
		seed = seed * 1103515245u + 12345u;
		b[i] = (uint8_t)(seed >> 16);
	}
}

static void
test_blocks(uint32_t suite)
{
	tessera_seal_keys_t k;
	uint8_t pt[TESSERA_SECTOR_SIZE], b[TESSERA_SECTOR_SIZE],
	    t[TESSERA_SECTOR_SIZE], old[TESSERA_SECTOR_SIZE], tag[16], tag2[16];
	const uint8_t K = TESSERA_SEAL_KIND_BTREE_NODE;
	int refused = 0;

	CHECK(tessera_seal_keys_init(&k, suite, VK, UUID) == TESSERA_OK);
	fill(pt, sizeof(pt), 7);
	memcpy(b, pt, sizeof(b));
	CHECK(tessera_block_seal(&k, K, 1234, TESSERA_BTREE_KIND_INODE, NULL, b,
	    tag) == TESSERA_OK);
	CHECK(memcmp(tag, tessera_block_tag(b), 16) == 0);
	CHECK(memcmp(b, pt, TESSERA_SEALED_BLOCK_PAYLOAD) != 0);

	memcpy(t, b, sizeof(t));
	CHECK(tessera_block_open(&k, K, 1234, 0, NULL, tag, t) == TESSERA_OK);
	CHECK(memcmp(t, pt, TESSERA_SEALED_BLOCK_PAYLOAD) == 0);

	/* Any one byte flipped anywhere in the block. */
	for (size_t off = 0; off < TESSERA_SECTOR_SIZE; off++) {
		memcpy(t, b, sizeof(t));
		t[off] ^= 0x80;
		if (tessera_block_open(&k, K, 1234, 0, NULL, NULL, t) ==
		    TESSERA_EAUTH)
			refused++;
	}
	CHECK(refused == TESSERA_SECTOR_SIZE);
	/* Failure leaves no plaintext. */
	memcpy(t, b, sizeof(t));
	t[5] ^= 1;
	CHECK(tessera_block_open(&k, K, 1234, 0, NULL, NULL, t) == TESSERA_EAUTH);
	for (size_t i = 0; i < TESSERA_SEALED_BLOCK_PAYLOAD; i++)
		if (t[i] != 0) { CHECK(!"payload not zeroed"); break; }

	/* Wrong place, kind, tree or chain. */
	memcpy(t, b, sizeof(t));
	CHECK(tessera_block_open(&k, K, 1235, 0, NULL, NULL, t) == TESSERA_EAUTH);
	memcpy(t, b, sizeof(t));
	CHECK(tessera_block_open(&k, TESSERA_SEAL_KIND_PEL, 1234, 0, NULL, NULL,
	    t) == TESSERA_EAUTH);
	memcpy(t, b, sizeof(t));
	CHECK(tessera_block_open(&k, K, 1234, TESSERA_BTREE_KIND_PACK_REG, NULL,
	    NULL, t) == TESSERA_EAUTH);
	memcpy(t, b, sizeof(t));
	CHECK(tessera_block_open(&k, K, 1234, 0, tag, NULL, t) == TESSERA_EAUTH);

	/* An older copy: the same plaintext resealed at the same sector is a
	 * valid unit, but its parent now records the newer tag. */
	memcpy(old, b, sizeof(old));
	memcpy(b, pt, sizeof(b));
	b[0] ^= 1;
	CHECK(tessera_block_seal(&k, K, 1234, 0, NULL, b, tag2) == TESSERA_OK);
	CHECK(memcmp(tag, tag2, 16) != 0);
	memcpy(t, old, sizeof(t));
	CHECK(tessera_block_open(&k, K, 1234, 0, NULL, NULL, t) == TESSERA_OK);
	memcpy(t, old, sizeof(t));
	CHECK(tessera_block_open(&k, K, 1234, 0, NULL, tag2, t) == TESSERA_EAUTH);
	memcpy(t, b, sizeof(t));
	CHECK(tessera_block_open(&k, K, 1234, 0, NULL, tag2, t) == TESSERA_OK);
	tessera_seal_keys_clear(&k);
}

static void
test_journal_chain(uint32_t suite)
{
	tessera_seal_keys_t k;
	uint8_t rec[3][TESSERA_SECTOR_SIZE], t[TESSERA_SECTOR_SIZE];
	uint8_t tags[3][16], anchor[16];
	const uint8_t J = TESSERA_SEAL_KIND_JOURNAL_REC;

	CHECK(tessera_seal_keys_init(&k, suite, VK, UUID) == TESSERA_OK);
	memset(anchor, 0x5a, 16);
	for (int i = 0; i < 3; i++) {
		fill(rec[i], sizeof(rec[i]), 100 + i);
		CHECK(tessera_block_seal(&k, J, 10 + i, 500 + i,
		    i == 0 ? anchor : tags[i - 1], rec[i], tags[i]) ==
		    TESSERA_OK);
	}
	/* In order, each chained to the one before. */
	for (int i = 0; i < 3; i++) {
		memcpy(t, rec[i], sizeof(t));
		CHECK(tessera_block_open(&k, J, 10 + i, 500 + i,
		    i == 0 ? anchor : tags[i - 1], NULL, t) == TESSERA_OK);
	}
	/* Record 2 spliced after record 0 (record 1 dropped). */
	memcpy(t, rec[2], sizeof(t));
	CHECK(tessera_block_open(&k, J, 12, 502, tags[0], NULL, t) ==
	    TESSERA_EAUTH);
	/* Record 0 against another anchor (an older journal). */
	memcpy(t, rec[0], sizeof(t));
	CHECK(tessera_block_open(&k, J, 10, 500, NULL, NULL, t) == TESSERA_EAUTH);
	tessera_seal_keys_clear(&k);
}

static void
test_units(uint32_t suite)
{
	tessera_seal_keys_t k;
	tessera_hash_t id, id2;
	uint8_t pt[700], out[700 + TESSERA_SEAL_OVERHEAD], back[700], tag[16];
	uint8_t uuid2[16];

	CHECK(tessera_seal_keys_init(&k, suite, VK, UUID) == TESSERA_OK);
	fill(pt, sizeof(pt), 9);
	CHECK(tessera_keyed_id(TESSERA_HASH_ALG_BLAKE3_256, k.id, pt,
	    sizeof(pt), id) == TESSERA_OK);
	memcpy(id2, id, sizeof(id2));
	id2[31] ^= 1;
	memcpy(uuid2, UUID, 16);
	uuid2[15] ^= 1;

	CHECK(tessera_blob_seal(&k.blob, UUID, 0, id, pt, sizeof(pt), out) ==
	    TESSERA_OK);
	CHECK(tessera_blob_open(&k.blob, UUID, 0, id, out, sizeof(out), back) ==
	    TESSERA_OK);
	CHECK(memcmp(back, pt, sizeof(pt)) == 0);
	CHECK(tessera_blob_open(&k.blob, UUID, 0, id2, out, sizeof(out),
	    back) == TESSERA_EAUTH);
	CHECK(tessera_blob_open(&k.blob, UUID, 5, id, out, sizeof(out),
	    back) == TESSERA_EAUTH);
	CHECK(tessera_blob_open(&k.blob, uuid2, 0, id, out, sizeof(out),
	    back) == TESSERA_EAUTH);
	CHECK(tessera_blob_open(&k.meta, UUID, 0, id, out, sizeof(out),
	    back) == TESSERA_EAUTH);
	CHECK(tessera_blob_open(&k.blob, UUID, 0, id, out, sizeof(out) - 1,
	    back) == TESSERA_EAUTH);
	CHECK(tessera_blob_open(&k.blob, UUID, 0, id, out, 39, back) ==
	    TESSERA_EINVAL);

	/* Front matter with a recorded tag; a kind mismatch is refused. */
	CHECK(tessera_unit_seal(&k.meta, UUID, TESSERA_SEAL_KIND_PACK_FRONT, 3,
	    NULL, 0, pt, 100, out, tag) == TESSERA_OK);
	CHECK(tessera_unit_open(&k.meta, UUID, TESSERA_SEAL_KIND_PACK_FRONT, 3,
	    NULL, 0, out, 140, tag, back) == TESSERA_OK);
	CHECK(memcmp(back, pt, 100) == 0);
	CHECK(tessera_unit_open(&k.meta, UUID, TESSERA_SEAL_KIND_BLOB, 3,
	    NULL, 0, out, 140, NULL, back) == TESSERA_EAUTH);
	tag[0] ^= 1;
	CHECK(tessera_unit_open(&k.meta, UUID, TESSERA_SEAL_KIND_PACK_FRONT, 3,
	    NULL, 0, out, 140, tag, back) == TESSERA_EAUTH);
	/* Empty payload still authenticates. */
	CHECK(tessera_unit_seal(&k.meta, UUID, TESSERA_SEAL_KIND_PACK_FRONT, 0,
	    NULL, 0, NULL, 0, out, NULL) == TESSERA_OK);
	CHECK(tessera_unit_open(&k.meta, UUID, TESSERA_SEAL_KIND_PACK_FRONT, 0,
	    NULL, 0, out, 40, NULL, NULL) == TESSERA_OK);
	out[30] ^= 1;
	CHECK(tessera_unit_open(&k.meta, UUID, TESSERA_SEAL_KIND_PACK_FRONT, 0,
	    NULL, 0, out, 40, NULL, NULL) == TESSERA_EAUTH);
	/* Oversized AAD tail. */
	CHECK(tessera_unit_seal(&k.meta, UUID, 1, 0, pt, 65, pt, 1, out,
	    NULL) == TESSERA_EINVAL);
	tessera_seal_keys_clear(&k);
}

static void
test_keys(void)
{
	tessera_seal_keys_t a, b;
	uint8_t uuid2[16];

	CHECK(tessera_seal_keys_init(&a, 9, VK, UUID) == TESSERA_EINCOMPAT);
	CHECK(tessera_seal_keys_init(&a, TESSERA_SUITE_XAES_256_GCM, VK,
	    UUID) == TESSERA_OK);
	CHECK(memcmp(a.meta.key, a.blob.key, 32) != 0);
	CHECK(memcmp(a.meta.key, a.id, 32) != 0);
	memcpy(uuid2, UUID, 16);
	uuid2[0] ^= 1;
	CHECK(tessera_seal_keys_init(&b, TESSERA_SUITE_XAES_256_GCM, VK,
	    uuid2) == TESSERA_OK);
	CHECK(memcmp(a.meta.key, b.meta.key, 32) != 0);
	CHECK(memcmp(a.check, b.check, 32) != 0);
	tessera_seal_keys_clear(&a);
	CHECK(a.meta.key[0] == 0 && a.meta.suite == 0);
	tessera_seal_keys_clear(&b);
}

int
main(void)
{
	static const uint32_t suites[] = { TESSERA_SUITE_XAES_256_GCM,
	    TESSERA_SUITE_XCHACHA20_POLY1305 };

	printf("test_seal: superblock HMAC, sealed blocks, journal chain, "
	    "blobs\n");
	tessera_crypt_openssl_install();
	test_keys();
	for (unsigned i = 0; i < 2; i++) {
		test_superblock(suites[i]);
		test_blocks(suites[i]);
		test_journal_chain(suites[i]);
		test_units(suites[i]);
	}
	if (failures > 0) {
		fprintf(stderr, "%d failure(s)\n", failures);
		return 1;
	}
	printf("ok\n");
	return 0;
}
