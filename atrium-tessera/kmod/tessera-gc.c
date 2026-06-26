/* tessera-gc <path> — force an on-demand data-zone GC sweep (no remount).
 * Reclaims orphan packs left by in-place rewrites / xattr churn, which the
 * mount-time and tombstone-flush GC passes don't catch. <path> is any file
 * or directory on the Tessera mount. */
#include <stdio.h>
#include <stdlib.h>
#include <fcntl.h>
#include <stdint.h>
#include <sys/ioctl.h>

#define TESSERA_IOC_GC   _IOR('T', 2, uint64_t)

int main(int argc, char **argv) {
	if (argc != 2) { fprintf(stderr, "usage: %s <path-on-mount>\n", argv[0]); return 2; }
	uint64_t reclaimed = 0;
	int fd = open(argv[1], O_RDONLY);
	if (fd < 0) { perror("open"); return 1; }
	if (ioctl(fd, TESSERA_IOC_GC, &reclaimed) != 0) { perror("ioctl"); return 1; }
	printf("gc: reclaimed %llu orphan pack(s)\n", (unsigned long long)reclaimed);
	return 0;
}
