/* fill: hold MB megabytes of anonymous memory filled with real file contents
 * (binaries, libraries, text from the paths on stdin), then keep touching it
 * for SECS seconds so the pageout daemon swaps the cold part. A stand-in for
 * an app heap with real data, used to drive atrium_zram under pressure.
 *
 * Every page is checksummed after the fill and verified at the end, so a
 * page that came back from swap wrong (a store bug, or a BIO_DELETE that
 * raced a reuse of its block) is counted, not just survived.
 *
 *   find /usr -type f | ./fill <MB> <SECS>       exit 0 = every page intact */
#include <sys/mman.h>
#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>
#include <stdint.h>
#include <unistd.h>

/* FNV-1a over the page's 64-bit words. */
static uint64_t
pagesum(const char *p)
{
	const uint64_t *w = (const uint64_t *)p;
	uint64_t h = 0xcbf29ce484222325ULL;
	for (int i = 0; i < 512; i++)
		h = (h ^ w[i]) * 0x100000001b3ULL;
	return (h);
}
int main(int argc, char **argv) {
	size_t mb = atol(argv[1]), len = mb << 20, off = 0;
	int secs = atoi(argv[2]);
	char *m = mmap(NULL, len, PROT_READ | PROT_WRITE, MAP_ANON | MAP_PRIVATE, -1, 0);
	char path[1024];
	if (m == MAP_FAILED) { perror("mmap"); return 1; }
	while (off < len && fgets(path, sizeof path, stdin)) {
		path[strcspn(path, "\n")] = 0;
		int fd = open(path, O_RDONLY);
		if (fd < 0) continue;
		ssize_t n;
		while (off < len && (n = read(fd, m + off, len - off > 65536 ? 65536 : len - off)) > 0)
			off += n;
		close(fd);
		/* page-align each file so pages are not artificially straddled */
		off = (off + 4095) & ~(size_t)4095;
	}
	size_t npages = off / 4096, bad = 0;
	uint64_t *ck = malloc(npages * sizeof(*ck));
	if (ck == NULL) { perror("malloc"); return 1; }
	for (size_t p = 0; p < npages; p++)
		ck[p] = pagesum(m + p * 4096);
	fprintf(stderr, "fill: %zu MB of file data resident, %zu pages checksummed\n",
	    off >> 20, npages);
	time_t end = time(NULL) + secs;
	unsigned long x = 88172645463325252UL, sum = 0;
	while (time(NULL) < end) {
		/* hot 10%: touched constantly; the rest goes cold and gets swapped */
		for (int i = 0; i < 10000; i++) {
			x ^= x << 13; x ^= x >> 7; x ^= x << 17;
			sum += m[(x % (off / 10 + 1)) & ~4095UL];
		}
		/* dirty a few random pages anywhere, rewriting a byte with itself:
		 * content unchanged, but the VM frees the page's swap block, so
		 * blocks are freed and reused while the workload runs. */
		for (int i = 0; i < 64; i++) {
			x ^= x << 13; x ^= x >> 7; x ^= x << 17;
			volatile char *q = m + ((x % off) & ~4095UL);
			*q = *q;
		}
		usleep(1000);
	}
	/* read everything back: every swapped page must come back intact */
	for (size_t p = 0; p < npages; p++)
		if (pagesum(m + p * 4096) != ck[p])
			bad++;
	fprintf(stderr, "fill: done (%lu), %zu of %zu pages corrupt\n", sum, bad, npages);
	return (bad == 0 ? 0 : 2);
}
