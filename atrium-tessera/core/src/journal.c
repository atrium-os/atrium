/* tessera-core: journal codec + replay. Phase 1 implements. */

#include "tessera/journal.h"
#include "tessera/error.h"

struct tessera_journal { int dummy; };

tessera_journal_t *tessera_journal_open(const tessera_block_io_t *io, uint64_t s, uint64_t l)
{ (void)io; (void)s; (void)l; return NULL; }
int tessera_journal_format(const tessera_block_io_t *io, uint64_t s, uint64_t l)
{ (void)io; (void)s; (void)l; return TESSERA_ENOTIMPL; }
int tessera_journal_replay(tessera_journal_t *j, tessera_replay_cb_t cb, void *ctx)
{ (void)j; (void)cb; (void)ctx; return TESSERA_ENOTIMPL; }
int tessera_journal_tx_begin(tessera_journal_t *j, uint64_t *out, const char tag[16])
{ (void)j; (void)out; (void)tag; return TESSERA_ENOTIMPL; }
int tessera_journal_append(tessera_journal_t *j, uint64_t tx,
    tessera_record_type_t type, const void *body, uint32_t len)
{ (void)j; (void)tx; (void)type; (void)body; (void)len; return TESSERA_ENOTIMPL; }
int tessera_journal_tx_commit(tessera_journal_t *j, uint64_t tx)
{ (void)j; (void)tx; return TESSERA_ENOTIMPL; }
int tessera_journal_tx_abort(tessera_journal_t *j, uint64_t tx, uint32_t r)
{ (void)j; (void)tx; (void)r; return TESSERA_ENOTIMPL; }
void tessera_journal_close(tessera_journal_t *j) { (void)j; }
