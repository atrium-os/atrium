/*
 * tessera/extent.h — free-extent allocator.
 *
 * Tracks free runs of blocks within the pack-file zone. Best-fit
 * with anti-fragmentation rotation. Backed by a B+tree on disk
 * (key = start_sector, value = length_sectors).
 */

#ifndef TESSERA_EXTENT_H_
#define TESSERA_EXTENT_H_

#include "tessera/btree.h"

#ifdef __cplusplus
extern "C" {
#endif

typedef struct tessera_extent_alloc tessera_extent_alloc_t;

tessera_extent_alloc_t *tessera_extent_open(const tessera_block_io_t *io,
                                              uint64_t free_root_sector);

/* Allocate `n_sectors` contiguous. Returns 0 on success, ENOSPC on
 * failure. On success *out_start is the start sector. */
int tessera_extent_alloc(tessera_extent_alloc_t *,
                         uint64_t n_sectors,
                         uint64_t *out_start);

/* Free a previously-allocated extent. Adjacent free extents are
 * coalesced. */
int tessera_extent_free(tessera_extent_alloc_t *,
                        uint64_t start, uint64_t n_sectors);

/* Reachability stats. */
uint64_t tessera_extent_free_blocks(const tessera_extent_alloc_t *);
uint64_t tessera_extent_largest_free_run(const tessera_extent_alloc_t *);

/* COW-publish the current free-extent set as a fresh B+tree. Allocates
 * a new tree via the io passed to open(); the returned root_sector is
 * what the caller writes into the superblock's free_extent_root.
 *
 * Returns TESSERA_EINVAL if the allocator was opened without io
 * (root_sector == 0 + NULL io). The previous tree (if any) is NOT
 * freed — the caller is responsible for retiring it as part of its
 * commit protocol. */
int tessera_extent_flush(tessera_extent_alloc_t *,
                         uint64_t *out_new_root_sector);

/* Same as tessera_extent_flush, but the new tree's nodes are
 * allocated via `alt_io` instead of the allocator's own io. Required
 * for in-kernel commits — using the data-zone allocator (which is
 * the same set we're iterating) recurses unsafely. The caller passes
 * a metadata-reserve bump-allocator-backed io. */
int tessera_extent_flush_via(tessera_extent_alloc_t *,
                             const tessera_block_io_t *alt_io,
                             uint64_t *out_new_root_sector);

void tessera_extent_close(tessera_extent_alloc_t *);

#ifdef __cplusplus
}
#endif

#endif /* TESSERA_EXTENT_H_ */
