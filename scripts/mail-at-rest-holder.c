/*
 * The swap probe of scripts/mail-at-rest-vm-check.sh (mail.md §6.4a A5).
 *
 *   mail-at-rest-holder hold MIB MARKER STATUS
 *       MIB of incompressible pages (random bytes), each carrying MARKER at
 *       offset 64, so a page that reaches zram's backing store is written
 *       raw (atrium-zram-v2.md §4.5) and the marker is on the disk as it is,
 *       unless the store is encrypted. Every second it writes to STATUS
 *       how many of its pages are resident (mincore(2)), as
 *       "resident R of N".
 *   mail-at-rest-holder press CAPMIB STATUS
 *       Allocates incompressible memory 64 MiB at a time until the holder's
 *       STATUS says fewer than half its pages are resident, or CAPMIB is
 *       reached; prints "PRESS_DONE mib=M resident=R of N" and keeps the
 *       memory until killed, so the holder stays out.
 *   mail-at-rest-holder count MARKER < DEVICE
 *       Prints how many times MARKER occurs in standard input (a raw device
 *       read), streaming, so a device of zeros with no newline in it costs
 *       no memory (grep would hold it as one line).
 *   mail-at-rest-holder scan MARKER DIR
 *       Prints how many regular files under DIR hold MARKER (not following
 *       links; unreadable files are skipped and counted on stderr). It goes
 *       in the gate's Mail bundle, so it runs inside an app's jail, which
 *       has no grep.
 *
 * Built on the host against the sysroot (the gate does it).
 */
#include <sys/types.h>
#include <sys/mman.h>
#include <sys/stat.h>

#include <err.h>
#include <fcntl.h>
#include <fts.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>

#define PAGE 4096
#define STEP (64UL << 20)

static unsigned long
mib_arg(const char *s)
{
	char *end;
	unsigned long v = strtoul(s, &end, 10);

	if (*s == '\0' || *end != '\0' || v == 0 || v > 16384)
		errx(2, "bad MiB %s", s);
	return (v);
}

static char *
incompressible(size_t len)
{
	char *p = mmap(NULL, len, PROT_READ | PROT_WRITE, MAP_ANON | MAP_PRIVATE, -1, 0);

	if (p == MAP_FAILED)
		err(1, "mmap %zu", len);
	arc4random_buf(p, len);
	return (p);
}

static void
write_status(const char *path, size_t resident, size_t n)
{
	char tmp[1024], line[64];
	int fd, len;

	if (snprintf(tmp, sizeof(tmp), "%s.new", path) >= (int)sizeof(tmp))
		errx(1, "status path too long");
	len = snprintf(line, sizeof(line), "resident %zu of %zu\n", resident, n);
	fd = open(tmp, O_WRONLY | O_CREAT | O_TRUNC, 0644);
	if (fd < 0 || write(fd, line, len) != len || close(fd) != 0)
		err(1, "%s", tmp);
	if (rename(tmp, path) != 0)
		err(1, "rename %s", tmp);
}

static int
hold(unsigned long mib, const char *marker, const char *status)
{
	size_t len = mib << 20, n = len / PAGE, mlen = strlen(marker);
	char *p, *vec;

	if (mlen < 20 || mlen > 256)
		errx(2, "marker must be 20-256 bytes");
	p = incompressible(len);
	for (size_t i = 0; i < n; i++)
		memcpy(p + i * PAGE + 64, marker, mlen);
	if ((vec = malloc(n)) == NULL)
		err(1, "malloc");
	printf("HOLD_READY pages=%zu\n", n);
	fflush(stdout);
	for (;;) {
		size_t r = 0;

		/* mincore() reads the page tables only: it pages nothing in. */
		if (mincore(p, len, vec) != 0)
			err(1, "mincore");
		for (size_t i = 0; i < n; i++)
			r += (vec[i] & MINCORE_INCORE) != 0;
		write_status(status, r, n);
		sleep(1);
	}
}

