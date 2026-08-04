/*
 * tessera-core: per-directory-tree quota domain logic.
 *
 * Pure reserve/release arithmetic over a quota domain record. Spec:
 * docs/spec/tessera-quotas.md §5. The kmod calls these under the
 * per-domain spinlock; the critical section is two loads + a compare +
 * an add (no I/O), so throughput is unaffected.
 */

#include "tessera/quota.h"
#include "tessera/error.h"
#include "tessera_compat.h"   /* memset */

void
tessera_quota_domain_init(tessera_quota_domain_t *d,
                          uint64_t domain_id,
                          uint64_t root_inode_no,
                          uint64_t limit_bytes)
{
	if (d == NULL) return;
	memset(d, 0, sizeof(*d));
	d->domain_id = domain_id;
	d->root_inode_no = root_inode_no;
	d->limit_bytes = limit_bytes;
	/* GLOBAL, not DEFERRED.
	 *
	 * This used to default to DEFERRED on the reasoning that it is "the
	 * safe default" (§20.2 calls it that for OVERLAYS). But the publish
	 * path ignored the field entirely, so nothing ever acted on it — and
	 * the moment it stopped ignoring it, every domain including domain 1
	 * (the whole-FS default, covering the root and the trusted-ingest app
	 * trees) would have flipped to append-anyway. That regresses exactly
	 * the case the disk-cost thesis is won on, and §20.2's own table
	 * assigns `global` to those trees.
	 *
	 * So: default to the behaviour the filesystem already had, and make
	 * `deferred` an explicit per-domain choice (TESSERA_IOC_DEDUP_POLICY),
	 * which is what Portcullis sets on an overlay. */
	d->dedup_policy = TESSERA_DEDUP_GLOBAL;
}

int
tessera_quota_reserve(tessera_quota_domain_t *d, uint64_t delta)
{
	if (d == NULL) return TESSERA_EINVAL;
	if (delta == 0) return TESSERA_OK;

	/* limit_bytes == 0 → unlimited: account but never reject. */
	if (d->limit_bytes != 0) {
		/* Overflow-safe form of `used + delta > limit`: never adds
		 * first. The >= guard also covers a domain already at/over
		 * limit (defensive — shouldn't happen via this path). */
		if (d->used_bytes >= d->limit_bytes)
			return TESSERA_EDQUOT;
		if (delta > d->limit_bytes - d->used_bytes)
			return TESSERA_EDQUOT;
	}
	d->used_bytes += delta;
	return TESSERA_OK;
}

void
tessera_quota_release(tessera_quota_domain_t *d, uint64_t delta)
{
	if (d == NULL) return;
	if (delta >= d->used_bytes)
		d->used_bytes = 0;
	else
		d->used_bytes -= delta;
}
