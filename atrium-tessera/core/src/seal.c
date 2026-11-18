/*
 * seal.c — the sealed on-disk format (tessera/seal.h,
 * docs/spec/tessera-encryption.md §5.2–§5.5). Tests: tests/test_seal.c.
 */
#include "tessera_compat.h"

#include "tessera/error.h"
#include "tessera/pack.h"
#include "tessera/seal.h"

static inline void
put_le64(uint8_t *p, uint64_t v)
{
	for (int i = 0; i < 8; i++)
		p[i] = (uint8_t)(v >> (8 * i));
}

int
tessera_ct_eq(const void *a, const void *b, size_t n)
{
	const volatile uint8_t *x = a, *y = b;
	uint8_t d = 0;

	for (size_t i = 0; i < n; i++)
		d |= x[i] ^ y[i];
	return (d == 0);
}

/* ── keys ──────────────────────────────────────────────────────────── */

int
tessera_seal_keys_init(tessera_seal_keys_t *k, uint32_t suite,
    const uint8_t vk[32], const uint8_t volume_uuid[16])
{
	uint8_t sub[32];
	int err;

	if (k == NULL || vk == NULL || volume_uuid == NULL)
		return (TESSERA_EINVAL);
	memset(k, 0, sizeof(*k));
	memcpy(k->uuid, volume_uuid, 16);
	err = tessera_subkey(vk, TESSERA_KDF_META, volume_uuid, 0, sub);
	if (err == TESSERA_OK)
		err = tessera_aead_key_init(&k->meta, suite, sub);
	if (err == TESSERA_OK)
		err = tessera_subkey(vk, TESSERA_KDF_BLOB, volume_uuid, 0, sub);
	if (err == TESSERA_OK)
		err = tessera_aead_key_init(&k->blob, suite, sub);
	if (err == TESSERA_OK)
		err = tessera_subkey(vk, TESSERA_KDF_ID, volume_uuid, 0, k->id);
	if (err == TESSERA_OK)
		err = tessera_key_check(vk, volume_uuid, 0, k->check);
	tessera_wipe(sub, sizeof(sub));
	if (err != TESSERA_OK)
		tessera_seal_keys_clear(k);
	return (err);
}

void
tessera_seal_keys_clear(tessera_seal_keys_t *k)
{
	if (k != NULL)
		tessera_wipe(k, sizeof(*k));
}

/* ── key groups ────────────────────────────────────────────────────── */

int
tessera_group_keys_init(tessera_group_keys_t *g, uint32_t suite,
    const uint8_t gk[32], const uint8_t volume_uuid[16], uint64_t group_id)
{
	uint8_t sub[32];
	int err;

	if (g == NULL || gk == NULL || volume_uuid == NULL || group_id == 0 ||
	    group_id > TESSERA_KEY_GROUP_ID_MAX)
		return (TESSERA_EINVAL);
	memset(g, 0, sizeof(*g));
	memcpy(g->uuid, volume_uuid, 16);
	g->group_id = group_id;
	err = tessera_subkey(gk, TESSERA_KDF_GMETA, volume_uuid, group_id, sub);
	if (err == TESSERA_OK)
		err = tessera_aead_key_init(&g->gmeta, suite, sub);
	if (err == TESSERA_OK)
		err = tessera_subkey(gk, TESSERA_KDF_GBLOB, volume_uuid,
		    group_id, sub);
	if (err == TESSERA_OK)
		err = tessera_aead_key_init(&g->gblob, suite, sub);
	if (err == TESSERA_OK)
		err = tessera_subkey(gk, TESSERA_KDF_GID, volume_uuid, group_id,
		    g->gid);
	if (err == TESSERA_OK)
		err = tessera_subkey(gk, TESSERA_KDF_NAME, volume_uuid, group_id,
		    g->name);
	if (err == TESSERA_OK)
		err = tessera_subkey(gk, TESSERA_KDF_CDC, volume_uuid, group_id,
		    g->cdc);
	if (err == TESSERA_OK)
		err = tessera_key_check(gk, volume_uuid, group_id, g->check);
	tessera_wipe(sub, sizeof(sub));
	if (err != TESSERA_OK)
		tessera_group_keys_clear(g);
	return (err);
}

