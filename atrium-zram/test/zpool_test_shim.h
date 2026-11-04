/* Userspace stand-ins for the kernel hooks zram_pool.c uses (test only). */
#include <sys/queue.h>
#include <errno.h>
#include <stdint.h>
#include <stdlib.h>
#include <string.h>

extern int zpt_fail_pages;	/* >0: fail that many page allocations */
extern int zpt_fail_hdrs;
extern long zpt_pages_live;	/* bytes of span memory outstanding */

static inline void *
zpt_pages_alloc(size_t n)
{
	void *p;
	if (zpt_fail_pages > 0) { zpt_fail_pages--; return NULL; }
	p = aligned_alloc(4096, n);
	if (p) zpt_pages_live += (long)n;
	return p;
}
static inline void
zpt_pages_free(void *p, size_t n)
{
	zpt_pages_live -= (long)n;
	free(p);
}
static inline void *
zpt_hdr_alloc(size_t n)
{
	if (zpt_fail_hdrs > 0) { zpt_fail_hdrs--; return NULL; }
	return malloc(n);
}

#define	ZP_PAGES_ALLOC(bytes)	zpt_pages_alloc(bytes)
#define	ZP_PAGES_ALLOC_WAIT(bytes) zpt_pages_alloc(bytes)
#define	ZP_PAGES_FREE(p, bytes)	zpt_pages_free((p), (bytes))
#define	ZP_HDR_ALLOC()		zpt_hdr_alloc(sizeof(struct zspan))
#define	ZP_HDR_ALLOC_WAIT()	malloc(sizeof(struct zspan))
#define	ZP_HDR_FREE(h)		free(h)
#define	ZP_ARRAY_ALLOC(bytes)	calloc(1, (bytes))
#define	ZP_ARRAY_FREE(p)	free(p)
#define	ZP_LOCK(zp)		((void)0)
#define	ZP_UNLOCK(zp)		((void)0)
