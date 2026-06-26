/*
 * tessera-core: quota-domain persistence over the B+tree.
 *
 * Thin layer that maps a tessera_quota_domain_t to/from a btree record
 * (key = domain_id as 8 big-endian bytes so key order is ascending id;
 * value = the 128-byte encoded record). The quota domains live in their
 * own tree (TESSERA_BTREE_KIND_QUOTA) rooted at SB.quota_tree_root —
 * separate from the inode tree because the value size differs.
 *
 * Both the userspace tools and the kmod use these so key encoding +
 * codec stay consistent. Spec: docs/spec/tessera-quotas.md §4.2.
 */

#include "tessera/quota.h"
#include "tessera/btree.h"
#include "tessera/codec.h"
#include "tessera/error.h"
#include "tessera_compat.h"

void
tessera_quota_key(uint64_t domain_id, uint8_t out[8])
{
	/* Big-endian: lexicographic btree order == ascending domain_id. */
	for (int i = 7; i >= 0; i--) {
		out[i] = (uint8_t)(domain_id & 0xffu);
		domain_id >>= 8;
	}
}

int
tessera_quota_store_put(tessera_btree_t *t,
                        const tessera_quota_domain_t *d,
                        uint64_t *new_root)
{
	if (t == NULL || d == NULL) return TESSERA_EINVAL;
	uint8_t key[8];
	uint8_t val[TESSERA_QUOTA_DOMAIN_SIZE];
	tessera_quota_key(d->domain_id, key);
	int rc = tessera_encode_quota_domain(d, val);
	if (rc != TESSERA_OK) return rc;
	return tessera_btree_put(t, key, val, new_root);
}

int
tessera_quota_store_get(tessera_btree_t *t,
                        uint64_t domain_id,
                        tessera_quota_domain_t *out)
{
	if (t == NULL || out == NULL) return TESSERA_EINVAL;
	uint8_t key[8];
	uint8_t val[TESSERA_QUOTA_DOMAIN_SIZE];
	tessera_quota_key(domain_id, key);
	int rc = tessera_btree_get(t, key, val);   /* ENOENT propagates */
	if (rc != TESSERA_OK) return rc;
	return tessera_decode_quota_domain(val, out);
}

int
tessera_quota_store_delete(tessera_btree_t *t,
                           uint64_t domain_id,
                           uint64_t *new_root)
{
	if (t == NULL) return TESSERA_EINVAL;
	uint8_t key[8];
	tessera_quota_key(domain_id, key);
	return tessera_btree_delete(t, key, new_root);
}