void
tessera_group_keys_clear(tessera_group_keys_t *g)
{
	if (g != NULL)
		tessera_wipe(g, sizeof(*g));
}

/* HMAC-SHA-256 under K_gmeta's bytes, with a domain-separating label,
 * cut to 16 bytes. */
static int
group_mac(const tessera_group_keys_t *g, const char *label,
    const void *msg, size_t len, uint8_t out[16])
{
	uint8_t full[32];
	const uint8_t *parts[3] = { (const uint8_t *)label, g->uuid, msg };
	size_t lens[3] = { strlen(label), 16, len };
	int err = tessera_hmac_sha256_v(g->gmeta.key, 32, parts, lens, 3, full);

	if (err == TESSERA_OK)
		memcpy(out, full, 16);
	tessera_wipe(full, sizeof(full));
	return (err);
}

#define GROUP_REC_MAC_LEN  offsetof(tessera_key_group_t, mac)

int
tessera_key_group_reseal(const tessera_group_keys_t *g,
    tessera_key_group_t *rec)
{
	if (g == NULL || rec == NULL || rec->group_id != g->group_id)
		return (TESSERA_EINVAL);
	return (group_mac(g, "tessera group record", rec, GROUP_REC_MAC_LEN,
	    rec->mac));
}

int
tessera_key_group_make(const tessera_group_keys_t *g,
    const uint8_t key_id[16], uint32_t suite, uint64_t generation,
    tessera_key_group_t *out)
{
	if (g == NULL || key_id == NULL || out == NULL)
		return (TESSERA_EINVAL);
	memset(out, 0, sizeof(*out));
	out->group_id = g->group_id;
	memcpy(out->key_id, key_id, 16);
	memcpy(out->check, g->check, 32);
	out->suite = suite;
	out->generation = generation;
	return (tessera_key_group_reseal(g, out));
}

int
tessera_key_group_verify(const tessera_group_keys_t *g,
    const tessera_key_group_t *rec)
{
	uint8_t mac[16];
	int err;

	if (g == NULL || rec == NULL)
		return (TESSERA_EINVAL);
	if (rec->group_id != g->group_id)
		return (TESSERA_ENOKEY);
	if (!tessera_ct_eq(rec->check, g->check, 32))
		return (TESSERA_ENOKEY);
	err = group_mac(g, "tessera group record", rec, GROUP_REC_MAC_LEN, mac);
	if (err == TESSERA_OK && !tessera_ct_eq(mac, rec->mac, 16))
		err = TESSERA_EAUTH;
	return (err);
}

/* The domain fields the MAC covers (§5.4): id, root inode, dedup policy,
 * salt and group, little-endian where wider than a byte. */
static int
domain_mac(const tessera_group_keys_t *g, const tessera_quota_domain_t *d,
    uint8_t out[16])
{
	uint8_t m[8 + 8 + 1 + 32 + 8];

	put_le64(m, d->domain_id);
	put_le64(m + 8, d->root_inode_no);
	m[16] = d->dedup_policy;
	memcpy(m + 17, d->domain_salt, 32);
	put_le64(m + 49, d->key_group);
	return (group_mac(g, "tessera domain record", m, sizeof(m), out));
}

int
tessera_domain_seal_group(const tessera_group_keys_t *g,
    tessera_quota_domain_t *d)
{
	if (g == NULL || d == NULL)
		return (TESSERA_EINVAL);
	d->key_group = g->group_id;
	return (domain_mac(g, d, d->group_mac));
}

