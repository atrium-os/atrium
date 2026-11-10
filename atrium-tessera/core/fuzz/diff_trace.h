/*
 * diff_trace.h — the result trace fuzz_diff.c compares (see diff_body.c).
 * Not renamed between the two builds: it is the shared vocabulary.
 *
 * SPDX-License-Identifier: BSD-2-Clause
 */
#ifndef DIFF_TRACE_H_
#define DIFF_TRACE_H_

#include <stddef.h>
#include <stdint.h>

enum {
	DIFF_CODEC, DIFF_MANIFEST, DIFF_PACK, DIFF_BTREE, DIFF_JOURNAL,
	DIFF_SUPERBLOCK, DIFF_NTARGETS
};

/* A trace value that matches anything (a documented, allowed difference). */
#define DIFF_ANY 0xd1ffd1ffd1ffd1ffULL

typedef struct {
	uint64_t *v;
	size_t    n, cap;
	int       oom;
} diff_trace_t;

void diff_put(diff_trace_t *t, uint64_t x);

void diff_run_c(int target, const uint8_t *data, size_t len, diff_trace_t *t);
void diff_run_rs(int target, const uint8_t *data, size_t len, diff_trace_t *t);

#endif
