/*
 * tq — trigger one synchronous Tessera GC pass and report what it cost.
 *
 * TESSERA_IOC_GC runs gc_data_zone_ex(tmp_, NULL) in the calling thread, so a
 * pass happens exactly when asked instead of waiting on pressure arming. That
 * matters because gc_arm() is a cmpset 0->1: while a background pass runs,
 * every arm is a no-op (measured: 1161 arms, 1 scan), which makes the
 * background path useless as a measurement harness.
 *
 * usage: tq <mountpoint>
 */
#include <sys/types.h>
#include <sys/ioctl.h>
#include <fcntl.h>
#include <stdio.h>
#include <stdint.h>
#include <unistd.h>
#include <string.h>
#include <errno.h>

#define TESSERA_IOC_GC _IOR('T', 2, uint64_t)

int
main(int argc, char **argv)
{
	if (argc != 2) {
		fprintf(stderr, "usage: %s <mountpoint>\n", argv[0]);
		return (2);
	}
	int fd = open(argv[1], O_RDONLY);
	if (fd < 0) { perror("open"); return (2); }

	uint64_t reclaimed = 0;
	if (ioctl(fd, TESSERA_IOC_GC, &reclaimed) != 0) {
		fprintf(stderr, "ioctl(TESSERA_IOC_GC): %s\n", strerror(errno));
		close(fd);
		return (1);
	}
	close(fd);
	printf("reclaimed=%llu\n", (unsigned long long)reclaimed);
	return (0);
}