int
tessera_domain_verify_group(const tessera_group_keys_t *g,
    const tessera_quota_domain_t *d)
{
	uint8_t mac[16];
	int err;

	if (g == NULL || d == NULL)
		return (TESSERA_EINVAL);
	if (d->key_group != g->group_id)
		return (TESSERA_EAUTH);
	err = domain_mac(g, d, mac);
	if (err == TESSERA_OK && !tessera_ct_eq(mac, d->group_mac, 16))
		err = TESSERA_EAUTH;
	return (err);
}

int
tessera_group_domain_id_key(const tessera_group_keys_t *g,
    const tessera_quota_domain_t *d, uint8_t out[32])
{
	if (g == NULL || d == NULL || out == NULL)
		return (TESSERA_EINVAL);
	if (d->dedup_policy != TESSERA_DEDUP_SALTED) {
		memcpy(out, g->gid, 32);
		return (TESSERA_OK);
	}
	return (tessera_hmac_sha256(g->gid, 32, d->domain_salt, 32, out));
}

uint64_t
tessera_dir_name_hash_keyed(const uint8_t name_key[32], const char *name,
    size_t name_len)
{
	uint8_t h[32];
	uint64_t v = 0;

	if (tessera_hmac_sha256(name_key, 32, (const uint8_t *)name, name_len,
	    h) != TESSERA_OK)
		return (0);
	/* Big-endian, like tessera_dir_name_hash's first 8 digest bytes. */
	for (int i = 0; i < 8; i++)
		v = (v << 8) | h[i];
	tessera_wipe(h, sizeof(h));
	return (v);
}

int
tessera_group_name_seal(const tessera_group_keys_t *g, uint64_t inode_no,
    const uint8_t *name, size_t len, uint8_t *out)
{
	uint8_t tail[8];

	if (g == NULL || (name == NULL && len > 0) || out == NULL)
		return (TESSERA_EINVAL);
	put_le64(tail, inode_no);
	return (tessera_unit_seal(&g->gmeta, g->uuid,
	    TESSERA_SEAL_KIND_GROUP_NAME, g->group_id, tail, 8, name, len, out,
	    NULL));
}

int
tessera_group_name_open(const tessera_group_keys_t *g, uint64_t inode_no,
    const uint8_t *in, size_t in_len, uint8_t *name)
{
	uint8_t tail[8];

	if (g == NULL || in == NULL || name == NULL)
		return (TESSERA_EINVAL);
	put_le64(tail, inode_no);
	return (tessera_unit_open(&g->gmeta, g->uuid,
	    TESSERA_SEAL_KIND_GROUP_NAME, g->group_id, tail, 8, in, in_len,
	    NULL, name));
}

/* ── superblock ────────────────────────────────────────────────────── */

#define SB_CRC_OFF   offsetof(tessera_superblock_t, crc32)
#define SB_TAIL_LEN  (sizeof(tessera_superblock_t) - SB_CRC_OFF)

/* HMAC(K_meta, sb with crc32 and hmac zeroed). crc32 and hmac are the last
 * 36 bytes, so the message is the head followed by 36 zero bytes. */
static int
sb_hmac(const tessera_seal_keys_t *k, const tessera_superblock_t *sb,
    uint8_t out[32])
{
	static const uint8_t zero[SB_TAIL_LEN];
	const uint8_t *parts[2] = { (const uint8_t *)sb, zero };
	size_t lens[2] = { SB_CRC_OFF, SB_TAIL_LEN };

	_Static_assert(offsetof(tessera_superblock_t, hmac) ==
	    SB_CRC_OFF + 4 && SB_TAIL_LEN == 36, "crc32, hmac at the end");
	return (tessera_hmac_sha256_v(k->meta.key, 32, parts, lens, 2, out));
}

static size_t
stats_aad(const tessera_seal_keys_t *k, uint64_t generation, uint8_t aad[25])
{
	memcpy(aad, k->uuid, 16);
	aad[16] = TESSERA_SEAL_KIND_SB_STATS;
	put_le64(aad + 17, generation);
	return (25);
}

