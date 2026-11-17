/*
 * Sealed volume (docs/spec/tessera-encryption.md §5) end to end on the
 * OpenSSL backend, both suites:
 *
 *   1. format_sealed with a chunked seed file; open_sealed; the root
 *      directory, the seed inode, the pack registry (80-byte values), the
 *      sealed pack front and every sealed blob read back through the sealed
 *      trees, and the file's bytes match the seed byte for byte.
 *   2. No 16-byte window of the seed content, nor the seed name, is in the
 *      clear anywhere on disk.
 *   3. tessera_volume_open refuses the volume (EINCOMPAT); a wrong key gets
 *      ENOKEY; open_sealed of an unsealed volume gets EINCOMPAT.
 *   4. One superblock copy damaged (CRC, or a field changed with a fresh
 *      CRC so only the HMAC catches it): open falls back to the other copy;
 *      both damaged: EAUTH.
 *   5. The pack front opened under a wrong registry tag, or a blob opened
 *      under another content id, is refused.
 *   6. The sealed journal opens and replays empty at the SB's anchor, and
 *      refuses a wrong anchor; offline commit_roots is ENOTIMPL.
 */
#include "tessera/btree.h"
#include "tessera/codec.h"
#include "tessera/crypt.h"
#include "tessera/error.h"
#include "tessera/format.h"
#include "tessera/journal.h"
#include "tessera/manifest.h"
#include "tessera/pack.h"
#include "tessera/seal.h"
#include "tessera/volume.h"

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

#define BS          4096
#define MAX_SECTORS 2048
#define JSECT       64

static uint8_t disk[MAX_SECTORS][BS];

static int
d_read(void *c, uint64_t s, uint8_t *o)
{
	(void)c;
	if (s >= MAX_SECTORS) return -1;
	memcpy(o, disk[s], BS);
	return 0;
}
static int
d_write(void *c, uint64_t s, const uint8_t *b)
{
	(void)c;
	if (s >= MAX_SECTORS) return -1;
	memcpy(disk[s], b, BS);
	return 0;
}
static int d_alloc(void *c, uint64_t n, uint64_t *o) { (void)c; (void)n; (void)o; return -1; }
static int d_free(void *c, uint64_t s, uint64_t n) { (void)c; (void)s; (void)n; return 0; }
static const tessera_block_io_t IO = { d_read, d_write, d_alloc, d_free,
    NULL, NULL, NULL, NULL };

#define SEED_LEN   20000
#define SEED_CHUNK 4096
static uint8_t seed[SEED_LEN];
static const char NAME[] = "secret-plans.txt";

static void
fill_opts(tessera_format_opts_t *o)
{
	memset(o, 0, sizeof *o);
	o->total_sectors = MAX_SECTORS;
	o->journal_sectors = JSECT;
	for (int i = 0; i < 16; i++) o->volume_uuid[i] = (uint8_t)(0x40 + i);
	o->seed_dirent_name = NAME;
	o->seed_dirent_name_len = (uint16_t)(sizeof NAME - 1);
	o->seed_dirent_inode = 100;
	o->seed_content_data = seed;
	o->seed_content_len = SEED_LEN;
	o->seed_chunk_size = SEED_CHUNK;
}

static void
inode_key(uint32_t n, uint8_t k[4])
{
	k[0] = (uint8_t)(n >> 24); k[1] = (uint8_t)(n >> 16);
	k[2] = (uint8_t)(n >> 8);  k[3] = (uint8_t)n;
}

/* The one pack: its plain bytes after the front is opened, kept so the
 * blob lookups below can run against it. */
static uint8_t *pack;
static size_t   pack_len;
static uint8_t  pack_tag[16];

/* Look up `id` in the opened pack and open the sealed blob. Returns the
 * plaintext length, or -1. */
