/*
 * tessera/btree.h — B+tree primitives over caller-provided block I/O.
 *
 * The tree's nodes live on disk; the caller provides a tessera_block_io_t
 * vtable to read/write blocks. Used for the inode table, pack registry,
 * and free-extent map.
 *
 * COW semantics: mutations write a new node and propagate up to the
 * root. Old blocks are freed via the caller's allocator.
 */

#ifndef TESSERA_BTREE_H_
#define TESSERA_BTREE_H_

#include "tessera/format.h"

#ifdef __cplusplus
extern "C" {
#endif

/* Pluggable block I/O for the B+tree (and other structured stores). */
typedef struct {
	/* Read one block (4 KiB) into out_buf. */
	int (*read_block) (void *ctx, uint64_t sector, uint8_t *out_buf);
	/* Write one block. Caller owns buf for the call's duration. */
	int (*write_block)(void *ctx, uint64_t sector, const uint8_t *buf);
	/* Allocate `n` contiguous sectors; return start sector via out_start. */
	int (*alloc)      (void *ctx, uint64_t n, uint64_t *out_start);
	/* Free a previously-allocated extent. */
	int (*free)       (void *ctx, uint64_t start, uint64_t n);
	/* Opaque context passed to every callback. */
	void *ctx;
} tessera_block_io_t;

typedef struct tessera_btree tessera_btree_t;

/* Open an existing B+tree. `tree_kind` matches tessera_btree_node_header. */
tessera_btree_t *tessera_btree_open(const tessera_block_io_t *io,
                                    uint64_t root_sector,
                                    uint8_t tree_kind,
                                    uint32_t key_size,
                                    uint32_t value_size);

/* Create an empty B+tree. Allocates the root via io->alloc. */
tessera_btree_t *tessera_btree_create(const tessera_block_io_t *io,
                                      uint8_t tree_kind,
                                      uint32_t key_size,
                                      uint32_t value_size,
                                      uint64_t *out_root_sector);

/* O(log n). Returns TESSERA_ENOENT if absent. */
int tessera_btree_get(tessera_btree_t *,
                      const void *key, void *out_value);

/* COW-style insert/update. Writes new path to root; allocates new
 * blocks; frees old. Returns the new root sector via *out_root. */
/* Apply n (key,value) pairs sorted STRICTLY ascending by key in one
 * copy-on-write pass — semantics identical to n sequential puts
 * (insert-or-replace), but every affected node is rewritten once.
 * Returns TESSERA_EINVAL on a mis-sorted batch. */
int tessera_btree_put_sorted_batch(tessera_btree_t *, const void *keys,
                                   const void *values, uint32_t n,
                                   uint64_t *out_new_root);

/* Called once per batch key that REPLACED an existing entry, with that
 * entry's key, its old value, and the new value replacing it — a caller
 * reclaiming resources owned by the old value needs the new one to tell
 * a genuine displacement from a rewrite that keeps the same resources.
 *
 * ★ RECORD ONLY. This fires from INSIDE the merge, mid-COW, with whatever
 * lock the caller holds over the whole batch. It must not block, perform
 * I/O, allocate, or re-enter any tree — copy what you need into caller
 * storage and do the real work after the batch call returns. Doing the
 * reclaim in the callback instead deadlocked the kmod against its own
 * flush gate. All three pointers are valid only for the duration of the
 * call. */
typedef void (*tessera_btree_displaced_cb_t)(void *ctx, const void *key,
                                             const void *old_value,
                                             const void *new_value);

/* put_sorted_batch with displaced-entry notification. The merge pass
 * already knows which keys collide; a caller that must reclaim resources
 * owned by the old value gets them here instead of paying a separate
 * O(n log n) lookup pass. cb == NULL is exactly put_sorted_batch. */
int tessera_btree_put_sorted_batch_ex(tessera_btree_t *, const void *keys,
                                      const void *values, uint32_t n,
                                      uint64_t *out_new_root,
                                      tessera_btree_displaced_cb_t cb,
                                      void *ctx);

int tessera_btree_put(tessera_btree_t *,
                      const void *key, const void *value,
                      uint64_t *out_new_root);

int tessera_btree_delete(tessera_btree_t *,
                         const void *key,
                         uint64_t *out_new_root);

/* In-order iteration. Cursor state is opaque. */
typedef struct tessera_btree_cursor tessera_btree_cursor_t;
tessera_btree_cursor_t *tessera_btree_seek_first(tessera_btree_t *);
tessera_btree_cursor_t *tessera_btree_seek_at(tessera_btree_t *, const void *key);
int                      tessera_btree_cursor_get(tessera_btree_cursor_t *,
                                                   void *out_key, void *out_value);
int                      tessera_btree_cursor_next(tessera_btree_cursor_t *);
void                     tessera_btree_cursor_free(tessera_btree_cursor_t *);

void tessera_btree_close(tessera_btree_t *);

/* Suppress the tree_kind-mismatch debug print in load_node for this handle
 * (still returns ECORRUPT). For speculative walks of possibly-recycled roots
 * — e.g. the pin-bitmap scan's retained-snapshot roots — where a mismatch is
 * an expected, benign miss rather than corruption. */
void tessera_btree_set_quiet_kind_mismatch(tessera_btree_t *t, int quiet);

/*
 * ★ #102: why the most recent node load failed.
 *
 * load_node collapsed three conditions into ECORRUPT, so a caller could not
 * distinguish a transient read failure from positive evidence that a sector
 * had been recycled. KIND is that evidence: the sector holds a VALID btree
 * node belonging to a different tree, which can only happen if it was freed
 * and reused. A retained snapshot root reporting KIND is genuinely gone; one
 * reporting IO may be perfectly fine and merely unreadable this instant.
 *
 * That difference decides whether reclaiming what the snapshot referenced is
 * safe. Acting without it retired healthy snapshots and produced a dangling
 * blob, so treat IO/HEADER as "unknown, do nothing".
 */
typedef enum {
	TESSERA_BTREE_FAIL_NONE   = 0,
	TESSERA_BTREE_FAIL_IO     = 1,	/* device read failed */
	TESSERA_BTREE_FAIL_HEADER = 2,	/* not a btree node (magic/CRC) */
	TESSERA_BTREE_FAIL_KIND   = 3,	/* valid node of ANOTHER tree */
} tessera_btree_fail_t;

/* out_sector / out_found_kind may be NULL. found_kind is meaningful only for
 * TESSERA_BTREE_FAIL_KIND. */
tessera_btree_fail_t tessera_btree_last_fail(const tessera_btree_t *t,
                                            uint64_t *out_sector,
                                            uint8_t *out_found_kind);

/* Visitor: called once per on-disk node sector (root + every internal
 * + every leaf), pre-order. Return non-zero to abort the walk. */
typedef int (*tessera_btree_node_visitor_t)(void *ctx, uint64_t sector);

/* Pre-visitor return value meaning "I have already seen this sector; do
 * NOT descend into it, but keep walking the rest of the tree". Distinct
 * from 0 (descend) and from every tessera_errno_t (abort). Only
 * tessera_btree_walk_nodes_ex() honours it.
 *
 * This is what makes a multi-tree walk cost O(distinct nodes) rather
 * than O(sum of nodes per tree). Under COW a node's sector changes
 * whenever anything beneath it changes, so two trees that reach the
 * SAME sector reach byte-identical subtrees — re-descending is pure
 * waste. Snapshots of one volume share almost all of their nodes. */
#define TESSERA_BTREE_WALK_PRUNE  0x50524E45  /* 'PRNE' */

/* Walk every node sector in the tree. Used at mount time to
 * reconstruct which sectors of the metadata-reserve are still
 * referenced (and therefore which are free to recycle). */
int tessera_btree_walk_nodes(tessera_btree_t *,
                             tessera_btree_node_visitor_t cb, void *ctx);

/* As above, with two hooks:
 *   pre  — called before descending; may return TESSERA_BTREE_WALK_PRUNE.
 *   post — called after a node's ENTIRE subtree has been walked without
 *          error; NULL if not needed.
 *
 * `post` exists so a caller can prune safely. A pre-visitor that prunes
 * on "already marked" is WRONG if the earlier visit aborted part-way:
 * that node is marked but its subtree is not, so pruning on it silently
 * under-reports. Pruning on "post-visited" instead is sound, because a
 * node is post-visited only once every descendant has been reported. */
int tessera_btree_walk_nodes_ex(tessera_btree_t *,
                                tessera_btree_node_visitor_t pre,
                                tessera_btree_node_visitor_t post,
                                void *ctx);

#ifdef __cplusplus
}
#endif

#endif /* TESSERA_BTREE_H_ */
