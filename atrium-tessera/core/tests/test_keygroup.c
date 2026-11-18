/*
 * Key groups (docs/spec/tessera-encryption.md §4.2, §5.3, §5.4) on the
 * OpenSSL backend, both suites:
 *
 *   1. Group keys: every subkey differs from the others and from another
 *      group's or another volume's; group 0 and an id past the cap are
 *      refused.
 *   2. Group records: made, then verified; the wrong key is ENOKEY; a
 *      rolled-back generation (or any other byte changed) without a new MAC
 *      is EAUTH; resealing after a real change verifies.
 *   3. Domain records: a domain put in the group verifies; stripping the
 *      group, naming another, changing the salt, the policy, the root or
 *      the id is refused (EAUTH); K_gid[d] differs for a salted domain.
 *   4. Group and domain records share the quota tree: group keys carry the
 *      0xFF prefix, domains don't, and both round-trip.
 *   5. Keyed names: differ from the plain hash and between groups; journal
 *      names round-trip, are not in the clear, and don't open for another
 *      inode or group.
 */
#include "tessera/btree.h"
#include "tessera/crypt.h"
#include "tessera/error.h"
#include "tessera/format.h"
#include "tessera/manifest.h"
#include "tessera/quota.h"
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

static const uint8_t GK[32] = {
	9, 8, 7, 6, 5, 4, 3, 2, 1, 0, 11, 12, 13, 14, 15, 16,
	17, 18, 19, 20, 21, 22, 23, 24, 25, 26, 27, 28, 29, 30, 31, 32 };
static const uint8_t UUID[16] = {
	0xb0, 0xb1, 0xb2, 0xb3, 0xb4, 0xb5, 0xb6, 0xb7,
	0xb8, 0xb9, 0xba, 0xbb, 0xbc, 0xbd, 0xbe, 0xbf };
static const uint8_t KEY_ID[16] = { 0x11, 0x22, 0x33 };

static void
test_keys(uint32_t suite)
{
	tessera_group_keys_t a, b, c;
	uint8_t other_uuid[16];

	memcpy(other_uuid, UUID, 16);
	other_uuid[15] ^= 1;
	CHECK(tessera_group_keys_init(&a, suite, GK, UUID, 7) == TESSERA_OK);
	CHECK(tessera_group_keys_init(&b, suite, GK, UUID, 8) == TESSERA_OK);
	CHECK(tessera_group_keys_init(&c, suite, GK, other_uuid, 7) == TESSERA_OK);
	const uint8_t *k[5] = { a.gmeta.key, a.gblob.key, a.gid, a.name, a.cdc };
	for (int i = 0; i < 5; i++)
		for (int j = i + 1; j < 5; j++)
			CHECK(memcmp(k[i], k[j], 32) != 0);
	CHECK(memcmp(a.gmeta.key, b.gmeta.key, 32) != 0);
	CHECK(memcmp(a.gblob.key, c.gblob.key, 32) != 0);
	CHECK(memcmp(a.check, b.check, 32) != 0);
	CHECK(tessera_group_keys_init(&b, suite, GK, UUID, 0) == TESSERA_EINVAL);
	CHECK(tessera_group_keys_init(&b, suite, GK, UUID,
	    TESSERA_KEY_GROUP_ID_MAX + 1) == TESSERA_EINVAL);
	tessera_group_keys_clear(&a);
	CHECK(a.group_id == 0 && a.gmeta.key[0] == 0 && a.gmeta.key[31] == 0);
	tessera_group_keys_clear(&c);
}

static void
test_group_records(uint32_t suite)
{
	tessera_group_keys_t g, wrong;
	tessera_key_group_t rec, t;
	uint8_t other[32];

	memcpy(other, GK, 32);
	other[0] ^= 0x80;
	CHECK(tessera_group_keys_init(&g, suite, GK, UUID, 7) == TESSERA_OK);
	CHECK(tessera_group_keys_init(&wrong, suite, other, UUID, 7) == TESSERA_OK);
	CHECK(tessera_key_group_make(&g, KEY_ID, suite, 41, &rec) == TESSERA_OK);
	CHECK(rec.group_id == 7 && rec.generation == 41 && rec.suite == suite);
	CHECK(memcmp(rec.key_id, KEY_ID, 16) == 0);
	CHECK(tessera_key_group_verify(&g, &rec) == TESSERA_OK);
	CHECK(tessera_key_group_verify(&wrong, &rec) == TESSERA_ENOKEY);

	/* Rolled back without the key: refused. */
	t = rec;
	t.generation = 40;
	CHECK(tessera_key_group_verify(&g, &t) == TESSERA_EAUTH);
	/* Any byte before the MAC, past check (check itself is ENOKEY). */
	for (size_t i = offsetof(tessera_key_group_t, suite);
	    i < offsetof(tessera_key_group_t, mac); i++) {
		t = rec;
		((uint8_t *)&t)[i] ^= 1;
		CHECK(tessera_key_group_verify(&g, &t) == TESSERA_EAUTH);
	}
	t = rec;
	t.mac[3] ^= 1;
	CHECK(tessera_key_group_verify(&g, &t) == TESSERA_EAUTH);
	/* With the key: a new generation verifies. */
	t = rec;
	t.generation = 42;
	CHECK(tessera_key_group_reseal(&g, &t) == TESSERA_OK);
	CHECK(tessera_key_group_verify(&g, &t) == TESSERA_OK);
	tessera_group_keys_clear(&g);
	tessera_group_keys_clear(&wrong);
}

