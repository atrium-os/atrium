/* tessera-quota-set <dir> <limit_bytes> — mark a directory a quota root. */
#include <stdio.h>
#include <stdlib.h>
#include <fcntl.h>
#include <stdint.h>
#include <sys/ioctl.h>

#define TESSERA_IOC_QUOTA_SET   _IOW('T', 1, uint64_t)

int main(int argc, char **argv) {
	if (argc != 3) { fprintf(stderr, "usage: %s <dir> <bytes>\n", argv[0]); return 2; }
	uint64_t limit = strtoull(argv[2], NULL, 0);
	int fd = open(argv[1], O_RDONLY);
	if (fd < 0) { perror("open"); return 1; }
	if (ioctl(fd, TESSERA_IOC_QUOTA_SET, &limit) != 0) { perror("ioctl"); return 1; }
	printf("quota set on %s: %llu bytes\n", argv[1], (unsigned long long)limit);
	return 0;
}
