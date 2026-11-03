/* fill: hold MB megabytes of anonymous memory filled with real file contents
 * (binaries, libraries, text from the paths on stdin), then keep touching it
 * for SECS seconds so the pageout daemon swaps the cold part. A stand-in for
 * an app heap with real data, used to drive atrium_zram under pressure. */
#include <sys/mman.h>
#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>
#include <unistd.h>
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
	fprintf(stderr, "fill: %zu MB of file data resident\n", off >> 20);
	time_t end = time(NULL) + secs;
	unsigned long x = 88172645463325252UL, sum = 0;
	while (time(NULL) < end) {
		/* hot 10%: touched constantly; the rest goes cold and gets swapped */
		for (int i = 0; i < 10000; i++) {
			x ^= x << 13; x ^= x >> 7; x ^= x << 17;
			sum += m[(x % (off / 10 + 1)) & ~4095UL];
		}
		usleep(1000);
	}
	/* read everything back once: every swapped page must come back intact */
	for (size_t i = 0; i < off; i += 4096) sum += m[i];
	fprintf(stderr, "fill: done (%lu)\n", sum);
	return 0;
}
