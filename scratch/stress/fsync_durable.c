/* fsync_durable <dir> <n> — create n files, write+fsync each, fsync the
 * parent dir, print DURABLE_DONE, then spin. The harness resets the VM
 * (power cut) after seeing DURABLE_DONE; on remount every file MUST exist
 * with its content (fsync is the POSIX durability barrier). */
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <fcntl.h>
#include <unistd.h>

int main(int argc, char **argv) {
	if (argc != 3) { fprintf(stderr, "usage: %s <dir> <n>\n", argv[0]); return 2; }
	const char *dir = argv[1];
	int n = atoi(argv[2]);
	for (int i = 0; i < n; i++) {
		char path[1024], buf[64];
		snprintf(path, sizeof path, "%s/d%d", dir, i);
		int fd = open(path, O_WRONLY | O_CREAT | O_TRUNC, 0644);
		if (fd < 0) { perror("open"); return 1; }
		int len = snprintf(buf, sizeof buf, "durable-content-%d\n", i);
		if (write(fd, buf, len) != len) { perror("write"); return 1; }
		if (fsync(fd) != 0) { perror("fsync"); return 1; }
		close(fd);
	}
	/* fsync the directory so the new dirents are durable too. */
	int dfd = open(dir, O_RDONLY);
	if (dfd < 0) { perror("opendir"); return 1; }
	if (fsync(dfd) != 0) { perror("fsync dir"); return 1; }
	close(dfd);
	printf("DURABLE_DONE n=%d\n", n);
	fflush(stdout);
	for (;;) sleep(1);   /* hold until the harness power-cuts us */
	return 0;
}
