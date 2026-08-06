/* Standalone driver: feed corpus files to LLVMFuzzerTestOneInput WITHOUT
 * libFuzzer.
 *
 * ★ WHY THIS EXISTS — it is not a convenience, it is the only way to get ASAN
 * over this code on macOS. Two constraints collide:
 *
 *   - Apple clang has ASAN but ships no libFuzzer runtime
 *     (libclang_rt.fuzzer_osx.a is simply absent), so it cannot build a
 *     -fsanitize=fuzzer target at all.
 *   - Homebrew LLVM 21 has libFuzzer, but its ASAN runtime DEADLOCKS at
 *     startup on Darwin 25.5 before main() ever runs: AsanInitInternal →
 *     InitializeShadowMemory → get_dyld_hdr → _Block_copy → malloc →
 *     __sanitizer_mz_malloc → back into AsanInitFromRtl, which then spins
 *     forever in StaticSpinMutex::LockSlow. ASAN's own malloc interceptor
 *     re-enters ASAN's initialization. Verified: -fsanitize=fuzzer and
 *     -fsanitize=fuzzer,undefined both run; adding `address` hangs.
 *
 * So no single compiler on this host can give us fuzzing AND ASAN. Splitting
 * them does: the Homebrew build EXPLORES (fuzzer + UBSAN) and grows a corpus;
 * this driver, built by Apple clang with address+undefined, REPLAYS that
 * corpus. Memory errors are found in the replay.
 *
 * The split is worth keeping even if the deadlock is fixed upstream. The
 * corpus is committed, so the ASAN replay is a deterministic regression test
 * that runs anywhere, on every build, with no brew dependency — while fuzzing
 * stays an on-demand job needing a special toolchain.
 */
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

int LLVMFuzzerTestOneInput(const uint8_t *data, size_t size);

#define MAX_INPUT (8u << 20)

int
main(int argc, char **argv)
{
	if (argc < 2) {
		fprintf(stderr, "usage: %s <file>...\n", argv[0]);
		return 2;
	}
	unsigned long n = 0;
	for (int i = 1; i < argc; i++) {
		FILE *f = fopen(argv[i], "rb");
		if (f == NULL) { perror(argv[i]); return 1; }
		uint8_t *buf = malloc(MAX_INPUT);
		if (buf == NULL) { fclose(f); return 1; }
		size_t len = fread(buf, 1, MAX_INPUT, f);
		fclose(f);
		/* Hand over a buffer sized to the DATA, not the 8 MiB slab, so
		 * ASAN's redzone sits immediately past the last real byte and a
		 * one-past-the-end read is a report rather than a quiet hit on
		 * the unused tail of the allocation. */
		uint8_t *exact = malloc(len ? len : 1);
		if (exact == NULL) { free(buf); return 1; }
		memcpy(exact, buf, len);
		free(buf);
		(void)LLVMFuzzerTestOneInput(exact, len);
		free(exact);
		n++;
	}
	printf("replayed %lu input(s)\n", n);
	return 0;
}
