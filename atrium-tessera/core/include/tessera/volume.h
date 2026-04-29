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
} tessera_format_opts_t;

/* Reserved metadata-zone size, in sectors, immediately after the
 * journal. Holds the empty inode/pack-registry/free-extent root nodes
 * and a small overprovisioning headroom. Volume-runtime allocations
 * come from after this zone. */
#define TESSERA_METADATA_ZONE_SECTORS 16

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

/* Read-only accessors over the active superblock's fields. */
uint64_t        tessera_volume_total_sectors    (const tessera_volume_t *);
uint64_t        tessera_volume_generation       (const tessera_volume_t *);
uint64_t        tessera_volume_inode_root       (const tessera_volume_t *);
uint64_t        tessera_volume_pack_registry_root(const tessera_volume_t *);
uint64_t        tessera_volume_free_extent_root (const tessera_volume_t *);
uint64_t        tessera_volume_journal_start    (const tessera_volume_t *);
uint64_t        tessera_volume_journal_length   (const tessera_volume_t *);
const uint8_t  *tessera_volume_uuid             (const tessera_volume_t *);

#ifdef __cplusplus
}
#endif

#endif /* TESSERA_VOLUME_H_ */
