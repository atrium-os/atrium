/*
 * sysfil-bench: system call latency, lat_syscall style, for the per-jail
 * system call filter's performance gate (docs/spec/jail-sysfil.md §4).
 *
 *   sysfil-bench [iterations]
 *
 * Prints one line per operation: the best of 7 runs, in ns per call.
 * "Best of" rather than mean, as LMbench does, so a scheduler hiccup does
 * not count as overhead.  Operations:
 *   null      getppid(2)                 every syscall pays the sysfil test
 *   nullio    write(2) of 1 byte to /dev/null
 *   ioctl     ioctl(FIONREAD) on a pipe   kern_ioctl hook
 *   sockopt   getsockopt(SO_SNDBUF)       kern_getsockopt hook
 *   socket    socket(AF_UNIX)+close       kern_socket hook
 * Run it on the host, in an unfiltered jail and in a filtered one (whose
 * lists allow all of the above) to see the cost of the hook and the filter.
 */
#include <sys/types.h>
#include <sys/filio.h>
#include <sys/ioctl.h>
#include <sys/socket.h>
#include <sys/time.h>

#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <time.h>
#include <unistd.h>

static long iters = 2000000;
static int devnull, pfd[2], sock;

static double
now(void)
{
	struct timespec ts;

	clock_gettime(CLOCK_MONOTONIC, &ts);
	return (ts.tv_sec * 1e9 + ts.tv_nsec);
}

static void
op_null(void)
{
	(void)getppid();
}

static void
op_nullio(void)
{
	char c = 0;

	(void)write(devnull, &c, 1);
}

static void
op_ioctl(void)
{
	int n;

	(void)ioctl(pfd[0], FIONREAD, &n);
}

static void
op_sockopt(void)
{
	socklen_t len;
	int v;

	len = sizeof(v);
	(void)getsockopt(sock, SOL_SOCKET, SO_SNDBUF, &v, &len);
}

static void
op_socket(void)
{
	int s;

	s = socket(AF_UNIX, SOCK_DGRAM, 0);
	if (s >= 0)
		close(s);
}

static void
bench(const char *name, void (*fn)(void), long n)
{
	double best, t0, t;
	long i;
	int r;

	best = 1e30;
	for (r = 0; r < 7; r++) {
		t0 = now();
		for (i = 0; i < n; i++)
			fn();
		t = (now() - t0) / n;
		if (t < best)
			best = t;
	}
	printf("%-8s %10.1f ns\n", name, best);
	fflush(stdout);
}

int
main(int argc, char **argv)
{

	if (argc > 1)
		iters = strtol(argv[1], NULL, 0);
	devnull = open("/dev/null", O_WRONLY);
	if (devnull < 0 || pipe(pfd) != 0)
		return (1);
	sock = socket(AF_UNIX, SOCK_STREAM, 0);
	if (sock < 0)
		return (1);
	bench("null", op_null, iters);
	bench("nullio", op_nullio, iters);
	bench("ioctl", op_ioctl, iters);
	bench("sockopt", op_sockopt, iters);
	bench("socket", op_socket, iters / 10);
	return (0);
}