int
tessera_sb_seal(const tessera_seal_keys_t *k, tessera_superblock_t *sb,
    const tessera_sb_stats_t *stats)
{
	tessera_sb_stats_t st;
	uint8_t aad[25], *ss;
	size_t al;
	int err;

	_Static_assert(TESSERA_SB_SEALED_STATS_SIZE ==
	    sizeof(tessera_sb_stats_t) + TESSERA_SEAL_OVERHEAD, "stats size");
	if (k == NULL || sb == NULL)
		return (TESSERA_EINVAL);
	if (memcmp(sb->volume_uuid, k->uuid, 16) != 0)
		return (TESSERA_EINVAL);

	if (stats != NULL) {
		st = *stats;
	} else {
		memset(&st, 0, sizeof(st));
		st.total_blob_count = sb->total_blob_count;
		st.total_pack_count = sb->total_pack_count;
		st.format_time      = sb->format_time;
		st.last_mount_time  = sb->last_mount_time;
	}

	sb->incompat_flags   |= TESSERA_INCOMPAT_SEALED;
	sb->feature_flags    |= TESSERA_FEATURE_ENCRYPTED;
	sb->encryption_flags |= TESSERA_ENC_SEALED;
	sb->enc_suite         = (uint8_t)k->meta.suite;
	memcpy(sb->key_check, k->check, 32);
	memcpy(sb->master_key_id, k->key_id, 16);
	sb->total_blob_count = 0;
	sb->total_pack_count = 0;
	sb->format_time      = 0;
	sb->last_mount_time  = 0;

	ss = sb->sealed_stats;
	al = stats_aad(k, sb->generation, aad);
	err = tessera_aead_seal(&k->meta, ss, aad, al, (const uint8_t *)&st,
	    sizeof(st), ss + TESSERA_NONCE_SIZE,
	    ss + TESSERA_NONCE_SIZE + sizeof(st));
	tessera_wipe(&st, sizeof(st));
	if (err != TESSERA_OK)
		return (err);
	return (sb_hmac(k, sb, sb->hmac));
}

int
tessera_sb_verify(const tessera_seal_keys_t *k,
    const tessera_superblock_t *sb, tessera_sb_stats_t *stats)
{
	uint8_t mac[32], aad[25];
	const uint8_t *ss;
	size_t al;
	int err, ok;

	if (k == NULL || sb == NULL)
		return (TESSERA_EINVAL);
	if ((sb->incompat_flags & TESSERA_INCOMPAT_SEALED) == 0 ||
	    (sb->encryption_flags & TESSERA_ENC_SEALED) == 0)
		return (TESSERA_EINCOMPAT);
	if (sb->enc_suite != k->meta.suite)
		return (TESSERA_ENOKEY);
	if (memcmp(sb->volume_uuid, k->uuid, 16) != 0 ||
	    !tessera_ct_eq(sb->key_check, k->check, 32))
		return (TESSERA_ENOKEY);
	if ((err = sb_hmac(k, sb, mac)) != TESSERA_OK)
		return (err);
	ok = tessera_ct_eq(mac, sb->hmac, 32);
	tessera_wipe(mac, sizeof(mac));
	if (!ok)
		return (TESSERA_EAUTH);
	if (stats == NULL)
		return (TESSERA_OK);
	ss = sb->sealed_stats;
	al = stats_aad(k, sb->generation, aad);
	return (tessera_aead_open(&k->meta, ss, aad, al,
	    ss + TESSERA_NONCE_SIZE, sizeof(*stats),
	    ss + TESSERA_NONCE_SIZE + sizeof(*stats), (uint8_t *)stats));
}

/* ── metadata blocks ───────────────────────────────────────────────── */

#define BLOCK_AAD_LEN  (16 + 1 + 8 + 8 + 16)
#define B_NONCE        TESSERA_SEALED_BLOCK_PAYLOAD
#define B_TAG          (TESSERA_SEALED_BLOCK_PAYLOAD + TESSERA_NONCE_SIZE)

