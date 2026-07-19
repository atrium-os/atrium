/*
 * test_reader — exercise tessera/reader.h against a real volume image.
 *   usage: test_reader <image> <path>
 *     if <path> is a directory: list it (readdir).
 *     if <path> is a file: dump its content to stdout.
 * Not part of `make check` (needs a populated image); built + run manually
 * to validate the loader read path on the host / in-VM.
 */
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <fcntl.h>
#include <unistd.h>

#include "tessera/error.h"
#include "tessera/reader.h"

#define SEC 4096

static int g_fd;
static int rb(void *ctx, uint64_t sector, uint8_t *out)
{
	(void)ctx;
	ssize_t n = pread(g_fd, out, SEC, (off_t)sector * SEC);
	return (n == SEC) ? 0 : -1;
}
/* bulk read: like the loader's tf_read_blocks (one I/O per pack), so this
 * harness measures the reader, not per-sector raw-device latency. */
static int rbb(void *ctx, uint64_t sector, uint32_t count, uint8_t *out)
{
	(void)ctx;
	size_t bytes = (size_t)count * SEC;
	ssize_t n = pread(g_fd, out, bytes, (off_t)sector * SEC);
	return ((size_t)n == bytes) ? 0 : -1;
}

#define S_IFMT  0170000
#define S_IFDIR 0040000
#define S_IFREG 0100000

int
main(int argc, char **argv)
{
	if (argc < 3) { fprintf(stderr, "usage: %s <image> <path>\n", argv[0]); return 2; }
	g_fd = open(argv[1], O_RDONLY);
	if (g_fd < 0) { perror("open"); return 2; }

	tessera_block_io_t io = { .read_block = rb, .write_block = NULL,
	    .alloc = NULL, .free = NULL, .ctx = NULL };
	tessera_reader_t *rd = tessera_reader_open_ex(&io, rbb);
	if (rd == NULL) { fprintf(stderr, "reader_open failed (bad superblock?)\n"); return 1; }

	uint32_t ino, mode; uint64_t size;
	int rc = tessera_reader_lookup(rd, argv[2], &ino, &mode, &size);
	if (rc != TESSERA_OK) { fprintf(stderr, "lookup '%s' -> rc=%d\n", argv[2], rc); return 1; }
	fprintf(stderr, "lookup '%s' -> ino=%u mode=0%o size=%llu\n",
	    argv[2], ino, mode, (unsigned long long)size);

	if ((mode & S_IFMT) == S_IFDIR) {
		fprintf(stderr, "== directory listing ==\n");
		for (uint32_t i = 0; ; i++) {
			char name[256]; uint64_t child; uint32_t cm;
			if (tessera_reader_readdir(rd, ino, i, name, sizeof name, &child, &cm) != TESSERA_OK)
				break;
			printf("%8llu  0%06o  %s\n", (unsigned long long)child, cm, name);
		}
	} else {
		/* dump content in 64 KiB chunks */
		uint64_t off = 0;
		uint8_t *buf = malloc(65536);
		while (off < size) {
			size_t want = (size - off) < 65536 ? (size_t)(size - off) : 65536;
			size_t got = 0;
			if (tessera_reader_pread(rd, ino, off, buf, want, &got) != TESSERA_OK || got == 0) {
				fprintf(stderr, "pread at %llu failed\n", (unsigned long long)off);
				break;
			}
			fwrite(buf, 1, got, stdout);
			off += got;
		}
		free(buf);
		fprintf(stderr, "\n== read %llu / %llu bytes ==\n",
		    (unsigned long long)off, (unsigned long long)size);
	}
	tessera_reader_close(rd);
	close(g_fd);
	return 0;
}