static int
read_status(const char *path, size_t *r, size_t *n)
{
	FILE *f = fopen(path, "r");
	int ok;

	if (f == NULL)
		return (0);
	ok = fscanf(f, "resident %zu of %zu", r, n) == 2 && *n > 0;
	fclose(f);
	return (ok);
}

static int
press(unsigned long cap, const char *status)
{
	unsigned long done = 0;
	size_t r = 0, n = 0;

	while (done < cap) {
		(void)incompressible(STEP);
		done += STEP >> 20;
		/* Give the page daemon a moment, then ask the holder. */
		sleep(1);
		if (read_status(status, &r, &n) && r * 2 < n)
			break;
	}
	/* zram writes RAW pages back within about 5 s (§4.5). */
	sleep(12);
	if (!read_status(status, &r, &n))
		errx(1, "no holder status at %s", status);
	printf("PRESS_DONE mib=%lu resident=%zu of %zu\n", done, r, n);
	fflush(stdout);
	for (;;)
		pause();
}

/* Occurrences of marker in fd, streaming; -1 on a read error. */
static long long
occurrences(int fd, const char *marker)
{
	size_t mlen = strlen(marker), have = 0, len = 1 << 20;
	long long n = 0;
	static char *buf;
	ssize_t r;

	if (buf == NULL && (buf = malloc(len + 256)) == NULL)
		err(1, "malloc");
	/* Keep the last mlen-1 bytes of each read, so a marker across two
	 * reads is counted once. */
	while ((r = read(fd, buf + have, len)) > 0) {
		size_t end = have + (size_t)r, i;

		for (i = 0; i + mlen <= end; i++)
			if (buf[i] == marker[0] && memcmp(buf + i, marker, mlen) == 0)
				n++;
		have = mlen - 1 < end ? mlen - 1 : end;
		memmove(buf, buf + end - have, have);
	}
	return (r < 0 ? -1 : n);
}

static int
count(const char *marker)
{
	long long n;

	if (strlen(marker) < 8 || strlen(marker) > 256)
		errx(2, "marker must be 8-256 bytes");
	if ((n = occurrences(STDIN_FILENO, marker)) < 0)
		err(1, "read");
	printf("%lld\n", n);
	return (0);
}

static int
scan(const char *marker, char *dir)
{
	char *roots[] = { dir, NULL };
	unsigned long files = 0, skipped = 0;
	FTS *fts;
	FTSENT *e;

	if (strlen(marker) < 8 || strlen(marker) > 256)
		errx(2, "marker must be 8-256 bytes");
	if ((fts = fts_open(roots, FTS_PHYSICAL | FTS_NOCHDIR, NULL)) == NULL)
		err(1, "fts_open %s", dir);
	while ((e = fts_read(fts)) != NULL) {
		int fd;
		long long n;

		if (e->fts_info == FTS_DNR || e->fts_info == FTS_ERR) {
			skipped++;
			continue;
		}
		if (e->fts_info != FTS_F)
			continue;
		if ((fd = open(e->fts_accpath, O_RDONLY | O_NOFOLLOW | O_NONBLOCK)) < 0) {
			skipped++;
			continue;
		}
		n = occurrences(fd, marker);
		close(fd);
		if (n < 0)
			skipped++;
		else if (n > 0)
			files++;
	}
	fts_close(fts);
	fprintf(stderr, "scan %s: %lu unreadable\n", dir, skipped);
	printf("%lu\n", files);
	return (0);
}

int
main(int argc, char **argv)
{
	if (argc == 3 && strcmp(argv[1], "count") == 0)
		return (count(argv[2]));
	if (argc == 4 && strcmp(argv[1], "scan") == 0)
		return (scan(argv[2], argv[3]));
	if (argc == 5 && strcmp(argv[1], "hold") == 0)
		return (hold(mib_arg(argv[2]), argv[3], argv[4]));
	if (argc == 4 && strcmp(argv[1], "press") == 0)
		return (press(mib_arg(argv[2]), argv[3]));
	fprintf(stderr, "usage: mail-at-rest-holder hold MIB MARKER STATUS | press CAPMIB STATUS | count MARKER | scan MARKER DIR\n");
	return (2);
}