static void
block_aad(const tessera_seal_keys_t *k, uint8_t kind, uint64_t sector,
    uint64_t ctx, const uint8_t prev_tag[16], uint8_t aad[BLOCK_AAD_LEN])
{
	memcpy(aad, k->uuid, 16);
	aad[16] = kind;
	put_le64(aad + 17, sector);
	put_le64(aad + 25, ctx);
	if (prev_tag != NULL)
		memcpy(aad + 33, prev_tag, 16);
	else
		memset(aad + 33, 0, 16);
}

int
tessera_block_seal(const tessera_seal_keys_t *k, uint8_t kind,
    uint64_t sector, uint64_t ctx, const uint8_t prev_tag[16],
    uint8_t block[TESSERA_SECTOR_SIZE], uint8_t tag_out[16])
{
	uint8_t aad[BLOCK_AAD_LEN];
	int err;

	_Static_assert(B_TAG + TESSERA_TAG_SIZE == TESSERA_SECTOR_SIZE,
	    "sealed block trailer");
	if (k == NULL || block == NULL)
		return (TESSERA_EINVAL);
	block_aad(k, kind, sector, ctx, prev_tag, aad);
	err = tessera_aead_seal(&k->meta, block + B_NONCE, aad, sizeof(aad),
	    block, TESSERA_SEALED_BLOCK_PAYLOAD, block, block + B_TAG);
	if (err == TESSERA_OK && tag_out != NULL)
		memcpy(tag_out, block + B_TAG, 16);
	return (err);
}

int
tessera_block_open(const tessera_seal_keys_t *k, uint8_t kind,
    uint64_t sector, uint64_t ctx, const uint8_t prev_tag[16],
    const uint8_t expected_tag[16], uint8_t block[TESSERA_SECTOR_SIZE])
{
	uint8_t aad[BLOCK_AAD_LEN];

	if (k == NULL || block == NULL)
		return (TESSERA_EINVAL);
	if (expected_tag != NULL &&
	    !tessera_ct_eq(block + B_TAG, expected_tag, 16)) {
		tessera_wipe(block, TESSERA_SEALED_BLOCK_PAYLOAD);
		return (TESSERA_EAUTH);
	}
	block_aad(k, kind, sector, ctx, prev_tag, aad);
	return (tessera_aead_open(&k->meta, block + B_NONCE, aad, sizeof(aad),
	    block, TESSERA_SEALED_BLOCK_PAYLOAD, block + B_TAG, block));
}

const uint8_t *
tessera_block_tag(const uint8_t block[TESSERA_SECTOR_SIZE])
{
	return (block + B_TAG);
}

/* ── variable-length units ─────────────────────────────────────────── */

#define UNIT_AAD_MAX  (16 + 1 + 8 + TESSERA_SEAL_AAD_TAIL_MAX)

static int
unit_aad(const uint8_t uuid[16], uint8_t kind, uint64_t group,
    const uint8_t *tail, size_t tail_len, uint8_t aad[UNIT_AAD_MAX],
    size_t *out_len)
{
	if (uuid == NULL || tail_len > TESSERA_SEAL_AAD_TAIL_MAX ||
	    (tail_len > 0 && tail == NULL))
		return (TESSERA_EINVAL);
	memcpy(aad, uuid, 16);
	aad[16] = kind;
	put_le64(aad + 17, group);
	if (tail_len > 0)
		memcpy(aad + 25, tail, tail_len);
	*out_len = 25 + tail_len;
	return (TESSERA_OK);
}