static void
test_domains(uint32_t suite)
{
	tessera_group_keys_t g, h;
	tessera_quota_domain_t d, t;
	uint8_t k1[32], k2[32];

	CHECK(tessera_group_keys_init(&g, suite, GK, UUID, 7) == TESSERA_OK);
	CHECK(tessera_group_keys_init(&h, suite, GK, UUID, 9) == TESSERA_OK);
	tessera_quota_domain_init(&d, 3, 1234, 1 << 20);
	CHECK(tessera_domain_verify_group(&g, &d) == TESSERA_EAUTH);
	CHECK(tessera_domain_seal_group(&g, &d) == TESSERA_OK);
	CHECK(d.key_group == 7);
	CHECK(tessera_domain_verify_group(&g, &d) == TESSERA_OK);
	CHECK(tessera_domain_verify_group(&h, &d) == TESSERA_EAUTH);

	/* Usage and limits aren't covered: they change on every write. */
	t = d;
	t.used_bytes += 4096;
	t.limit_bytes = 0;
	CHECK(tessera_domain_verify_group(&g, &t) == TESSERA_OK);
	/* Everything the group depends on is. */
	t = d; t.key_group = 0;
	CHECK(tessera_domain_verify_group(&g, &t) == TESSERA_EAUTH);
	t = d; t.key_group = 9;
	CHECK(tessera_domain_verify_group(&h, &t) == TESSERA_EAUTH);
	t = d; t.domain_salt[5] ^= 1;
	CHECK(tessera_domain_verify_group(&g, &t) == TESSERA_EAUTH);
	t = d; t.dedup_policy ^= 1;
	CHECK(tessera_domain_verify_group(&g, &t) == TESSERA_EAUTH);
	t = d; t.root_inode_no++;
	CHECK(tessera_domain_verify_group(&g, &t) == TESSERA_EAUTH);
	t = d; t.domain_id++;
	CHECK(tessera_domain_verify_group(&g, &t) == TESSERA_EAUTH);

	/* K_gid[d]: the group's for unsalted domains, per salt otherwise. */
	CHECK(tessera_group_domain_id_key(&g, &d, k1) == TESSERA_OK);
	CHECK(memcmp(k1, g.gid, 32) == 0);
	t = d;
	t.dedup_policy = TESSERA_DEDUP_SALTED;
	memset(t.domain_salt, 0x5a, 32);
	CHECK(tessera_group_domain_id_key(&g, &t, k2) == TESSERA_OK);
	CHECK(memcmp(k1, k2, 32) != 0);
	t.domain_salt[0] ^= 1;
	CHECK(tessera_group_domain_id_key(&g, &t, k1) == TESSERA_OK);
	CHECK(memcmp(k1, k2, 32) != 0);
	tessera_group_keys_clear(&g);
	tessera_group_keys_clear(&h);
}

/* ── an in-memory disk for the quota tree ─────────────────────────── */

#define MAX_SECTORS 512u
struct mem_disk {
	uint8_t  blocks[MAX_SECTORS][TESSERA_SECTOR_SIZE];
	uint8_t  used[MAX_SECTORS];
};
static int mem_read(void *c, uint64_t s, uint8_t *o) {
	struct mem_disk *d = c;
	if (s >= MAX_SECTORS || !d->used[s]) return -1;
	memcpy(o, d->blocks[s], TESSERA_SECTOR_SIZE); return 0;
}
static int mem_write(void *c, uint64_t s, const uint8_t *b) {
	struct mem_disk *d = c;
	if (s >= MAX_SECTORS) return -1;
	memcpy(d->blocks[s], b, TESSERA_SECTOR_SIZE); return 0;
}
static int mem_alloc(void *c, uint64_t n, uint64_t *o) {
	struct mem_disk *d = c;
	if (n != 1) return -1;
	for (uint64_t i = 1; i < MAX_SECTORS; i++)
		if (!d->used[i]) { d->used[i] = 1; *o = i; return 0; }
	return -1;
}
static int mem_free(void *c, uint64_t s, uint64_t n) {
	struct mem_disk *d = c;
	if (n != 1 || s >= MAX_SECTORS || !d->used[s]) return -1;
	d->used[s] = 0; return 0;
}

