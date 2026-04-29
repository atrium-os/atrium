/* tessera-core: free-extent allocator. Phase 1 implements. */

#include "tessera/extent.h"
#include "tessera/error.h"

struct tessera_extent_alloc { int dummy; };

tessera_extent_alloc_t *tessera_extent_open(const tessera_block_io_t *io, uint64_t r)
{ (void)io; (void)r; return NULL; }
int tessera_extent_alloc(tessera_extent_alloc_t *a, uint64_t n, uint64_t *o)
{ (void)a; (void)n; (void)o; return TESSERA_ENOTIMPL; }
int tessera_extent_free(tessera_extent_alloc_t *a, uint64_t s, uint64_t n)
{ (void)a; (void)s; (void)n; return TESSERA_ENOTIMPL; }
uint64_t tessera_extent_free_blocks(const tessera_extent_alloc_t *a)
{ (void)a; return 0; }
uint64_t tessera_extent_largest_free_run(const tessera_extent_alloc_t *a)
{ (void)a; return 0; }
void tessera_extent_close(tessera_extent_alloc_t *a) { (void)a; }
