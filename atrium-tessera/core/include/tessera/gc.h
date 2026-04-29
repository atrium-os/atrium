/*
 * tessera/gc.h — garbage collection (mark-sweep over reachability).
 *
 * Per tessera-fs §11. The mark phase walks live inodes and the
 * GC-root list, transitively expanding reachable blob hashes. The
 * sweep phase examines packs: fully-dead packs are retired;
 * partially-live packs are repacked.
 */

#ifndef TESSERA_GC_H_
#define TESSERA_GC_H_

#include "tessera/format.h"
#include "tessera/btree.h"

#ifdef __cplusplus
extern "C" {
#endif

typedef enum {
	TESSERA_GC_MODE_GC          = 0, /* mark-sweep only; retire dead packs */
	TESSERA_GC_MODE_FULL        = 1, /* + repack partial packs below threshold */
	TESSERA_GC_MODE_AGGRESSIVE  = 2, /* + repack every partial pack */
} tessera_gc_mode_t;

typedef struct {
	tessera_gc_mode_t  mode;
	uint32_t           live_ratio_threshold;  /* 0..100; for FULL mode */
	uint64_t           grace_seconds;         /* don't touch packs newer than this */
} tessera_gc_options_t;

extern const tessera_gc_options_t tessera_gc_default_options;

typedef struct {
	uint64_t  blobs_marked_live;
	uint64_t  packs_retired;
	uint64_t  packs_repacked;
	uint64_t  bytes_reclaimed;
} tessera_gc_stats_t;

/*
 * Run a GC pass against the volume identified by its mount-time
 * roots (inode_root, pack_registry_root). Pluggable block I/O.
 *
 * Idempotent and interruptible: a partial run can be re-started
 * and will pick up where it left off (or restart from scratch
 * with no harm).
 */
int tessera_gc_run(const tessera_block_io_t *io,
                   uint64_t inode_root_sector,
                   uint64_t pack_registry_root_sector,
                   const tessera_gc_options_t *opts,
                   tessera_gc_stats_t *out_stats);

#ifdef __cplusplus
}
#endif

#endif /* TESSERA_GC_H_ */
