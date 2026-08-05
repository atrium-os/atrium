/*
 * Quota domain logic + codec (tessera-quotas.md §4.2, §5).
 *
 *   - reserve/release arithmetic: fit, exact-fit, over-by-one (EDQUOT),
 *     already-full, unlimited (limit 0), release-clamp.
 *   - codec round-trip for the QuotaDomain record.
 *   - inode round-trip carries the new quota_domain field.
 */

#include "tessera/quota.h"
#include "tessera/codec.h"
#include "tessera/error.h"
#include "tessera/format.h"

#include <stdint.h>
#include <stdio.h>
#include <string.h>

static int failures = 0;

#define CHECK(cond) do {                                                 \
	if (!(cond)) {                                                   \
		fprintf(stderr, "FAIL %s:%d: %s\n",                      \
		    __FILE__, __LINE__, #cond);                          \
		failures++;                                              \
	}                                                                \
} while (0)

static void
test_init(void)
{
	tessera_quota_domain_t d;
	/* dirty it first so we know init zeroes. */
	memset(&d, 0xAB, sizeof(d));
	tessera_quota_domain_init(&d, 42, 1000, 100);
	CHECK(d.domain_id == 42);
	CHECK(d.root_inode_no == 1000);
	CHECK(d.limit_bytes == 100);
	CHECK(d.used_bytes == 0);
	/* GLOBAL, not DEFERRED — changed deliberately in 6bac9879 (#114 step 1)
	 * because domain 1 covers the root and the trusted-ingest trees, and
	 * flipping those to append-anyway regresses the case the disk-cost
	 * thesis is won on. `deferred` is now an explicit per-domain choice
	 * via TESSERA_IOC_DEDUP_POLICY. See the rationale in quota.c.
	 *
	 * This assertion sat wrong for two days: the host test suite could not
	 * be built at all (it linked the cross archive), so nothing ran it. */
	CHECK(d.dedup_policy == TESSERA_DEDUP_GLOBAL);
	CHECK(d.limit_inodes == 0 && d.used_inodes == 0);
}

static void
test_reserve_release(void)
{
	tessera_quota_domain_t d;
	tessera_quota_domain_init(&d, 1, 2, 100);

	/* Partial fit. */
	CHECK(tessera_quota_reserve(&d, 60) == TESSERA_OK);
	CHECK(d.used_bytes == 60);

	/* Over by one — rejected, domain unchanged. */
	CHECK(tessera_quota_reserve(&d, 41) == TESSERA_EDQUOT);
	CHECK(d.used_bytes == 60);

	/* Exact remaining fit. */
	CHECK(tessera_quota_reserve(&d, 40) == TESSERA_OK);
	CHECK(d.used_bytes == 100);

	/* Now full: any further byte is rejected; zero is a no-op OK. */
	CHECK(tessera_quota_reserve(&d, 1) == TESSERA_EDQUOT);
	CHECK(tessera_quota_reserve(&d, 0) == TESSERA_OK);
	CHECK(d.used_bytes == 100);

	/* Release frees room; clamp never underflows. */
	tessera_quota_release(&d, 30);
	CHECK(d.used_bytes == 70);
	CHECK(tessera_quota_reserve(&d, 30) == TESSERA_OK);
	CHECK(d.used_bytes == 100);
	tessera_quota_release(&d, 1000);   /* over-release */
	CHECK(d.used_bytes == 0);
}

static void
test_unlimited(void)
{
	/* limit_bytes == 0 → no rejection, but usage still accounted. */
	tessera_quota_domain_t d;
	tessera_quota_domain_init(&d, 7, 8, 0);
	CHECK(tessera_quota_reserve(&d, 1u << 30) == TESSERA_OK);
	CHECK(tessera_quota_reserve(&d, 1u << 30) == TESSERA_OK);
	CHECK(d.used_bytes == (2ull << 30));
}

static void
test_null(void)
{
	CHECK(tessera_quota_reserve(NULL, 1) == TESSERA_EINVAL);
	tessera_quota_release(NULL, 1);          /* must not crash */
	tessera_quota_domain_init(NULL, 1, 2, 3); /* must not crash */
}

static void
test_codec(void)
{
	tessera_quota_domain_t in, out;
	tessera_quota_domain_init(&in, 9, 1234, 1ull << 40);
	in.used_bytes = 555;
	in.dedup_policy = TESSERA_DEDUP_SALTED;
	for (int i = 0; i < 32; i++) in.domain_salt[i] = (uint8_t)(i * 3 + 1);

	uint8_t buf[TESSERA_QUOTA_DOMAIN_SIZE];
	CHECK(tessera_encode_quota_domain(&in, buf) == TESSERA_OK);
	memset(&out, 0, sizeof(out));
	CHECK(tessera_decode_quota_domain(buf, &out) == TESSERA_OK);
	CHECK(memcmp(&in, &out, sizeof(in)) == 0);

	CHECK(tessera_encode_quota_domain(NULL, buf) == TESSERA_EINVAL);
	CHECK(tessera_decode_quota_domain(NULL, &out) == TESSERA_EINVAL);
}

static void
test_inode_carries_domain(void)
{
	tessera_inode_record_t in, out;
	memset(&in, 0, sizeof(in));
	in.inode_no = 5;
	in.mode = 0100644;
	in.size = 4096;
	in.quota_domain = 0xDEADBEEFCAFEull;

	uint8_t buf[TESSERA_INODE_RECORD_SIZE];
	CHECK(tessera_encode_inode(&in, buf) == TESSERA_OK);
	memset(&out, 0, sizeof(out));
	CHECK(tessera_decode_inode(buf, &out) == TESSERA_OK);
	CHECK(out.quota_domain == 0xDEADBEEFCAFEull);
	CHECK(memcmp(&in, &out, sizeof(in)) == 0);
}

int
main(void)
{
	test_init();
	test_reserve_release();
	test_unlimited();
	test_null();
	test_codec();
	test_inode_carries_domain();
	if (failures > 0) {
		fprintf(stderr, "%d failure(s)\n", failures);
		return 1;
	}
	printf("ok\n");
	return 0;
}
