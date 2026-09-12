/* Retire the Tessera quota/dedup domain rooted at a directory.
 *
 * usage: tqdetach <directory>
 *
 * Only the domain's ROOT directory can detach it (EPERM otherwise), and the
 * mount's default domain refuses (EBUSY). The directory does not have to be
 * empty — the intended sequence is detach, then remove the tree.
 *
 * This is the manual counterpart to what atrium-volumes' tessera plugin does
 * in its destroy path; it exists so the domain table can be repaired by hand
 * and so the ioctl can be exercised without a volume manager.
 */
#include <sys/types.h>
#include <sys/ioccom.h>
#include <sys/ioctl.h>
#include <fcntl.h>
#include <stdio.h>
#include <string.h>
#include <errno.h>
#include <unistd.h>

#define TESSERA_IOC_QUOTA_DETACH _IO('T', 5)

int main(int argc, char **argv)
{
	if (argc < 2) {
		fprintf(stderr, "usage: %s <directory>\n", argv[0]);
		return 2;
	}
	int fd = open(argv[1], O_RDONLY);
	if (fd < 0) {
		fprintf(stderr, "open %s: %s\n", argv[1], strerror(errno));
		return 1;
	}
	if (ioctl(fd, TESSERA_IOC_QUOTA_DETACH) != 0) {
		/* Name the likely cause: a bare errno here is hard to act on. */
		const char *why = "";
		if (errno == EPERM)
			why = " (not the domain's root directory)";
		else if (errno == EINVAL)
			why = " (directory is not in any quota domain)";
		else if (errno == EBUSY)
			why = " (that is the mount's default domain)";
		else if (errno == ENOTTY)
			why = " (not a Tessera mount, or the kmod predates this)";
		fprintf(stderr, "ioctl QUOTA_DETACH %s: %s%s\n",
		    argv[1], strerror(errno), why);
		close(fd);
		return 1;
	}
	close(fd);
	return 0;
}
