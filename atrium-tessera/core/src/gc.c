/* tessera-core: garbage collection. Phase 1 implements. */

#include "tessera/gc.h"
#include "tessera/error.h"

const tessera_gc_options_t tessera_gc_default_options = {
	.mode = TESSERA_GC_MODE_FULL,
	.live_ratio_threshold = 50u,
	.grace_seconds = 300u,
};

int
tessera_gc_run(const tessera_block_io_t *io, uint64_t inode_root,
               uint64_t pack_reg_root, const tessera_gc_options_t *opts,
               tessera_gc_stats_t *stats)
{
	(void)io; (void)inode_root; (void)pack_reg_root;
	(void)opts; (void)stats;
	return TESSERA_ENOTIMPL;
}
