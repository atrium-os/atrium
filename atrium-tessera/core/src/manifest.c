/* tessera-core: manifest builder/parser. Phase 1 implements. */

#include "tessera/manifest.h"
#include "tessera/error.h"

struct tessera_manifest_builder { int dummy; };
struct tessera_manifest_parser  { int dummy; };

tessera_manifest_builder_t *tessera_manifest_begin(tessera_manifest_kind_t k)
{ (void)k; return NULL; }
int tessera_manifest_add_chunk(tessera_manifest_builder_t *b,
    const tessera_hash_t h, uint64_t off, uint32_t sz, uint32_t fl)
{ (void)b; (void)h; (void)off; (void)sz; (void)fl; return TESSERA_ENOTIMPL; }
int tessera_manifest_add_tree_child(tessera_manifest_builder_t *b,
    const tessera_hash_t h, uint64_t off)
{ (void)b; (void)h; (void)off; return TESSERA_ENOTIMPL; }
int tessera_manifest_set_inline(tessera_manifest_builder_t *b,
    const uint8_t *d, size_t l)
{ (void)b; (void)d; (void)l; return TESSERA_ENOTIMPL; }
int tessera_manifest_set_symlink(tessera_manifest_builder_t *b, const char *t)
{ (void)b; (void)t; return TESSERA_ENOTIMPL; }
int tessera_manifest_add_dirent(tessera_manifest_builder_t *b, uint64_t i,
    const char *n, size_t l)
{ (void)b; (void)i; (void)n; (void)l; return TESSERA_ENOTIMPL; }
int tessera_manifest_finalize(tessera_manifest_builder_t *b,
    uint8_t *o, size_t bl, size_t *os, tessera_hash_t oh)
{ (void)b; (void)o; (void)bl; (void)os; (void)oh; return TESSERA_ENOTIMPL; }
void tessera_manifest_free(tessera_manifest_builder_t *b) { (void)b; }

tessera_manifest_parser_t *tessera_manifest_parse(const uint8_t *d, size_t l)
{ (void)d; (void)l; return NULL; }
tessera_manifest_kind_t tessera_manifest_parser_kind(const tessera_manifest_parser_t *p)
{ (void)p; return TESSERA_MFT_INLINE; }
uint64_t tessera_manifest_parser_size(const tessera_manifest_parser_t *p)
{ (void)p; return 0; }
uint32_t tessera_manifest_parser_count(const tessera_manifest_parser_t *p)
{ (void)p; return 0; }
int tessera_manifest_chunk_at(const tessera_manifest_parser_t *p, uint32_t i,
    tessera_chunk_record_t *o)
{ (void)p; (void)i; (void)o; return TESSERA_ENOTIMPL; }
int tessera_manifest_tree_at(const tessera_manifest_parser_t *p, uint32_t i,
    tessera_tree_record_t *o)
{ (void)p; (void)i; (void)o; return TESSERA_ENOTIMPL; }
int tessera_manifest_inline_data(const tessera_manifest_parser_t *p,
    const uint8_t **od, size_t *ol)
{ (void)p; (void)od; (void)ol; return TESSERA_ENOTIMPL; }
void tessera_manifest_parser_free(tessera_manifest_parser_t *p) { (void)p; }