int
tessera_unit_seal(const tessera_aead_key_t *key, const uint8_t volume_uuid[16],
    uint8_t kind, uint64_t key_group, const uint8_t *aad_tail, size_t tail_len,
    const uint8_t *pt, size_t len, uint8_t *out, uint8_t tag_out[16])
{
	uint8_t aad[UNIT_AAD_MAX];
	size_t al;
	int err;

	if (key == NULL || out == NULL)
		return (TESSERA_EINVAL);
	if ((err = unit_aad(volume_uuid, kind, key_group, aad_tail, tail_len,
	    aad, &al)) != TESSERA_OK)
		return (err);
	err = tessera_aead_seal(key, out, aad, al, pt, len,
	    out + TESSERA_NONCE_SIZE, out + TESSERA_NONCE_SIZE + len);
	if (err == TESSERA_OK && tag_out != NULL)
		memcpy(tag_out, out + TESSERA_NONCE_SIZE + len, 16);
	return (err);
}

int
tessera_unit_open(const tessera_aead_key_t *key, const uint8_t volume_uuid[16],
    uint8_t kind, uint64_t key_group, const uint8_t *aad_tail, size_t tail_len,
    const uint8_t *in, size_t in_len, const uint8_t expected_tag[16],
    uint8_t *pt)
{
	uint8_t aad[UNIT_AAD_MAX];
	size_t al, len;
	int err;

	if (key == NULL || in == NULL || in_len < TESSERA_SEAL_OVERHEAD)
		return (TESSERA_EINVAL);
	len = in_len - TESSERA_SEAL_OVERHEAD;
	if (expected_tag != NULL &&
	    !tessera_ct_eq(in + TESSERA_NONCE_SIZE + len, expected_tag, 16)) {
		if (len > 0 && pt != NULL)
			tessera_wipe(pt, len);
		return (TESSERA_EAUTH);
	}
	if ((err = unit_aad(volume_uuid, kind, key_group, aad_tail, tail_len,
	    aad, &al)) != TESSERA_OK)
		return (err);
	return (tessera_aead_open(key, in, aad, al, in + TESSERA_NONCE_SIZE,
	    len, in + TESSERA_NONCE_SIZE + len, pt));
}

int
tessera_blob_seal(const tessera_aead_key_t *key, const uint8_t volume_uuid[16],
    uint64_t key_group, const tessera_hash_t id, const uint8_t *pt, size_t len,
    uint8_t *out)
{
	return (tessera_unit_seal(key, volume_uuid, TESSERA_SEAL_KIND_BLOB,
	    key_group, id, TESSERA_HASH_SIZE, pt, len, out, NULL));
}

int
tessera_blob_open(const tessera_aead_key_t *key, const uint8_t volume_uuid[16],
    uint64_t key_group, const tessera_hash_t id, const uint8_t *in,
    size_t in_len, uint8_t *pt)
{
	return (tessera_unit_open(key, volume_uuid, TESSERA_SEAL_KIND_BLOB,
	    key_group, id, TESSERA_HASH_SIZE, in, in_len, NULL, pt));
}

/* ── sealed packs ──────────────────────────────────────────────────── */

#define PRE  TESSERA_SEALED_PACK_PRELUDE

/* The plain front of a finished pack: [0, F) holds header, index and bloom,
 * and the data area starts far enough behind to hold the sealed form. */
static int
pack_front_len(const uint8_t *pack, size_t len, size_t *out_f)
{
	tessera_pack_geom_t g;
	int err = tessera_pack_geom_from_header(pack, len, &g);
	if (err != TESSERA_OK)
		return (err);
	size_t f = (size_t)(g.bloom_off + g.bloom_bytes);
	if (f < TESSERA_SECTOR_SIZE || f + TESSERA_SEALED_PACK_SLACK > g.data_off ||
	    f > (uint32_t)-1 - TESSERA_SEAL_OVERHEAD)
		return (TESSERA_ECORRUPT);
	*out_f = f;
	return (TESSERA_OK);
}

