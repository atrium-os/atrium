/* reader_bench — time tessera_reader reading a file from a raw tessera volume.
 * Validates the loader read path (blob→pack index) at scale.
 *   cc -O2 -I<core/include> reader_bench.c libtessera_core.a -lmd -o reader_bench
 *   ./reader_bench /dev/vtbd3p2 /boot/kernel/kernel
 */
#include <tessera/reader.h>
#include <tessera/btree.h>
#include <fcntl.h>
#include <unistd.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>
#include <stdint.h>

static unsigned long g_n1, g_nb, g_sectors;
static int rd_blk(void *ctx, uint64_t sec, uint8_t *out) {
	int fd = *(int *)ctx;
	g_n1++; g_sectors += 1;
	return pread(fd, out, 4096, (off_t)sec * 4096) == 4096 ? 0 : -1;
}
/* Bulk read path (mirrors the loader's EFI ReadBlocks) — read n sectors in one
 * pread so the data read isn't single-sector-bound. */
static int rd_blks(void *ctx, uint64_t sec, uint32_t n, uint8_t *out) {
	int fd = *(int *)ctx;
	size_t len = (size_t)n * 4096;
	g_nb++; g_sectors += n;
	return pread(fd, out, len, (off_t)sec * 4096) == (ssize_t)len ? 0 : -1;
}

int main(int argc, char **argv) {
	if (argc < 3) { fprintf(stderr, "usage: %s DEV PATH\n", argv[0]); return 2; }
	int fd = open(argv[1], O_RDONLY);
	if (fd < 0) { perror("open"); return 1; }
	tessera_block_io_t io; memset(&io, 0, sizeof io);
	io.read_block = rd_blk; io.ctx = &fd;

	tessera_reader_t *rd = tessera_reader_open_ex(&io, rd_blks);
	if (!rd) { fprintf(stderr, "reader_open failed\n"); return 1; }

	struct timespec t0, t1;
	clock_gettime(CLOCK_MONOTONIC, &t0);

	uint32_t ino, mode; uint64_t size;
	int rc = tessera_reader_lookup(rd, argv[2], &ino, &mode, &size);
	if (rc != 0) { fprintf(stderr, "lookup %s rc=%d\n", argv[2], rc); return 1; }
	printf("lookup ok: ino=%u size=%llu\n", ino, (unsigned long long)size);

	size_t bufsz = getenv("ONESHOT") ? (size_t)size : (1u << 16);
	uint8_t *buf = malloc(bufsz ? bufsz : 1);
	uint64_t off = 0, tot = 0, sum = 0;
	while (off < size) {
		size_t want = (size - off) < bufsz ? (size_t)(size - off) : bufsz;
		size_t got = 0;
		if (tessera_reader_pread(rd, ino, off, buf, want, &got) != 0 || got == 0) {
			fprintf(stderr, "pread failed at off=%llu\n", (unsigned long long)off);
			return 1;
		}
		for (size_t i = 0; i < got; i++) sum += buf[i];   /* touch bytes */
		off += got; tot += got;
	}
	free(buf);
	clock_gettime(CLOCK_MONOTONIC, &t1);
	double dt = (t1.tv_sec - t0.tv_sec) + (t1.tv_nsec - t0.tv_nsec) / 1e9;
	printf("read %llu bytes in %.2fs  (%.1f MB/s)  checksum=%llu\n",
	    (unsigned long long)tot, dt, tot / 1e6 / (dt > 0 ? dt : 1e-9),
	    (unsigned long long)sum);
	printf("  disk I/O: %lu single-sector + %lu bulk reads, %lu sectors (%.1f MiB)\n",
	    g_n1, g_nb, g_sectors, g_sectors * 4096.0 / 1048576.0);
	tessera_reader_close(rd);
	return 0;
}
