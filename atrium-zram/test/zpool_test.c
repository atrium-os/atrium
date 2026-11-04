/*
 * Userspace test of zram_pool.c: random-size alloc/free churn with a
 * pattern written into every object and checked before it is freed; the
 * accounting is audited against a recount at every phase; injected page
 * and header allocation failures exercise the reserve and the ENOMEM path;
 * the byte limit is checked to refuse, not overshoot.
 *
 *   cc -O2 -Wall -I.. -I. -o zpool_test zpool_test.c ../zram_pool.c && ./zpool_test
 */
#include <assert.h>
#include <stdio.h>
#include "zpool_test_shim.h"
#include "../zram_pool.h"

int zpt_fail_pages, zpt_fail_hdrs;
long zpt_pages_live;

#define	N 20000
struct obj { struct zspan *sp; uint16_t idx; uint32_t len; uint32_t seed; int live; };
static struct obj objs[N];
static uint64_t rng = 0x9e3779b97f4a7c15ULL;
static uint32_t rnd(void) { rng ^= rng << 13; rng ^= rng >> 7; rng ^= rng << 17; return (uint32_t)rng; }

static void fill(struct zpool *zp, struct obj *o) {
	unsigned char *p = zpool_ptr(o->sp, o->idx, zpool_objsize(zp, o->sp));
	for (uint32_t i = 0; i < o->len; i++) p[i] = (unsigned char)(o->seed + i * 31);
}
static int check(struct zpool *zp, struct obj *o) {
	unsigned char *p = zpool_ptr(o->sp, o->idx, zpool_objsize(zp, o->sp));
	for (uint32_t i = 0; i < o->len; i++) if (p[i] != (unsigned char)(o->seed + i * 31)) return 0;
	return 1;
}
static void audit(struct zpool *zp, const char *when) {
	uint64_t obj = 0, pool = 0, n = 0;
	struct zspan *sp;
	for (int i = 0; i < N; i++) if (objs[i].live) obj += zpool_objsize(zp, objs[i].sp);
	LIST_FOREACH(sp, &zp->spans, all) { pool += sp->bytes; n++;
		int used = 0; for (int b = 0; b < sp->nobj; b++) used += (sp->map[b >> 6] >> (b & 63)) & 1;
		assert(used == sp->nobj - sp->nfree); assert(sp->nfree < sp->nobj); }
	assert(obj == zp->obj_bytes); assert(pool == zp->pool_bytes); assert(n == zp->nspans);
	assert((long)pool + (long)zp->reserve_n * ZP_CHUNK == zpt_pages_live);
	printf("%-14s live obj %8.2f MB  pool %8.2f MB  waste %5.2f%%  spans %llu  reserve %d\n", when,
	    obj / 1048576.0, pool / 1048576.0, pool ? 100.0 * (pool - obj) / pool : 0.0,
	    (unsigned long long)n, zp->reserve_n);
}
static uint32_t rlen(void) {	/* roughly the measured histogram: 0.1-2.8 KiB, a few RAW */
	uint32_t r = rnd() % 100;
	if (r < 2) return 3073 + rnd() % 1023;
	return 64 + rnd() % 2700;
}

