/*
 * tessera/volume.h — volume-level operations (mkfs, mount).
 *
 * Format-time and open-time entry points. Sits above the primitives
 * (B+tree, extent allocator, journal) and produces / consumes a real
 * Tessera volume per the on-disk layout in tessera-fs.md §3.
 *
 *   mkfs flow:
 *     1. format the journal in sectors [4, 4+journal_sectors).
 *     2. create empty inode-table B+tree (tree_kind = 0).
 *     3. create empty pack-registry B+tree (tree_kind = 1).
 *     4. seed the free-extent map with the post-metadata free zone
 *        and flush it as a B+tree (tree_kind = 2).
 *     5. write Superblock A (sector 0) and Superblock B (sector 1)
 *        with generation = 1 and all four roots populated.
 *
 *   open flow:
 *     1. read both superblocks, pick the one with valid CRC + magic
 *        and the highest generation; ECORRUPT if neither is valid.
 *     2. verify version + sector_size = 4096; load roots into the
 *        opaque handle. (Journal replay is a Phase-3 wiring task and
 *        runs separately.)
 */

#ifndef TESSERA_VOLUME_H_
#define TESSERA_VOLUME_H_

#include "tessera/btree.h"

#ifdef __cplusplus
extern "C" {
#endif

typedef struct tessera_volume tessera_volume_t;

/* Format-time options. The caller fills in total_sectors + a
 * volume_uuid; everything else has sensible defaults. */
typedef struct {
	uint64_t total_sectors;     /* required, > journal + metadata zone */
	uint64_t journal_sectors;   /* default 256 (= 1 MiB) when 0 */
	uint8_t  volume_uuid[16];   /* caller-supplied; not generated here */

	/* Optional: pre-populate the root directory with a single dirent
	 * pointing at a fresh regular-file inode. Used by tests and the
	 * Phase-4 kmod bring-up to produce a non-empty volume without
	 * implementing the runtime mutation paths. NULL or name_len==0
	 * disables seeding; the volume mounts as an empty directory. */
	const char *seed_dirent_name;
	uint16_t    seed_dirent_name_len;
	uint64_t    seed_dirent_inode;     /* must be >= TESSERA_INODE_FIRST_USER */

	/* Optional: pre-populate the seeded file's content. If
	 * seed_content_len is 0 (default) the file is empty
	 * (manifest_hash all zero). If non-zero and seed_chunk_size is
	 * 0, an INLINE manifest is published. If seed_chunk_size > 0,
	 * the content is split into chunks of that size (last chunk
	 * may be shorter) and a CHUNK_LIST manifest is published with
	 * each chunk stored as a separate blob in the same initial
	 * pack. */
	const uint8_t *seed_content_data;
	size_t         seed_content_len;
	uint32_t       seed_chunk_size;     /* 0 = INLINE, >0 = CHUNK_LIST */

	/* Content-hash algorithm for the volume (TESSERA_HASH_ALG_*).
	 * 0 = SHA-256 (default). Non-zero sets TESSERA_INCOMPAT_HASH_ALG
	 * so older implementations refuse to mount. Rust FFI mirror:
	 * rs/tessera-sys/src/lib.rs — keep in lockstep. */
	uint32_t       hash_alg;
} tessera_format_opts_t;

/* Reserved metadata-zone size, in sectors, immediately after the
 * journal. The reserve serves both format-time and runtime: format
 * consumes the empty inode/pack-registry/free-extent root nodes
 * (~5 sectors); runtime mutation paths allocate inode/pack/free
 * tree updates from the rest via the SB-tracked bump pointer
 * (tessera-fs.md §3.3). Sized large enough to cover many commits
 * before a `tessera repack` is needed. */
#define TESSERA_METADATA_ZONE_SECTORS 1024   /* 4 MiB headroom; v1
                                                accepts the leak,
                                                round 7+ adds GC */

/* mkfs. The block_io's alloc/free callbacks may be NULL — format()
 * never invokes them; it bumps sectors itself out of the metadata
 * zone. read_block / write_block MUST be valid. */
int tessera_volume_format(const tessera_block_io_t *io,
                          const tessera_format_opts_t *opts);

/* Open an already-formatted volume. The block_io must have working
 * alloc/free callbacks for any mutating use of the returned handle
 * (the Phase-2 read-only path doesn't need them). */
int tessera_volume_open(const tessera_block_io_t *io,
                        tessera_volume_t **out);

void tessera_volume_close(tessera_volume_t *);

/*
 * Sealed volumes (docs/spec/tessera-encryption.md §5). `seal` holds the keys
 * derived from the volume key (tessera_seal_keys_init, with the same uuid as
 * opts->volume_uuid) and must outlive the handle. Format seals every tree
 * node, the journal (first epoch at a random anchor), the seed pack's front
 * and blobs (keyed content ids) and HMACs the superblock; registry values
 * are TESSERA_REGISTRY_ENTRY_SIZE_SEALED bytes (entry + front tag). Open
 * authenticates a superblock copy before believing any field (see volume.c).
 * tessera_volume_open still refuses a sealed volume (EINCOMPAT), as the
 * loader and the capsa reader must. Offline commit_roots on a sealed volume
 * is not built yet (ENOTIMPL).
 */
struct tessera_seal_keys;
int tessera_volume_format_sealed(const tessera_block_io_t *io,
                                 const tessera_format_opts_t *opts,
                                 const struct tessera_seal_keys *seal);
/* The same, with a recovery slot (tessera-encryption.md §7.4: type 3,
 * Argon2id, the volume key wrapped by arcad) in key_slots[0]. */
int tessera_volume_format_sealed_recovery(const tessera_block_io_t *io,
                                          const tessera_format_opts_t *opts,
                                          const struct tessera_seal_keys *seal,
                                          const uint8_t recovery_slot[256]);
int tessera_volume_open_sealed(const tessera_block_io_t *io,
                               const struct tessera_seal_keys *seal,
                               tessera_volume_t **out);
int tessera_volume_is_sealed(const tessera_volume_t *);
/* The root tag of tree `tree_kind` (TESSERA_BTREE_KIND_*), for
 * tessera_btree_open_sealed; the journal's current anchor. */
int tessera_volume_root_tag(const tessera_volume_t *, uint8_t tree_kind,
                            uint8_t out[16]);
int tessera_volume_journal_anchor(const tessera_volume_t *, uint8_t out[16]);

/* For tools that cannot see tessera_seal_keys_t (tessera-fsck --key-file):
 * keys derived from VK for the volume on `io`, from the clear superblock
 * fields (suite, uuid; key_id set from master_key_id). NULL with *err
 * TESSERA_EINVAL when neither copy says sealed. Free with _free. */
struct tessera_seal_keys *tessera_volume_seal_keys_for(
        const tessera_block_io_t *io, const uint8_t vk[32], int *err);
void tessera_volume_seal_keys_free(struct tessera_seal_keys *);
/* A sealed pack's front opened in place (tessera_pack_open_front with
 * K_meta, key group 0); a blob opened under K_blob; a keyed content id. */
int tessera_volume_open_pack_front(const struct tessera_seal_keys *,
        const uint8_t tag[16], uint8_t *pack, size_t len);
int tessera_volume_open_blob(const struct tessera_seal_keys *,
        const tessera_hash_t id, const uint8_t *in, size_t len, uint8_t *pt);
int tessera_volume_keyed_id(const struct tessera_seal_keys *,
        uint32_t hash_alg, const uint8_t *data, size_t len,
        tessera_hash_t out);


/* Offline-repair commit (tessera-fsck --repair). Overrides the roots a
 * consistency repair can legitimately move, bumps the superblock
 * generation, and writes both SB copies. All other SB fields are
 * preserved. `meta_reserve_bump` / `next_inode_no` are only advanced,
 * never regressed. The block_io's write_block MUST be valid and every
 * structure the new roots reference MUST already be durable on disk
 * (open the device O_SYNC). Not for the runtime mutation path — the
 * kmod commits through the journal. */
typedef struct {
	uint64_t inode_root;
	uint64_t pack_registry_root;
	uint64_t free_extent_root;
	uint64_t quota_tree_root;
	uint64_t snapshots_root;
	uint64_t meta_reserve_bump;   /* advanced past reserve sectors used */
	uint64_t next_inode_no;       /* advanced if repair minted an inode */
	uint64_t blob_index_root;     /* blob→pack index — WRITTEN VERBATIM.
	                               * (An earlier comment here claimed "0 keeps
	                               * current". It does not: the code below
	                               * compares and assigns, so a caller that
	                               * leaves this 0 ZEROES the index. Read the
	                               * live value and pass it through.) */
	uint64_t dead_extent_root;    /* deferred-dedup dead extents. IGNORED
	                               * unless TESSERA_COMMIT_DEAD_EXTENT is set,
	                               * so every existing caller keeps preserving
	                               * it by doing nothing. */
} tessera_commit_roots_t;

int tessera_volume_commit_roots(tessera_volume_t *v,
                                const tessera_commit_roots_t *roots);

/* commit_roots flags. Default (0) matches tessera_volume_commit_roots:
 * meta_reserve_bump only advances, never regresses (repair semantics). */
#define TESSERA_COMMIT_DEAD_EXTENT 0x2u  /* apply roots->dead_extent_root.
                                           * Opt-in because the field is new:
                                           * without it a caller that zero-fills
                                           * the struct would silently destroy
                                           * the dead-extent tree. Only
                                           * tessera-fsck --repair sets it, to
                                           * clear a root proven STALE. */
#define TESSERA_COMMIT_FEATURE_UNLINKED 0x4u  /* set TESSERA_FEATURE_UNLINKED_FLAG:
                                           * the caller has freed every
                                           * unflagged nlink=0 orphan, so the
                                           * kmod's one-time legacy sweep is
                                           * no longer needed. */
#define TESSERA_COMMIT_BUMP_EXACT  0x1u   /* set bump to roots->meta_reserve_bump
                                           * exactly, permitting it to LOWER —
                                           * for tessera-repack, which compacts
                                           * the reserve and reclaims headroom. */

/* As tessera_volume_commit_roots, but `flags` selects bump semantics. With
 * TESSERA_COMMIT_BUMP_EXACT the caller MUST have written the new (compacted)
 * trees to sectors the CURRENT committed SB does not reference, so a crash
 * before/during this commit leaves the old SB pointing at intact trees. */
int tessera_volume_commit_roots_ex(tessera_volume_t *v,
                                   const tessera_commit_roots_t *roots,
                                   uint32_t flags);

/* Read-only accessors over the active superblock's fields. */
uint64_t        tessera_volume_total_sectors    (const tessera_volume_t *);
uint64_t        tessera_volume_generation       (const tessera_volume_t *);
uint64_t        tessera_volume_inode_root       (const tessera_volume_t *);
uint64_t        tessera_volume_pack_registry_root(const tessera_volume_t *);
uint64_t        tessera_volume_free_extent_root (const tessera_volume_t *);
uint64_t        tessera_volume_journal_start    (const tessera_volume_t *);
uint64_t        tessera_volume_journal_length   (const tessera_volume_t *);
const uint8_t  *tessera_volume_uuid             (const tessera_volume_t *);
/* v2 snapshots / metadata-reserve / encryption fields (added 2026-04-30). */
uint32_t        tessera_volume_hash_alg         (const tessera_volume_t *);
uint64_t        tessera_volume_snapshots_root   (const tessera_volume_t *);
uint64_t        tessera_volume_snapshots_gen    (const tessera_volume_t *);
uint64_t        tessera_volume_meta_reserve_start (const tessera_volume_t *);
uint64_t        tessera_volume_meta_reserve_length(const tessera_volume_t *);
uint64_t        tessera_volume_meta_reserve_bump  (const tessera_volume_t *);
uint64_t        tessera_volume_pack_zone_start   (const tessera_volume_t *);
uint64_t        tessera_volume_pack_zone_length  (const tessera_volume_t *);
uint16_t        tessera_volume_encryption_flags   (const tessera_volume_t *);
uint8_t         tessera_volume_active_slot_count  (const tessera_volume_t *);
uint64_t        tessera_volume_quota_tree_root    (const tessera_volume_t *);
uint64_t        tessera_volume_next_inode_no       (const tessera_volume_t *);
uint64_t        tessera_volume_blob_index_root      (const tessera_volume_t *);
uint32_t        tessera_volume_feature_flags        (const tessera_volume_t *);
uint64_t        tessera_volume_dead_extent_root     (const tessera_volume_t *);

#ifdef __cplusplus
}
#endif

#endif /* TESSERA_VOLUME_H_ */
