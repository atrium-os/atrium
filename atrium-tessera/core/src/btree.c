/* tessera-core: B+tree primitives. Phase 1 implements. */

#include "tessera/btree.h"
#include "tessera/error.h"

#include <stdlib.h>

struct tessera_btree { int dummy; };
struct tessera_btree_cursor { int dummy; };

tessera_btree_t *
tessera_btree_open(const tessera_block_io_t *io, uint64_t root_sector,
                   uint8_t tree_kind, uint32_t key_size, uint32_t value_size)
{
	(void)io; (void)root_sector; (void)tree_kind; (void)key_size; (void)value_size;
	return NULL;
}

tessera_btree_t *
tessera_btree_create(const tessera_block_io_t *io, uint8_t tree_kind,
                     uint32_t key_size, uint32_t value_size,
                     uint64_t *out_root_sector)
{
	(void)io; (void)tree_kind; (void)key_size; (void)value_size; (void)out_root_sector;
	return NULL;
}

int tessera_btree_get   (tessera_btree_t *t, const void *k, void *v)
{ (void)t; (void)k; (void)v; return TESSERA_ENOTIMPL; }
int tessera_btree_put   (tessera_btree_t *t, const void *k, const void *v, uint64_t *r)
{ (void)t; (void)k; (void)v; (void)r; return TESSERA_ENOTIMPL; }
int tessera_btree_delete(tessera_btree_t *t, const void *k, uint64_t *r)
{ (void)t; (void)k; (void)r; return TESSERA_ENOTIMPL; }

tessera_btree_cursor_t *tessera_btree_seek_first(tessera_btree_t *t)
{ (void)t; return NULL; }
tessera_btree_cursor_t *tessera_btree_seek_at(tessera_btree_t *t, const void *k)
{ (void)t; (void)k; return NULL; }
int tessera_btree_cursor_get(tessera_btree_cursor_t *c, void *k, void *v)
{ (void)c; (void)k; (void)v; return TESSERA_ENOTIMPL; }
int tessera_btree_cursor_next(tessera_btree_cursor_t *c)
{ (void)c; return TESSERA_ENOTIMPL; }
void tessera_btree_cursor_free(tessera_btree_cursor_t *c) { (void)c; }
void tessera_btree_close(tessera_btree_t *t) { (void)t; }