int main(void) {
	struct zpool zp;
	int bad = 0, fails = 0;

	assert(zpool_init(&zp, 16, NULL) == 0);
	/* class table sanity: every class's span fits, objects fit */
	for (int c = 0; c <= ZP_NCLASS; c++) {
		assert(zp.cls[c].nobj >= 1 && zp.cls[c].nobj <= ZP_MAXOBJS);
		assert((uint64_t)zp.cls[c].nobj * zp.cls[c].size <= (uint64_t)zp.cls[c].pages * ZP_PAGE);
	}
	assert(zpool_class(1) == 0 && zpool_class(32) == 0 && zpool_class(33) == 1);
	assert(zpool_class(3072) == ZP_NCLASS - 1 && zpool_class(3073) == ZP_RAWCLASS);

	for (int round = 0; round < 6; round++) {
		for (int i = 0; i < N; i++) {
			struct obj *o = &objs[rnd() % N];
			if (o->live) {
				if (!check(&zp, o)) bad++;
				zpool_free(&zp, o->sp, o->idx);
				o->live = 0;
			} else {
				o->len = rlen(); o->seed = rnd();
				if (zpool_alloc(&zp, o->len, &o->sp, &o->idx) != 0) { fails++; continue; }
				o->live = 1; fill(&zp, o);
			}
		}
		audit(&zp, "churn");
	}
	/* page allocator failure: the reserve must absorb it */
	zpt_fail_pages = 1000;
	int got = 0, enomem = 0;
	for (int i = 0; i < N; i++) {
		struct obj *o = &objs[i];
		if (o->live) continue;
		o->len = rlen(); o->seed = rnd();
		int e = zpool_alloc(&zp, o->len, &o->sp, &o->idx);
		if (e == 0) { o->live = 1; fill(&zp, o); got++; } else { assert(e == ENOMEM); enomem++; }
	}
	zpt_fail_pages = 0;
	printf("under page-alloc failure: %d allocs ok (partial spans + reserve), %d ENOMEM, reserve used %llu\n",
	    got, enomem, (unsigned long long)zp.reserve_used);
	assert(zp.reserve_used > 0 && enomem > 0);
	audit(&zp, "after-fail");
	zpool_refill(&zp);
	assert(zp.reserve_n == zp.reserve_target);
	assert(zp.hdr_n == zp.hdr_target);
	/* header allocator failure: the spare-header list must absorb it */
	for (int i = 0; i < N; i += 2)
		if (objs[i].live) { if (!check(&zp, &objs[i])) bad++; zpool_free(&zp, objs[i].sp, objs[i].idx); objs[i].live = 0; }
	zpt_fail_hdrs = 1 << 30;
	got = 0; enomem = 0;
	for (int i = 0; i < N; i++) {
		struct obj *o = &objs[i];
		if (o->live) continue;
		o->len = rlen(); o->seed = rnd();
		int e = zpool_alloc(&zp, o->len, &o->sp, &o->idx);
		if (e == 0) { o->live = 1; fill(&zp, o); got++; } else { assert(e == ENOMEM); enomem++; }
	}
	zpt_fail_hdrs = 0;
	printf("under header-alloc failure: %d allocs ok, %d ENOMEM, spare headers used %llu of %d\n",
	    got, enomem, (unsigned long long)zp.hdr_reserve_used, zp.hdr_target);
	assert(zp.hdr_reserve_used > 0);
	audit(&zp, "after-hdr-fail");
	zpool_refill(&zp);
	assert(zp.hdr_n == zp.hdr_target);
	/* limit: refuses with ENOSPC, never exceeds */
	zp.limit = zp.pool_bytes + 64 * 1024;
	int enospc = 0;
	for (int i = 0; i < N; i++) {
		struct obj *o = &objs[i];
		if (!o->live) continue;
		if (!check(&zp, o)) bad++;
		zpool_free(&zp, o->sp, o->idx); o->live = 0;
	}
	audit(&zp, "all-freed");
	assert(zp.pool_bytes == 0 && zp.obj_bytes == 0 && zp.nspans == 0);
	zp.limit = 256 * 1024;
	for (int i = 0; i < N; i++) {
		struct obj *o = &objs[i];
		o->len = 2000; o->seed = rnd();
		int e = zpool_alloc(&zp, o->len, &o->sp, &o->idx);
		if (e == ENOSPC) { enospc++; continue; }
		assert(e == 0); o->live = 1; fill(&zp, o);
		assert(zp.pool_bytes <= zp.limit);
	}
	printf("limit 256 KiB: pool %llu bytes, %d ENOSPC\n", (unsigned long long)zp.pool_bytes, enospc);
	assert(enospc > 0);
	for (int i = 0; i < N; i++) if (objs[i].live) { if (!check(&zp, &objs[i])) bad++; zpool_free(&zp, objs[i].sp, objs[i].idx); objs[i].live = 0; }
	zpool_fini(&zp);
	assert(zpt_pages_live == 0);
	printf("%s: %d corrupt objects, %d alloc failures during churn (expected 0)\n", bad ? "FAIL" : "OK", bad, fails);
	return bad || fails ? 1 : 0;
}
