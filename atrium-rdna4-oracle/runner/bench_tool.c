/*
 * bench_tool — helpers for the desktop-shader benchmark on the GPU box.
 *
 * gen: write the benchmark inputs there, so the pasted script does not carry
 * megabytes of data. Must stay identical to
 * `desktop()` in atrium-rdna4-oracle/src/cases.rs (same generator, same draw
 * order); `cargo test -p atrium-rdna4-oracle` checks that.
 *
 *   bench_tool gen plain|glyph <nodes> <out-dir>
 *   Writes <out-dir>/scene.in and, for glyph, <out-dir>/glyphs.in, and prints
 *   the glyph total (0 for plain).
 *
 * cmp: compare two output buffers.
 *   bench_tool cmp <a> <b> <bytes> [record]
 *   Compares the first <bytes> bytes; with <record>, as an unordered set of
 *   records of that size (buffers filled in atomic-counter order). Prints
 *   "same" or "differ ..." and exits 0 or 1.
 *
 * SPDX-License-Identifier: MIT OR Apache-2.0
 */
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

static uint64_t st;

static uint32_t
next(void)
{
	st = st * 6364136223846793005ull + 1442695040888963407ull;
	return (uint32_t)(st >> 33);
}

static float
f(void)
{
	return 1.0f + (float)(next() % 2000) / 8.0f;
}

static void
put(const char *dir, const char *name, const void *p, size_t n)
{
	char path[4096];
	snprintf(path, sizeof path, "%s/%s", dir, name);
	FILE *o = fopen(path, "wb");
	if (!o || fwrite(p, 1, n, o) != n) { perror(path); exit(1); }
	fclose(o);
}

static size_t rec;

static int
cmp_rec(const void *a, const void *b)
{
	return memcmp(a, b, rec);
}

static unsigned char *
slurp(const char *path, size_t want)
{
	unsigned char *b = calloc(1, want + 1);
	FILE *f = fopen(path, "rb");
	if (!f) { perror(path); exit(2); }
	size_t n = fread(b, 1, want, f);
	(void)n;
	fclose(f);
	return b;
}

static int
cmp_main(int argc, char **argv)
{
	size_t n = strtoull(argv[4], NULL, 0);
	rec = argc > 5 ? strtoull(argv[5], NULL, 0) : 0;
	unsigned char *a = slurp(argv[2], n), *b = slurp(argv[3], n);
	if (rec) {
		n -= n % rec;
		qsort(a, n / rec, rec, cmp_rec);
		qsort(b, n / rec, rec, cmp_rec);
	}
	for (size_t i = 0; i < n; i++)
		if (a[i] != b[i]) { printf("differ at byte %zu%s\n", i, rec ? " (sorted records)" : ""); return 1; }
	printf("same\n");
	return 0;
}

int
main(int argc, char **argv)
{
	if (argc >= 5 && strcmp(argv[1], "cmp") == 0) return cmp_main(argc, argv);
	if (argc != 5 || strcmp(argv[1], "gen") != 0) {
		fprintf(stderr, "usage: bench_tool gen plain|glyph <nodes> <dir> | cmp <a> <b> <bytes> [record]\n");
		return 2;
	}
	argv++;
	int glyph = strcmp(argv[1], "glyph") == 0;
	uint32_t nodes = (uint32_t)strtoul(argv[2], NULL, 0);
	st = 0x5eed ^ (uint64_t)nodes;
	size_t nf = 4 + 16 * (size_t)nodes;
	float *scene = malloc(nf * 4);
	for (size_t i = 0; i < nf; i++) scene[i] = f();
	memcpy(scene, &nodes, 4);
	uint32_t total = 0;
	if (glyph) {
		for (uint32_t i = 0; i < nodes; i++) {
			uint32_t count = next() % 4;
			size_t meta = (16 + 64 * (size_t)i + 48) / 4;
			memcpy(&scene[meta], &count, 4);
			memcpy(&scene[meta + 1], &total, 4);
			total += count;
		}
		size_t ng = 12 * (size_t)total;
		float *g = malloc(ng * 4 + 4);
		for (size_t i = 0; i < ng; i++) g[i] = f();
		put(argv[3], "glyphs.in", g, ng * 4);
	}
	put(argv[3], "scene.in", scene, nf * 4);
	printf("%u\n", total);
	return 0;
}
