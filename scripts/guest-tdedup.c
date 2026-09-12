/* Set a quota domain's dedup policy (tessera-fs.md §20.2).
 * usage: tdedup <path> <0=global|1=deferred|2=salted>
 * The path must already be a quota root (tquota set ...), because the dedup
 * domain and the quota domain are one record. */
#include <sys/types.h>
#include <sys/ioccom.h>
#include <sys/stat.h>
#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <errno.h>
#include <unistd.h>

#define TESSERA_IOC_DEDUP_POLICY _IOW('T', 3, uint64_t)

int main(int argc, char **argv)
{
	if (argc < 3) { fprintf(stderr, "usage: %s <path> <0|1|2>\n", argv[0]); return 2; }
	uint64_t pol = strtoull(argv[2], NULL, 10);
	int fd = open(argv[1], O_RDONLY);
	if (fd < 0) { fprintf(stderr, "open %s: %s\n", argv[1], strerror(errno)); return 1; }
	if (ioctl(fd, TESSERA_IOC_DEDUP_POLICY, &pol) != 0) {
		fprintf(stderr, "ioctl DEDUP_POLICY(%llu) on %s: %s\n",
		    (unsigned long long)pol, argv[1], strerror(errno));
		close(fd); return 1;
	}
	close(fd);
	printf("dedup_policy=%llu set on %s\n", (unsigned long long)pol, argv[1]);
	return 0;
}