int
tessera_pack_seal_front(const tessera_aead_key_t *key,
    const uint8_t volume_uuid[16], uint64_t key_group, uint8_t *pack,
    size_t len, uint8_t tag_out[16])
{
	size_t f;
	uint32_t ver = 1, fl;
	int err;

	if (key == NULL || volume_uuid == NULL || pack == NULL)
		return (TESSERA_EINVAL);
	if ((err = pack_front_len(pack, len, &f)) != TESSERA_OK)
		return (err == TESSERA_ECORRUPT ? TESSERA_EINVAL : err);
	fl = (uint32_t)(f + TESSERA_SEAL_OVERHEAD);
	memmove(pack + PRE + TESSERA_NONCE_SIZE, pack, f);
	/* The whole prelude is the AAD tail, so every byte of it is bound. */
	memset(pack, 0, PRE);
	memcpy(pack, TESSERA_MAGIC_PACK_SEALED, 8);
	memcpy(pack + 8, &ver, 4);
	memcpy(pack + 12, &fl, 4);
	memcpy(pack + 16, &key_group, 8);
	err = tessera_unit_seal(key, volume_uuid, TESSERA_SEAL_KIND_PACK_FRONT,
	    key_group, pack, PRE, pack + PRE + TESSERA_NONCE_SIZE, f,
	    pack + PRE, tag_out);
	if (err != TESSERA_OK)
		tessera_wipe(pack, PRE + fl);
	return (err);
}

int
tessera_pack_open_front(const tessera_aead_key_t *key,
    const uint8_t volume_uuid[16], uint64_t key_group,
    const uint8_t expected_tag[16], uint8_t *pack, size_t len)
{
	uint64_t g;
	uint32_t fl;
	size_t f, f2;
	int err;

	if (key == NULL || volume_uuid == NULL || pack == NULL)
		return (TESSERA_EINVAL);
	if (tessera_pack_sealed_prelude(pack, len, &g, &fl) != TESSERA_OK ||
	    (size_t)PRE + fl > len)
		return (TESSERA_EAUTH);
	if (g != key_group)
		return (TESSERA_EAUTH);
	f = fl - TESSERA_SEAL_OVERHEAD;
	uint8_t pre[PRE];
	memcpy(pre, pack, PRE);
	err = tessera_unit_open(key, volume_uuid, TESSERA_SEAL_KIND_PACK_FRONT,
	    key_group, pre, PRE, pack + PRE, fl, expected_tag,
	    pack + PRE + TESSERA_NONCE_SIZE);
	if (err != TESSERA_OK) {
		tessera_wipe(pack, PRE + fl);
		return (err == TESSERA_EINVAL ? TESSERA_EAUTH : err);
	}
	memmove(pack, pack + PRE + TESSERA_NONCE_SIZE, f);
	memset(pack + f, 0, PRE + fl - f);
	/* Authentic, so written by us: still insist the plain front is the
	 * canonical one, so the plain view is exactly the pack that was
	 * sealed. `len` may cover only the front (the kernel reads just those
	 * sectors), so the header's size is bounded here by nothing but
	 * itself; the caller checks it against the pack it has on disk. */
	if (pack_front_len(pack, (size_t)-1, &f2) != TESSERA_OK || f2 != f) {
		tessera_wipe(pack, f);
		return (TESSERA_ECORRUPT);
	}
	return (TESSERA_OK);
}

int
tessera_pack_add_sealed_blob(struct tessera_pack_builder *b,
    const tessera_aead_key_t *key, const uint8_t volume_uuid[16],
    uint64_t key_group, const tessera_hash_t id, const uint8_t *bytes,
    uint32_t len, uint32_t flags)
{
	uint8_t *s;
	int err;

	if (b == NULL || key == NULL || id == NULL ||
	    len > (uint32_t)-1 - TESSERA_SEAL_OVERHEAD)
		return (TESSERA_EINVAL);
	s = tessera_malloc((size_t)len + TESSERA_SEAL_OVERHEAD);
	if (s == NULL)
		return (TESSERA_ENOMEM);
	err = tessera_blob_seal(key, volume_uuid, key_group, id, bytes, len, s);
	if (err == TESSERA_OK)
		err = tessera_pack_add_blob(b, id, s, len + TESSERA_SEAL_OVERHEAD,
		    flags);
	tessera_free(s);
	return (err);
}
