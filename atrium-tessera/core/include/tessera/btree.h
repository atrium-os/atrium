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

/* Visitor: called once per on-disk node sector (root + every internal
 * + every leaf), pre-order. Return non-zero to abort the walk. */
typedef int (*tessera_btree_node_visitor_t)(void *ctx, uint64_t sector);

/* Walk every node sector in the tree. Used at mount time to
 * reconstruct which sectors of the metadata-reserve are still
 * referenced (and therefore which are free to recycle). */
int tessera_btree_walk_nodes(tessera_btree_t *,
                             tessera_btree_node_visitor_t cb, void *ctx);

#ifdef __cplusplus
}
#endif

#endif /* TESSERA_BTREE_H_ */