static void
test_shared_tree(uint32_t suite)
{
	struct mem_disk *disk = calloc(1, sizeof(*disk));
	tessera_block_io_t io = { .read_block = mem_read,
	    .write_block = mem_write, .alloc = mem_alloc, .free = mem_free,
	    .ctx = disk };
	tessera_group_keys_t g;
	tessera_key_group_t rec, got;
	tessera_quota_domain_t d, gd;
	uint64_t root;
	uint8_t key[8];

	CHECK(tessera_group_keys_init(&g, suite, GK, UUID, 7) == TESSERA_OK);
	tessera_btree_t *t = tessera_btree_create(&io, TESSERA_BTREE_KIND_QUOTA,
	    8, TESSERA_QUOTA_DOMAIN_SIZE, &root);
	CHECK(t != NULL);
	if (t == NULL)
		return;
	tessera_quota_domain_init(&d, 7, 99, 0);    /* same number as the group */
	CHECK(tessera_domain_seal_group(&g, &d) == TESSERA_OK);
	CHECK(tessera_quota_store_put(t, &d, &root) == TESSERA_OK);
	CHECK(tessera_key_group_make(&g, KEY_ID, suite, 5, &rec) == TESSERA_OK);
	CHECK(tessera_key_group_store_put(t, &rec, &root) == TESSERA_OK);

	CHECK(tessera_quota_store_get(t, 7, &gd) == TESSERA_OK);
	CHECK(memcmp(&gd, &d, sizeof(d)) == 0);
	CHECK(tessera_domain_verify_group(&g, &gd) == TESSERA_OK);
	CHECK(tessera_key_group_store_get(t, 7, &got) == TESSERA_OK);
	CHECK(memcmp(&got, &rec, sizeof(rec)) == 0);
	CHECK(tessera_key_group_verify(&g, &got) == TESSERA_OK);
	CHECK(tessera_key_group_store_get(t, 8, &got) == TESSERA_ENOENT);
	CHECK(tessera_key_group_store_get(t, 0, &got) == TESSERA_EINVAL);

	/* A walk sees both; only the group's key carries the prefix. */
	int domains = 0, groups = 0;
	tessera_btree_cursor_t *c = tessera_btree_seek_first(t);
	uint8_t v[TESSERA_QUOTA_DOMAIN_SIZE];
	while (c != NULL && tessera_btree_cursor_get(c, key, v) == TESSERA_OK) {
		if (tessera_quota_key_is_group(key))
			groups++;
		else
			domains++;
		if (tessera_btree_cursor_next(c) != TESSERA_OK)
			break;
	}
	if (c != NULL)
		tessera_btree_cursor_free(c);
	CHECK(domains == 1 && groups == 1);
	tessera_key_group_key(7, key);
	CHECK(key[0] == 0xFF && key[7] == 7);
	tessera_quota_key(7, key);
	CHECK(!tessera_quota_key_is_group(key));
	tessera_btree_close(t);
	free(disk);
	tessera_group_keys_clear(&g);
}

static void
test_names(uint32_t suite)
{
	tessera_group_keys_t g, h;
	static const char name[] = "Holiday photos";
	const size_t n = sizeof(name) - 1;
	uint8_t sealed[64 + 40], back[64];

	CHECK(tessera_group_keys_init(&g, suite, GK, UUID, 7) == TESSERA_OK);
	CHECK(tessera_group_keys_init(&h, suite, GK, UUID, 9) == TESSERA_OK);
	uint64_t kh = tessera_dir_name_hash_keyed(g.name, name, n);
	CHECK(kh != 0);
	CHECK(kh == tessera_dir_name_hash_keyed(g.name, name, n));
	CHECK(kh != tessera_dir_name_hash(name, n));
	CHECK(kh != tessera_dir_name_hash_keyed(h.name, name, n));
	CHECK(kh != tessera_dir_name_hash_keyed(g.name, name, n - 1));

	CHECK(tessera_group_name_seal(&g, 1234, (const uint8_t *)name, n,
	    sealed) == TESSERA_OK);
	for (size_t i = 0; i + 7 <= n + 40; i++)
		CHECK(memcmp(sealed + i, name, 7) != 0);
	CHECK(tessera_group_name_open(&g, 1234, sealed, n + 40, back) ==
	    TESSERA_OK);
	CHECK(memcmp(back, name, n) == 0);
	CHECK(tessera_group_name_open(&g, 1235, sealed, n + 40, back) ==
	    TESSERA_EAUTH);
	CHECK(tessera_group_name_open(&h, 1234, sealed, n + 40, back) ==
	    TESSERA_EAUTH);
	sealed[30] ^= 1;
	CHECK(tessera_group_name_open(&g, 1234, sealed, n + 40, back) ==
	    TESSERA_EAUTH);
	tessera_group_keys_clear(&g);
	tessera_group_keys_clear(&h);
}

int
main(void)
{
	static const uint32_t suites[] = { TESSERA_SUITE_XAES_256_GCM,
	    TESSERA_SUITE_XCHACHA20_POLY1305 };

	printf("test_keygroup: group keys, group and domain records, "
	    "keyed names\n");
	tessera_crypt_openssl_install();
	for (unsigned i = 0; i < 2; i++) {
		test_keys(suites[i]);
		test_group_records(suites[i]);
		test_domains(suites[i]);
		test_shared_tree(suites[i]);
		test_names(suites[i]);
	}
	if (failures > 0) {
		fprintf(stderr, "%d failure(s)\n", failures);
		return 1;
	}
	printf("ok\n");
	return 0;
}