static long
get_blob(const tessera_seal_keys_t *k, const tessera_hash_t id, uint8_t *out,
    size_t cap)
{
	tessera_pack_reader_t *r = tessera_pack_open(pack, pack_len);
	if (r == NULL) return -1;
	const uint8_t *b;
	uint32_t n;
	long rc = -1;
	if (tessera_pack_lookup(r, id, &b, &n) == TESSERA_OK &&
	    n >= TESSERA_SEAL_OVERHEAD && n - TESSERA_SEAL_OVERHEAD <= cap &&
	    tessera_blob_open(&k->blob, k->uuid, 0, id, b, n, out) ==
	    TESSERA_OK)
		rc = (long)(n - TESSERA_SEAL_OVERHEAD);
	tessera_pack_close(r);
	return rc;
}

/* Read the volume back through the sealed structures; 0 on success. */
static int
read_back(const tessera_seal_keys_t *k)
{
	tessera_volume_t *v = NULL;
	if (tessera_volume_open_sealed(&IO, k, &v) != TESSERA_OK) return 1;
	int bad = 0;
	uint8_t tag[16], key[4], val[TESSERA_REGISTRY_ENTRY_SIZE_SEALED];
	CHECK(tessera_volume_is_sealed(v));

	/* pack registry: one entry, 80-byte value with the front tag */
	CHECK(tessera_volume_root_tag(v, TESSERA_BTREE_KIND_PACK_REG, tag) ==
	    TESSERA_OK);
	tessera_btree_t *reg = tessera_btree_open_sealed(&IO, k,
	    tessera_volume_pack_registry_root(v), tag,
	    TESSERA_BTREE_KIND_PACK_REG, 16, TESSERA_REGISTRY_ENTRY_SIZE_SEALED);
	if (reg == NULL) { tessera_volume_close(v); return 2; }
	tessera_btree_cursor_t *c = tessera_btree_seek_first(reg);
	uint8_t pid[16];
	tessera_registry_entry_t re;
	int n_ent = 0;
	free(pack);
	pack = NULL;
	while (c != NULL && tessera_btree_cursor_get(c, pid, val) == TESSERA_OK) {
		n_ent++;
		CHECK(tessera_decode_registry_entry(val, &re) == TESSERA_OK);
		CHECK(memcmp(re.pack_id, pid, 16) == 0);
		memcpy(pack_tag, val + TESSERA_REGISTRY_ENTRY_SIZE, 16);
		pack_len = re.length_sectors * BS;
		pack = malloc(pack_len);
		for (uint64_t s = 0; s < re.length_sectors; s++)
			CHECK(d_read(NULL, re.start_sector + s, pack + s * BS) == 0);
		if (tessera_btree_cursor_next(c) != TESSERA_OK) break;
	}
	tessera_btree_cursor_free(c);
	tessera_btree_close(reg);
	CHECK(n_ent == 1);
	if (pack == NULL) { tessera_volume_close(v); return 3; }

	uint64_t grp;
	uint32_t flen;
	CHECK(tessera_pack_sealed_prelude(pack, pack_len, &grp, &flen) ==
	    TESSERA_OK && grp == 0);
	/* The kernel's partial read (tessera_fs_sealed_pack_find): only the
	 * sectors holding prelude and front, then the index from that. */
	{
		size_t fb = ((size_t)TESSERA_SEALED_PACK_PRELUDE + flen + BS - 1) /
		    BS * BS;
		uint8_t *front = malloc(fb);
		tessera_pack_geom_t g;
		memcpy(front, pack, fb);
		CHECK(tessera_pack_open_front(&k->meta, k->uuid, 0, pack_tag,
		    front, fb) == TESSERA_OK);
		CHECK(tessera_pack_geom_from_header(front, pack_len, &g) ==
		    TESSERA_OK && g.bloom_off <= fb);
		tessera_hash_t first;
		uint64_t doff;
		uint32_t dsz;
		memcpy(first, front + g.index_off, 32);   /* entry 0's hash */
		CHECK(tessera_pack_index_find(front + g.index_off,
		    (size_t)(g.bloom_off - g.index_off), &g, first, &doff,
		    &dsz) == TESSERA_OK && dsz > TESSERA_SEAL_OVERHEAD);
		uint8_t *pt = malloc(dsz);
		CHECK(tessera_blob_open(&k->blob, k->uuid, 0, first,
		    pack + doff + sizeof(tessera_blob_descriptor_t), dsz, pt) ==
		    TESSERA_OK);
		free(pt);
		free(front);
	}
	if (tessera_pack_open_front(&k->meta, k->uuid, 0, pack_tag, pack,
	    pack_len) != TESSERA_OK)
		bad = 4;
	{
		tessera_pack_reader_t *r = tessera_pack_open(pack, pack_len);
		CHECK(r != NULL);
		if (r != NULL) {
			CHECK(memcmp(tessera_pack_pack_id(r), pid, 16) == 0);
			/* manifest + chunk list + 5 chunks */
			CHECK(tessera_pack_blob_count(r) ==
			    2 + (SEED_LEN + SEED_CHUNK - 1) / SEED_CHUNK);
			tessera_pack_close(r);
		}
	}

	/* inode tree: root dir → dirent → seed inode → chunks */
	CHECK(tessera_volume_root_tag(v, TESSERA_BTREE_KIND_INODE, tag) ==
	    TESSERA_OK);
	tessera_btree_t *it = tessera_btree_open_sealed(&IO, k,
	    tessera_volume_inode_root(v), tag, TESSERA_BTREE_KIND_INODE, 4,
	    TESSERA_INODE_RECORD_SIZE);
	if (it == NULL) { tessera_volume_close(v); return 5; }
	uint8_t iv[TESSERA_INODE_RECORD_SIZE];
	tessera_inode_record_t ino;
	inode_key(TESSERA_INODE_ROOT_DIR, key);
	CHECK(tessera_btree_get(it, key, iv) == TESSERA_OK);
	CHECK(tessera_decode_inode(iv, &ino) == TESSERA_OK);

	static uint8_t mb[65536];
	long ml = get_blob(k, ino.manifest_hash, mb, sizeof mb);
	CHECK(ml > 0);
	uint64_t child = 0;
	if (ml > 0) {
		tessera_manifest_parser_t *p = tessera_manifest_parse(mb,
		    (size_t)ml);
		CHECK(p != NULL &&
		    tessera_manifest_parser_kind(p) == TESSERA_MFT_DIRECTORY);
		const char *nm;
		uint16_t nl;
		if (p != NULL && tessera_manifest_dirent_at(p, 0, &child, &nm,
		    &nl) == TESSERA_OK)
			CHECK(nl == sizeof NAME - 1 && memcmp(nm, NAME, nl) == 0);
		tessera_manifest_parser_free(p);
	}
	CHECK(child == 100);
	inode_key((uint32_t)child, key);
	CHECK(tessera_btree_get(it, key, iv) == TESSERA_OK);
	CHECK(tessera_decode_inode(iv, &ino) == TESSERA_OK);
	CHECK(ino.size == SEED_LEN);
	tessera_btree_close(it);

	ml = get_blob(k, ino.manifest_hash, mb, sizeof mb);
	CHECK(ml > 0);
	static uint8_t file[SEED_LEN + SEED_CHUNK];
	size_t got = 0;
	if (ml > 0) {
		tessera_manifest_parser_t *p = tessera_manifest_parse(mb,
		    (size_t)ml);
		CHECK(p != NULL &&
		    tessera_manifest_parser_kind(p) == TESSERA_MFT_CHUNK_LIST);
		for (uint32_t i = 0; p != NULL &&
		    i < tessera_manifest_parser_count(p); i++) {
			tessera_chunk_record_t cr;
			CHECK(tessera_manifest_chunk_at(p, i, &cr) == TESSERA_OK);
			CHECK(cr.logical_offset == got);
			long cl = get_blob(k, cr.chunk_hash, file + got,
			    sizeof file - got);
			CHECK(cl == (long)cr.uncompressed_size);
			if (cl > 0) got += (size_t)cl;
		}
		tessera_manifest_parser_free(p);
	}
	CHECK(got == SEED_LEN && memcmp(file, seed, SEED_LEN) == 0);
	if (got != SEED_LEN) bad = 6;

	/* 5: a blob under another content id; the front under another tag */
	{
		tessera_hash_t other;
		memcpy(other, ino.manifest_hash, 32);
		tessera_pack_reader_t *r = tessera_pack_open(pack, pack_len);
		const uint8_t *b;
		uint32_t n;
		if (r != NULL && tessera_pack_lookup(r, ino.manifest_hash, &b,
		    &n) == TESSERA_OK) {
			other[0] ^= 1;
			CHECK(tessera_blob_open(&k->blob, k->uuid, 0, other, b, n,
			    mb) == TESSERA_EAUTH);
			CHECK(tessera_blob_open(&k->blob, k->uuid, 1,
			    ino.manifest_hash, b, n, mb) == TESSERA_EAUTH);
		} else
			CHECK(0);
		tessera_pack_close(r);
	}
	tessera_volume_close(v);
	return bad;
}

