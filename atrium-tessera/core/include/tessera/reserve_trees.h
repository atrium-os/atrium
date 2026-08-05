/*
 * tessera/reserve_trees.h — THE list of B-trees that live in the metadata
 * reserve. One definition; every consumer expands it.
 *
 * Adding a tree to the reserve used to mean remembering seven places. Nobody
 * ever did: the blob index (#61) was missed by pinscan, the quota tree by
 * pinscan again, and the dead-extent log (#114) by repack's live set — and
 * each miss silently recycled a live root sector into another tree. Three
 * trees, three misses. See docs/spec/tessera-reserve-trees.md.
 *
 * So the list is data now. Add a row here and the pinscan table, its bounds,
 * fsck's stale-root sweep, and repack's live-node sweeps all pick it up. What
 * remains hand-written (repack's per-tree move) can then only fail to COMPACT
 * a tree, never to corrupt it — because the generated sweeps already pin it.
 *
 * Columns:
 *   field        superblock member holding the root. Also names the accessor:
 *                tessera_volume_<field>(). Root 0 means "absent" everywhere.
 *   kind         TESSERA_BTREE_KIND_*; a node must carry this in its header
 *                or the root is stale (that is the whole invariant).
 *   ksz, vsz     key / value sizes. MUST be plain #define'd constants from
 *                format.h — tessera-sys/build.rs resolves them textually to
 *                emit the Rust table, and cannot evaluate expressions.
 *   tier         what fsck --repair may do with a stale root:
 *                  REBUILD  reconstructible from other on-disk state
 *                  CLEAR    bounded, nameable loss; volume stays consistent
 *                  REFUSE   clearing destroys the filesystem
 *   consequence  what the operator actually loses. "kind mismatch" tells them
 *                nothing; this is the sentence they act on.
 */
#ifndef TESSERA_RESERVE_TREES_H
#define TESSERA_RESERVE_TREES_H

#include "tessera/format.h"

/* clang-format off */
#define TESSERA_RESERVE_TREES(X)                                              \
    X(inode_root,         TESSERA_BTREE_KIND_INODE,      4,                   \
      TESSERA_INODE_RECORD_SIZE,    REFUSE,                                   \
      "every file and directory is unreachable")                              \
    X(pack_registry_root, TESSERA_BTREE_KIND_PACK_REG,   16,                  \
      TESSERA_REGISTRY_ENTRY_SIZE,  REFUSE,                                   \
      "no blob can be located; the volume is effectively empty")              \
    X(free_extent_root,   TESSERA_BTREE_KIND_FREE_EXT,   8,                   \
      8,                            REBUILD,                                  \
      "free-space accounting is lost; allocation may hand out live sectors")  \
    X(snapshots_root,     TESSERA_BTREE_KIND_SNAPSHOT,   8,                   \
      TESSERA_SNAPSHOT_RECORD_SIZE, REFUSE,                                   \
      "all retained snapshots are unreachable")                               \
    X(quota_tree_root,    TESSERA_BTREE_KIND_QUOTA,      8,                   \
      TESSERA_QUOTA_DOMAIN_SIZE,    CLEAR,                                    \
      "all per-directory quota domains are lost; limits stop being enforced") \
    X(blob_index_root,    TESSERA_BTREE_KIND_BLOB_INDEX,                      \
      TESSERA_BLOB_INDEX_KEY_SIZE,                                            \
      TESSERA_BLOB_INDEX_VAL_SIZE,  CLEAR,                                    \
      "cold reads fall back to scanning the whole registry; run tessera-reindex") \
    X(dead_extent_root,   TESSERA_BTREE_KIND_DEAD_EXT,                        \
      TESSERA_DEAD_EXT_KEY_SIZE,                                              \
      TESSERA_DEAD_EXT_VAL_SIZE,    CLEAR,                                    \
      "deferred-dedup dead extents are stranded and never reclaimed")
/* clang-format on */

/* Derived, so no consumer ever hard-codes a count. The pinscan bug this
 * replaces had THREE independent numbers — array dimension, entry count and
 * loop bound — with nothing tying them together, so a row past the bound
 * compiled clean and never ran. */
#define TESSERA_RESERVE_TREE_COUNT_X(a, b, c, d, e, f) + 1
#define TESSERA_RESERVE_TREE_COUNT \
    (0 TESSERA_RESERVE_TREES(TESSERA_RESERVE_TREE_COUNT_X))

#endif /* TESSERA_RESERVE_TREES_H */
