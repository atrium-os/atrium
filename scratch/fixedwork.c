/*
 * Fixed-instruction-count microbenchmark for #110.
 *
 * Executes exactly the same instruction stream every run: no I/O, no syscalls
 * in the loop, no allocation, no branching on data. Its CPU time is therefore
 * constant BY CONSTRUCTION on a machine that runs instructions at a constant
 * rate. If guest-measured CPU time moves across runs, the guest is not being
 * given a constant rate — i.e. the host is slowing execution — and every other
 * timing number measured in this VM inherits that.
 *
 * The dependency chain is serial (each iteration needs the previous result) so
 * the compiler cannot vectorise or unroll it into something else, and the
 * result is printed so it cannot be dead-coded away.
 */
#include <stdio.h>
#include <stdint.h>

int
main(int argc, char **argv)
{
	(void)argc; (void)argv;
	/* ~3.2e9 dependent integer ops — a couple of seconds on this class of
	 * core, long enough that timer granularity is irrelevant. */
	const uint64_t N = 800000000ULL;
	uint64_t x = 88172645463325252ULL;
	for (uint64_t i = 0; i < N; i++) {
		x ^= x << 13;
		x ^= x >> 7;
		x ^= x << 17;
		x += i;
	}
	printf("%llu\n", (unsigned long long)x);
	return 0;
}