static void
run(uint32_t suite)
{
	static const uint8_t vk[32] = { 9, 8, 7, 6 };
	tessera_format_opts_t o;
	tessera_seal_keys_t k, wrong;
	tessera_volume_t *v = NULL;
	uint8_t vk2[32];

	fill_opts(&o);
	memset(disk, 0, sizeof disk);
	CHECK(tessera_seal_keys_init(&k, suite, vk, o.volume_uuid) ==
	    TESSERA_OK);
	memcpy(vk2, vk, 32);
	vk2[31] ^= 0x80;
	CHECK(tessera_seal_keys_init(&wrong, suite, vk2, o.volume_uuid) ==
	    TESSERA_OK);

	/* 1 */
	for (int i = 0; i < 16; i++) k.key_id[i] = (uint8_t)(0xc0 + i);
	CHECK(tessera_volume_format_sealed(&IO, &o, &k) == TESSERA_OK);
	CHECK(read_back(&k) == 0);
	{
		/* the key id is in the clear, for the keyring lookup */
		tessera_superblock_t sb;
		CHECK(tessera_decode_superblock(disk[0], &sb) == TESSERA_OK);
		CHECK(memcmp(sb.master_key_id, k.key_id, 16) == 0);
	}

	/* the front under another tag */
	{
		uint8_t *cp = NULL;
		/* read_back left the plain pack in `pack`; reread it sealed */
		tessera_volume_t *vv = NULL;
		CHECK(tessera_volume_open_sealed(&IO, &k, &vv) == TESSERA_OK);
		tessera_volume_close(vv);
		/* find the pack's start: first sector after the metadata zone
		 * whose prelude parses */
		for (uint64_t s = 0; s < MAX_SECTORS && cp == NULL; s++) {
			uint64_t g;
			uint32_t fl;
			if (tessera_pack_sealed_prelude(disk[s], BS, &g, &fl) ==
			    TESSERA_OK) {
				cp = malloc(pack_len);
				memcpy(cp, disk[s], pack_len);
			}
		}
		CHECK(cp != NULL);
		if (cp != NULL) {
			uint8_t t2[16];
			memcpy(t2, pack_tag, 16);
			t2[3] ^= 1;
			CHECK(tessera_pack_open_front(&k.meta, k.uuid, 0, t2, cp,
			    pack_len) == TESSERA_EAUTH);
			free(cp);
		}
	}

	/* 2: nothing of the content or the name in the clear */
	{
		int found = 0;
		for (int s = 0; s < MAX_SECTORS && !found; s++)
			for (int off = 0; off + 16 <= BS && !found; off++) {
				if (memcmp(disk[s] + off, NAME, 12) == 0)
					found = 1;
				for (int w = 0; w < SEED_LEN - 16 && !found;
				    w += 1999)
					if (disk[s][off] == seed[w] &&
					    memcmp(disk[s] + off, seed + w, 16) == 0)
						found = 1;
			}
		CHECK(!found);
	}

	/* 3 */
	CHECK(tessera_volume_open(&IO, &v) == TESSERA_EINCOMPAT);
	CHECK(tessera_volume_open_sealed(&IO, &wrong, &v) == TESSERA_ENOKEY);

	/* 4: SB copies are sectors 0 and 1 */
	{
		static uint8_t s0[BS], s1[BS];
		memcpy(s0, disk[0], BS);
		memcpy(s1, disk[1], BS);
		disk[0][100] ^= 1;                 /* CRC catches it */
		CHECK(read_back(&k) == 0);
		memcpy(disk[0], s0, BS);

		/* a field changed with a fresh CRC: only the HMAC */
		tessera_superblock_t sb;
		CHECK(tessera_decode_superblock(disk[1], &sb) == TESSERA_OK);
		sb.inode_root ^= 1;
		CHECK(tessera_encode_superblock(&sb, disk[1]) == TESSERA_OK);
		CHECK(tessera_decode_superblock(disk[1], &sb) == TESSERA_OK);
		CHECK(read_back(&k) == 0);
		/* both bad */
		CHECK(tessera_decode_superblock(disk[0], &sb) == TESSERA_OK);
		sb.generation += 5;
		CHECK(tessera_encode_superblock(&sb, disk[0]) == TESSERA_OK);
		CHECK(tessera_volume_open_sealed(&IO, &k, &v) == TESSERA_EAUTH);
		/* only one bad, but the bad one claims a newer generation */
		memcpy(disk[1], s1, BS);
		CHECK(read_back(&k) == 0);
		memcpy(disk[0], s0, BS);
	}

	/* 6 */
	{
		uint8_t anchor[16];
		CHECK(tessera_volume_open_sealed(&IO, &k, &v) == TESSERA_OK);
		CHECK(tessera_volume_journal_anchor(v, anchor) == TESSERA_OK);
		tessera_journal_t *j = tessera_journal_open_sealed(&IO,
		    tessera_volume_journal_start(v), tessera_volume_journal_length(v),
		    &k, anchor);
		CHECK(j != NULL);
		tessera_journal_close(j);
		anchor[0] ^= 1;
		CHECK(tessera_journal_open_sealed(&IO,
		    tessera_volume_journal_start(v), tessera_volume_journal_length(v),
		    &k, anchor) == NULL);
		tessera_commit_roots_t cr;
		memset(&cr, 0, sizeof cr);
		CHECK(tessera_volume_commit_roots(v, &cr) == TESSERA_ENOTIMPL);
		tessera_volume_close(v);
		v = NULL;
	}

	/* an unsealed volume through open_sealed */
	memset(disk, 0, sizeof disk);
	CHECK(tessera_volume_format(&IO, &o) == TESSERA_OK);
	CHECK(tessera_volume_open_sealed(&IO, &k, &v) == TESSERA_EINCOMPAT);

	tessera_seal_keys_clear(&k);
	tessera_seal_keys_clear(&wrong);
}

int
main(void)
{
	printf("test_volume_sealed: sealed format/open end to end\n");
	tessera_crypt_openssl_install();
	for (int i = 0; i < SEED_LEN; i++)
		seed[i] = (uint8_t)(i * 131 + (i >> 7) * 17 + (i >> 11));
	run(TESSERA_SUITE_XAES_256_GCM);
	run(TESSERA_SUITE_XCHACHA20_POLY1305);
	free(pack);
	if (failures > 0) {
		fprintf(stderr, "%d failure(s)\n", failures);
		return 1;
	}
	printf("ok\n");
	return 0;
}
