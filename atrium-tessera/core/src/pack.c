/* tessera-core: pack-file builder/reader. Phase 1 implements. */

#include "tessera/pack.h"
#include "tessera/error.h"

struct tessera_pack_builder { int dummy; };
struct tessera_pack_reader  { int dummy; };

tessera_pack_builder_t *tessera_pack_begin(uint32_t k, const uint8_t id[16], uint64_t tx)
{ (void)k; (void)id; (void)tx; return NULL; }
int tessera_pack_add_blob(tessera_pack_builder_t *b, const tessera_hash_t h,
    const uint8_t *bytes, uint32_t len, uint32_t fl)
{ (void)b; (void)h; (void)bytes; (void)len; (void)fl; return TESSERA_ENOTIMPL; }
int tessera_pack_finalize(tessera_pack_builder_t *b, uint8_t *o, size_t l, size_t *os)
{ (void)b; (void)o; (void)l; (void)os; return TESSERA_ENOTIMPL; }
void tessera_pack_free(tessera_pack_builder_t *b) { (void)b; }

tessera_pack_reader_t *tessera_pack_open(const uint8_t *d, size_t l)
{ (void)d; (void)l; return NULL; }
uint32_t tessera_pack_blob_count(const tessera_pack_reader_t *r)
{ (void)r; return 0; }
int tessera_pack_lookup(const tessera_pack_reader_t *r, const tessera_hash_t h,
    const uint8_t **ob, uint32_t *ol)
{ (void)r; (void)h; (void)ob; (void)ol; return TESSERA_ENOTIMPL; }
int tessera_pack_bloom_might_contain(const tessera_pack_reader_t *r, const tessera_hash_t h)
{ (void)r; (void)h; return 0; }
void tessera_pack_close(tessera_pack_reader_t *r) { (void)r; }
