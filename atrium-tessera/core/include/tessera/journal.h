/*
 * tessera/journal.h — circular-log journal codec and replay.
 *
 * Per tessera-fs §4. The journal records mutating transactions
 * (inode writes, manifest repoints, pack publish/retire, root
 * updates). Pack file *content* is not journaled (it's written
 * durably to its sealed extent before the journal references it).
 */

#ifndef TESSERA_JOURNAL_H_
#define TESSERA_JOURNAL_H_

#include "tessera/format.h"
#include "tessera/btree.h"

#ifdef __cplusplus
extern "C" {
#endif

typedef struct tessera_journal tessera_journal_t;

/* Open an existing journal at `journal_start..journal_start+length`. */
tessera_journal_t *tessera_journal_open(const tessera_block_io_t *io,
                                         uint64_t journal_start,
                                         uint64_t journal_length);

/* Format an empty journal at the given range (mkfs-time). */
int tessera_journal_format(const tessera_block_io_t *io,
                           uint64_t journal_start,
                           uint64_t journal_length);

/* ── Replay ──────────────────────────────────────────────────────── */

/* Caller-provided record handler. Called once per committed record
 * during replay. Return 0 to continue, negative to abort. */
typedef int (*tessera_replay_cb_t)(void *ctx,
                                    const tessera_record_header_t *hdr,
                                    const uint8_t *body);

/* Replay all committed transactions; aborted (no TX_COMMIT) ones
 * are silently discarded. */
int tessera_journal_replay(tessera_journal_t *,
                           tessera_replay_cb_t cb, void *ctx);

/* ── Append + commit ─────────────────────────────────────────────── */

/* Begin a transaction. Returns a tx_id that must be referenced by all
 * subsequent records in this transaction. */
int tessera_journal_tx_begin(tessera_journal_t *, uint64_t *out_tx_id,
                              const char reason_tag[16]);

/* Append a record to the current open transaction. */
int tessera_journal_append(tessera_journal_t *, uint64_t tx_id,
                           tessera_record_type_t type,
                           const void *body, uint32_t body_len);

/* Commit a transaction. Writes TX_COMMIT, fsyncs, advances head. */
int tessera_journal_tx_commit(tessera_journal_t *, uint64_t tx_id);

/* Checkpoint: reset head/tail to the start of the log, freeing all
 * sectors. Caller must guarantee that every committed record up to
 * the current head is durably applied (i.e., reflected in the
 * superblock or its dependents). After return the journal is empty
 * and ready for the next transaction. v1 phase: callers checkpoint
 * after every successful commit_sb. */
int tessera_journal_checkpoint(tessera_journal_t *);

/* Abort a transaction. Writes TX_ABORT (advisory). */
int tessera_journal_tx_abort(tessera_journal_t *, uint64_t tx_id,
                              uint32_t reason_code);

void tessera_journal_close(tessera_journal_t *);

/* Debug accessor: read the current in-memory head_block / tail_block.
 * Either out pointer may be NULL. Used to compare against on-disk
 * state when diagnosing crash-recovery divergence. */
void tessera_journal_peek_pos(const tessera_journal_t *,
    uint64_t *head_block, uint64_t *tail_block);

/* Deferred-write mode (perf). Some callers (e.g. the kmod's dirent/inode
 * redo-log drain) journal large batches of records whose durability is
 * not required until the next superblock commit barrier — the same
 * deferred-commit boundary the rest of the filesystem uses. For those,
 * an alternate block_io whose write_block defers to the host buffer
 * cache (bdwrite) lets a batch of records flush in bulk at commit time
 * instead of paying a synchronous device round-trip per record.
 *
 * tessera_journal_set_deferred_io registers that alternate io (copied,
 * like the primary io at open). tessera_journal_deferred_begin/_end
 * bracket a region whose record + header writes route through it. The
 * default mode is synchronous (the registered primary io), preserving
 * commit_sb / replay / checkpoint durability semantics unchanged. A
 * begin with no deferred io registered is a no-op. */
void tessera_journal_set_deferred_io(tessera_journal_t *,
    const tessera_block_io_t *deferred_io);
void tessera_journal_deferred_begin(tessera_journal_t *);
void tessera_journal_deferred_end(tessera_journal_t *);

#ifdef __cplusplus
}
#endif

#endif /* TESSERA_JOURNAL_H_ */
