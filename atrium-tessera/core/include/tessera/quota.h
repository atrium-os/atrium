/*
 * tessera/quota.h — per-directory-tree quota domain logic (pure).
 *
 * The reserve/release arithmetic over a tessera_quota_domain_t, in
 * logical bytes (tessera-quotas.md §3.2, §5). No I/O and no btree: the
 * caller (kmod) owns persistence — it loads the domain record, calls
 * these under the per-domain lock, journals the resulting delta, and
 * writes the record back. Pure functions so they unit-test in userspace
 * and run unchanged in the kernel.
 *
 * Spec: docs/spec/tessera-quotas.md.
 */
#ifndef TESSERA_QUOTA_H_
#define TESSERA_QUOTA_H_

#include "tessera/format.h"

#ifdef __cplusplus
extern "C" {
#endif

/*
 * Initialise a fresh domain record: zeroed, then domain_id / root_inode /
 * limit set, dedup_policy = deferred (the safe default — writes are never
 * observably deduplicated, but dedup still converges at rest; §3.2/§20.2).
 */
void tessera_quota_domain_init(tessera_quota_domain_t *d,
                               uint64_t domain_id,
                               uint64_t root_inode_no,
                               uint64_t limit_bytes);

/*
 * Reserve `delta` logical bytes against the domain (write / truncate-up).
 * Overflow-safe. Returns:
 *   TESSERA_OK     — it fits; used_bytes incremented by delta.
 *   TESSERA_EDQUOT — would exceed limit_bytes; the domain is unchanged.
 *   TESSERA_EINVAL — d == NULL.
 * delta == 0 is a no-op OK. A limit_bytes of 0 means "no limit": usage is
 * still accounted (for statfs) but a reservation is never rejected.
 */
int tessera_quota_reserve(tessera_quota_domain_t *d, uint64_t delta);

/*
 * Release `delta` bytes (truncate-down / unlink of the last name). Never
 * fails; clamps used_bytes at 0 so a mis-accounted delta can't underflow.
 */
void tessera_quota_release(tessera_quota_domain_t *d, uint64_t delta);

/* ── Persistence over the quota B+tree (quota_store.c) ──────────────
 *
 * Domain records live in their own tree (TESSERA_BTREE_KIND_QUOTA,
 * key_size 8, value_size TESSERA_QUOTA_DOMAIN_SIZE) rooted at
 * SB.quota_tree_root. These wrap btree get/put/delete with the
 * domain_id key encoding + record codec.
 */
struct tessera_btree;  /* fwd; full def in tessera/btree.h */

/* Encode domain_id as the 8-byte big-endian btree key. */
void tessera_quota_key(uint64_t domain_id, uint8_t out[8]);

/* Store/update a domain record; returns the new tree root in *new_root. */
int tessera_quota_store_put(struct tessera_btree *t,
                            const tessera_quota_domain_t *d,
                            uint64_t *new_root);

/* Load a domain by id; TESSERA_ENOENT if absent. */
int tessera_quota_store_get(struct tessera_btree *t,
                            uint64_t domain_id,
                            tessera_quota_domain_t *out);

/* Remove a domain record; returns the new tree root in *new_root. */
int tessera_quota_store_delete(struct tessera_btree *t,
                               uint64_t domain_id,
                               uint64_t *new_root);

#ifdef __cplusplus
}
#endif
#endif /* TESSERA_QUOTA_H_ */
