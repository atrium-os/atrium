/*
 * tessera/seal.h — the sealed on-disk format (docs/spec/tessera-encryption.md
 * §5.2–§5.5) on top of tessera/crypt.h.
 *
 * A sealed volume's keys come from its volume key (VK) by the labelled
 * subkeys of §4.2. This file turns them into the on-disk units:
 *
 *   - the superblock's HMAC and its sealed counters record (§5.2);
 *   - 4 KiB metadata blocks, sealed in place as payload[4056] | nonce[24] |
 *     tag[16], with the parent's recorded tag checked before opening and,
 *     for journal records, the previous record's tag in the AAD (§5.3);
 *   - variable-length units (pack front matter, blobs): nonce | ct | tag,
 *     with a caller-supplied AAD tail (key group, content id) (§5.5).
 *
 * Every AAD starts with the volume uuid and a one-byte kind (§5.1), so no
 * unit can be moved to another volume or read as another kind. No function
 * here allocates; all of it builds in the kernel.
 */
#ifndef TESSERA_SEAL_H_
#define TESSERA_SEAL_H_

#include "tessera/crypt.h"
#include "tessera/format.h"

#ifdef __cplusplus
extern "C" {
#endif

/* AAD kind byte. Never reuse a value. */
#define TESSERA_SEAL_KIND_SB_STATS     1u
#define TESSERA_SEAL_KIND_BTREE_NODE   2u
#define TESSERA_SEAL_KIND_JOURNAL_HDR  3u
#define TESSERA_SEAL_KIND_JOURNAL_REC  4u
#define TESSERA_SEAL_KIND_PEL          5u
#define TESSERA_SEAL_KIND_PACK_FRONT   6u
#define TESSERA_SEAL_KIND_BLOB         7u
#define TESSERA_SEAL_KIND_GROUP_NAME   8u   /* journal names, §5.3 */

/* The keys of one mounted sealed volume (§4.2). */
typedef struct tessera_seal_keys {
	tessera_aead_key_t meta;        /* K_meta: blocks, front matter */
	tessera_aead_key_t blob;        /* K_blob: blobs outside key groups */
	uint8_t            id[32];      /* K_id: keyed content ids */
	uint8_t            uuid[16];
	uint8_t            check[32];   /* expected sb.key_check */
	uint8_t            key_id[16];  /* VK's id, sb.master_key_id (§4.1);
	                                 * zero after init, set by the caller */
} tessera_seal_keys_t;

/* Derive every key from VK. TESSERA_EINCOMPAT for an unknown suite. */
int  tessera_seal_keys_init(tessera_seal_keys_t *k, uint32_t suite,
         const uint8_t vk[32], const uint8_t volume_uuid[16]);
void tessera_seal_keys_clear(tessera_seal_keys_t *k);

/* Constant-time comparison: 1 when equal. */
int  tessera_ct_eq(const void *a, const void *b, size_t n);

/* ── superblock (§5.2) ─────────────────────────────────────────────── */

/* Make `sb` a sealed superblock under `k`: sets the incompat bit, the
 * encryption flags, the suite, key_check, zeroes the clear counters and
 * times after sealing them into sealed_stats, then writes hmac. Call last,
 * after every other field (root tags, anchor tag) is final; the codec's
 * CRC is computed on encode as usual. */
int tessera_sb_seal(const tessera_seal_keys_t *k, tessera_superblock_t *sb,
        const tessera_sb_stats_t *stats);

/* Check a sealed superblock before using any other field: incompat bit,
 * suite, uuid, key_check (TESSERA_ENOKEY: this is not the volume's key),
 * then the HMAC (TESSERA_EAUTH). With `stats` non-NULL also opens the
 * sealed counters. */
int tessera_sb_verify(const tessera_seal_keys_t *k,
        const tessera_superblock_t *sb, tessera_sb_stats_t *stats);

/* ── 4 KiB metadata blocks (§5.3) ──────────────────────────────────── */

/*
 * Seal block[0..4056) in place and append nonce and tag. `ctx` is the tree
 * kind for B+tree nodes, the sequence number for journal records, 0
 * otherwise. `prev_tag` (journal records: the previous record's tag, or the
 * superblock's anchor) goes into the AAD; NULL means none. The tag is also
 * returned in tag_out (NULL to skip) for the parent to record.
 */
int tessera_block_seal(const tessera_seal_keys_t *k, uint8_t kind,
        uint64_t sector, uint64_t ctx, const uint8_t prev_tag[16],
        uint8_t block[TESSERA_SECTOR_SIZE], uint8_t tag_out[16]);

/*
 * Open a sealed block in place. When `expected_tag` is non-NULL the block's
 * tag must equal it (the parent's record, §5.3) or TESSERA_EAUTH is
 * returned before any decryption: this is what refuses an older copy of
 * the block. On any failure the payload is zeroed. On success the payload
 * is block[0..4056); the trailer is left as it was.
 */
int tessera_block_open(const tessera_seal_keys_t *k, uint8_t kind,
        uint64_t sector, uint64_t ctx, const uint8_t prev_tag[16],
        const uint8_t expected_tag[16], uint8_t block[TESSERA_SECTOR_SIZE]);

/* The tag a sealed block carries (no key needed). */
const uint8_t *tessera_block_tag(const uint8_t block[TESSERA_SECTOR_SIZE]);

/* ── variable-length units (§5.5) ──────────────────────────────────── */

/* Largest AAD tail a unit takes: key group (8) + content id (32). */
#define TESSERA_SEAL_AAD_TAIL_MAX  64u

/*
 * out = nonce[24] | ct[len] | tag[16] (len + TESSERA_SEAL_OVERHEAD bytes).
 * AAD = uuid | kind | key_group le64 | aad_tail. `key` is K_meta / K_blob
 * or a group's K_gmeta / K_gblob. tag_out may be NULL.
 */
int tessera_unit_seal(const tessera_aead_key_t *key,
        const uint8_t volume_uuid[16], uint8_t kind, uint64_t key_group,
        const uint8_t *aad_tail, size_t tail_len,
        const uint8_t *pt, size_t len, uint8_t *out, uint8_t tag_out[16]);

/* Opens `in` (in_len >= TESSERA_SEAL_OVERHEAD) into pt (in_len - 40
 * bytes). expected_tag as for tessera_block_open. pt is zeroed on failure. */
int tessera_unit_open(const tessera_aead_key_t *key,
        const uint8_t volume_uuid[16], uint8_t kind, uint64_t key_group,
        const uint8_t *aad_tail, size_t tail_len,
        const uint8_t *in, size_t in_len, const uint8_t expected_tag[16],
        uint8_t *pt);

/* A blob (§5.5): AAD tail = content id, under K_blob for blobs outside
 * key groups (key_group 0) or the group's K_gblob. */
int tessera_blob_seal(const tessera_aead_key_t *key,
        const uint8_t volume_uuid[16], uint64_t key_group,
        const tessera_hash_t id, const uint8_t *pt, size_t len, uint8_t *out);
int tessera_blob_open(const tessera_aead_key_t *key,
        const uint8_t volume_uuid[16], uint64_t key_group,
        const tessera_hash_t id, const uint8_t *in, size_t in_len,
        uint8_t *pt);

/* ── sealed packs (§5.5, tessera/pack.h) ───────────────────────────── */

/* Seal the front matter of a finished pack (built with
 * tessera_pack_set_sealed, len bytes) in place, under `key` (K_meta, or a
 * group's K_gmeta) and `key_group`. tag_out (may be NULL) is what the
 * pack-registry entry records. */
int tessera_pack_seal_front(const tessera_aead_key_t *key,
        const uint8_t volume_uuid[16], uint64_t key_group, uint8_t *pack,
        size_t len, uint8_t tag_out[16]);

/* Open a sealed pack's front in place, leaving the plain pack. When
 * expected_tag (the registry's record) is non-NULL the front's tag must
 * equal it. `key_group` must match the prelude's. TESSERA_EAUTH on any
 * failure, with the front region zeroed. */
int tessera_pack_open_front(const tessera_aead_key_t *key,
        const uint8_t volume_uuid[16], uint64_t key_group,
        const uint8_t expected_tag[16], uint8_t *pack, size_t len);

/* Add a blob sealed for this pack: AAD binds the content id and the key
 * group. `key` is K_blob or the group's K_gblob. */
struct tessera_pack_builder;
int tessera_pack_add_sealed_blob(struct tessera_pack_builder *b,
        const tessera_aead_key_t *key, const uint8_t volume_uuid[16],
        uint64_t key_group, const tessera_hash_t id, const uint8_t *bytes,
        uint32_t len, uint32_t flags);

#ifdef __cplusplus
}
#endif

#endif /* TESSERA_SEAL_H_ */
