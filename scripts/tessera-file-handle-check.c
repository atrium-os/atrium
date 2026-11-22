/*
 * Build on the host for FreeBSD, then run as root on a disposable guest.
 * The argument must be an empty test directory on a live Tessera volume.
 * Run "issue", reboot the guest, then run "redeem" in the same directory.
 */
#include <sys/types.h>
#include <sys/ioctl.h>
#include <sys/mount.h>
#include <sys/stat.h>

#include <err.h>
#include <errno.h>
#include <fcntl.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>
#include <unistd.h>

#define TESSERA_VOLUME_UUID _IOR('T', 12, uint8_t[16])

struct saved_handle {
	fhandle_t handle;
	uint8_t uuid[16];
};

static void
put(const char *name, const char *bytes, int flags)
{
	int fd = open(name, O_WRONLY | flags, 0600);
	size_t len = strlen(bytes);

	if (fd == -1 || write(fd, bytes, len) != (ssize_t)len ||
	    fsync(fd) == -1 || close(fd) == -1)
		err(1, "%s", name);
}

static void
uuid(uint8_t value[16])
{
	int fd = open("same-volume", O_RDONLY);

	if (fd == -1 || ioctl(fd, TESSERA_VOLUME_UUID, value) == -1)
		err(1, "volume UUID");
	close(fd);
}

static void
check(const fhandle_t *handle, const char *path)
{
	struct stat named, opened;
	char value[8] = {0};
	int fd = fhopen(handle, O_RDONLY);

	if (fd == -1 || fstat(fd, &opened) == -1 || stat(path, &named) == -1)
		err(1, "open file handle");
	if (opened.st_dev != named.st_dev || opened.st_ino != named.st_ino ||
	    read(fd, value, sizeof(value)) != 6 || strcmp(value, "edited") != 0)
		errx(1, "file handle opened the wrong file");
	close(fd);
}

int
main(int argc, char **argv)
{
	struct saved_handle saved = {0};
	uint8_t volume[16];
	fhandle_t same_volume;
	int fd;

	if (argc != 3 || (strcmp(argv[1], "issue") != 0 &&
	    strcmp(argv[1], "redeem") != 0))
		errx(2, "usage: %s issue|redeem DIRECTORY", argv[0]);
	if (chdir(argv[2]) == -1)
		err(1, "%s", argv[2]);
	if (strcmp(argv[1], "issue") == 0) {
		put("original", "before", O_CREAT | O_EXCL);
		put("same-volume", "", O_CREAT | O_EXCL);
		uuid(saved.uuid);
		if (getfh("original", &saved.handle) == -1)
			err(1, "getfh");
		put("original", "edited", O_TRUNC);
		if (mkdir("moved", 0700) == -1 ||
		    rename("original", "moved/actual") == -1)
			err(1, "move");
		put("original", "replacement", O_CREAT | O_EXCL);
		check(&saved.handle, "moved/actual");
		fd = open("handle", O_WRONLY | O_CREAT | O_EXCL, 0600);
		if (fd == -1 || write(fd, &saved, sizeof(saved)) != sizeof(saved) ||
		    fsync(fd) == -1)
			err(1, "store handle");
		close(fd);
		puts("PASS handle survived in-place write, rename, and replacement of the old name");
		return (0);
	}
	fd = open("handle", O_RDONLY);
	if (fd == -1 || read(fd, &saved, sizeof(saved)) != sizeof(saved))
		err(1, "load handle");
	close(fd);
	uuid(volume);
	if (memcmp(volume, saved.uuid, sizeof(volume)) != 0 ||
	    getfh("same-volume", &same_volume) == -1)
		errx(1, "could not identify the original Tessera volume");
	saved.handle.fh_fsid = same_volume.fh_fsid;
	check(&saved.handle, "moved/actual");
	if (unlink("moved/actual") == -1)
		err(1, "unlink original");
	put("moved/actual", "edited", O_CREAT | O_EXCL);
	if (fhopen(&saved.handle, O_RDONLY) != -1 || errno != ESTALE)
		errx(1, "deleted handle did not return ESTALE");
	puts("PASS handle reopened after reboot and rejected a byte-identical replacement");
	return (0);
}
