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

/*
 * Sealed journal (docs/spec/tessera-encryption.md §5.3): every block sealed
 * under the volume's K_meta, record blocks chained by tag from `anchor` (the
 * superblock's journal_anchor_tag). Open fails if the header block does not
 * open under this anchor (another epoch's journal, or tampering): the
 * superblock is then authoritative and the caller formats a fresh journal.
 * Appends need the chain's last tag: known after format, after
 * checkpoint_anchor, after open of an empty log, or after a replay that
 * reached head; otherwise append returns TESSERA_EIO. Replay stops at the
 * first block that does not open, as at a torn write.
 */
struct tessera_seal_keys;
int tessera_journal_format_sealed(const tessera_block_io_t *io,
                                  uint64_t journal_start,
                                  uint64_t journal_length,
                                  const struct tessera_seal_keys *seal,
                                  const uint8_t anchor[16]);
tessera_journal_t *tessera_journal_open_sealed(const tessera_block_io_t *io,
                                               uint64_t journal_start,
                                               uint64_t journal_length,
                                               const struct tessera_seal_keys *seal,
                                               const uint8_t anchor[16]);
/* The sealed journal's checkpoint: start a new epoch chained to `anchor`.
 * Call only once a superblock carrying `anchor` is durable. (Plain
 * tessera_journal_checkpoint refuses a sealed journal.) */
int tessera_journal_checkpoint_anchor(tessera_journal_t *,
                                      const uint8_t anchor[16]);

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

/* The scrub (tessera-encryption.md E4.7): zero every record block of an
 * empty journal (head == tail, as after a checkpoint). Records stay on disk
 * after a checkpoint until a later append overwrites them, and a plain
 * journal's dirent records name files in the clear. TESSERA_EBUSY if the
 * journal holds records; a sealed journal is left as it is. Writes through
 * the primary (synchronous) io. */
int tessera_journal_scrub(tessera_journal_t *);

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
