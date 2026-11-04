/* swaptouch MB PASSES [rnd]: write a stamped pattern into MB of anon memory,
 * then PASSES read+verify sweeps, each rewriting every page with the next
 * generation so it goes dirty and swaps out again. With a third argument,
 * odd pages are incompressible (exercises RAW pages and writeback).
 * Prints seconds per phase and the count of wrong pages. */
#include <stdio.h>
#include <stdlib.h>
#include <stdint.h>
#include <string.h>
#include <time.h>
#include <sys/mman.h>
static double now(void){struct timespec t;clock_gettime(CLOCK_MONOTONIC,&t);return t.tv_sec+t.tv_nsec/1e9;}
static int rnd;
static void fillpg(uint64_t *p, uint64_t i, uint64_t gen){
	if (rnd && (i & 1)) {	/* incompressible: xorshift seeded by page and gen */
		uint64_t x = (i + 1) * 0x9E3779B97F4A7C15ull ^ gen;
		for (int w = 0; w < 512; w++) { x ^= x << 13; x ^= x >> 7; x ^= x << 17; p[w] = x; }
		return;
	}
	for (int w = 0; w < 512; w++) p[w] = (w % 8 == 0) ? (i * 2654435761u) ^ gen : (uint64_t)(w & 31) * 0x0101010101010101ull;
}
int main(int argc, char **argv){
	size_t mb = argc > 1 ? atol(argv[1]) : 2500; int passes = argc > 2 ? atoi(argv[2]) : 2; rnd = argc > 3;
	size_t np = mb * 256; char *m = mmap(NULL, np * 4096, PROT_READ|PROT_WRITE, MAP_ANON|MAP_PRIVATE, -1, 0);
	if (m == MAP_FAILED) { perror("mmap"); return 1; }
	double t0 = now();
	for (size_t i = 0; i < np; i++) fillpg((uint64_t *)(m + i * 4096), i, 0);
	printf("write %.2f s\n", now() - t0); fflush(stdout);
	long bad = 0;
	for (int g = 0; g < passes; g++) {
		t0 = now();
		uint64_t ref[512];
		for (size_t i = 0; i < np; i++) {
			uint64_t *p = (uint64_t *)(m + i * 4096);
			fillpg(ref, i, g);
			if (memcmp(p, ref, 4096) != 0) bad++;
			fillpg(p, i, g + 1);
		}
		printf("pass %d %.2f s bad %ld\n", g, now() - t0, bad); fflush(stdout);
	}
	return bad != 0;
}
