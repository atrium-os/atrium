/*
 * tessera_gc_run — CONTRACT PIN, not a GC test.
 *
 * core/src/gc.c is a 20-line stub: tessera_gc_run() ignores its arguments and
 * returns TESSERA_ENOTIMPL. Nothing in the tree calls it. The GC that actually
 * runs — mark/sweep over the data zone, pinscan, the dead-pack apply — lives
 * in the kernel module (tessera_fs_gc_data_zone_ex and friends), where it has
 * access to the flush gate, the pin bitmap and the buffer cache. None of that
 * can be exercised from a userspace unit test.
 *
 * So this file deliberately does NOT test garbage collection. It pins the stub
 * so that:
 *
 *   1. the ENOTIMPL contract is explicit rather than folklore — a caller that
 *      links core and calls tessera_gc_run gets a defined answer, and
 *   2. anyone who IMPLEMENTS gc.c breaks this test immediately, which is the
 *      moment to write real tests rather than the moment to delete this file.
 *
 * A file named test_gc.c that quietly passed while testing nothing would be
 * worse than no file at all: it reads as coverage on the most dangerous
 * subsystem in the filesystem.
 */

#include "tessera/gc.h"
#include "tessera/error.h"

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

static int
null_read(void *ctx, uint64_t s, uint8_t *out)
{
	(void)ctx; (void)s; (void)out;
	return -1;              /* must never be reached: the stub returns first */
}

int
main(void)
{
	tessera_block_io_t io;
	memset(&io, 0, sizeof io);
	io.read_block = null_read;

	tessera_gc_stats_t st;
	memset(&st, 0xAB, sizeof st);

	int rc = tessera_gc_run(&io, 1, 2, &tessera_gc_default_options, &st);
	CHECK(rc == TESSERA_ENOTIMPL);

	/* NULL options and NULL stats must be equally harmless while stubbed. */
	CHECK(tessera_gc_run(&io, 1, 2, NULL, NULL) == TESSERA_ENOTIMPL);

	if (failures != 0) {
		fprintf(stderr, "%d failure(s)\n", failures);
		return 1;
	}
	printf("gc stub pinned at ENOTIMPL (real GC is in the kmod)\n");
	return 0;
}
