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

void tessera_extent_close(tessera_extent_alloc_t *);

#ifdef __cplusplus
}
#endif

#endif /* TESSERA_EXTENT_H_ */
