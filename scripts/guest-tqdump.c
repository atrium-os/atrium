/* Dump a Tessera mount's quota/dedup domain table (diagnostic).
 * usage: tqdump <path-on-the-mount>   — output goes to the kernel console. */
#include <sys/types.h>
#include <sys/ioccom.h>
#include <fcntl.h>
#include <stdio.h>
#include <string.h>
#include <errno.h>
#include <unistd.h>
#define TESSERA_IOC_QUOTA_DUMP _IO('T', 4)
int main(int argc, char **argv)
{
	if (argc < 2) { fprintf(stderr, "usage: %s <path>\n", argv[0]); return 2; }
	int fd = open(argv[1], O_RDONLY);
	if (fd < 0) { fprintf(stderr, "open %s: %s\n", argv[1], strerror(errno)); return 1; }
	if (ioctl(fd, TESSERA_IOC_QUOTA_DUMP) != 0) {
		fprintf(stderr, "ioctl QUOTA_DUMP: %s\n", strerror(errno)); close(fd); return 1;
	}
	close(fd); return 0;
}
