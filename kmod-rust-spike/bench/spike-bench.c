/*
 * spike-bench.c -- correctness and latency of /dev/rspike (Rust) against
 * /dev/cspike (C).  Runs on the FreeBSD guest as root.
 *
 *   spike-bench test [iterations]   differential test: the same random and
 *                                   malformed inputs to both devices, every
 *                                   result must agree (rc, errno, outputs)
 *   spike-bench bench [ops] [runs]  latency per operation, both devices
 *                                   interleaved run by run; prints the median
 *                                   and min of the per-run means, in ns/op
 *
 * cc -O2 -o spike-bench spike-bench.c
 *
 * SPDX-License-Identifier: BSD-2-Clause
 */
#include <sys/types.h>
#include <sys/ioctl.h>

#include <err.h>
#include <errno.h>
#include <fcntl.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>
#include <unistd.h>

#include "../spike_abi.h"

static const char *devs[2] = { "/dev/cspike", "/dev/rspike" };
static int fds[2];

/* xorshift64*, seeded: the same stream for both devices. */
static uint64_t rng = 0x9e3779b97f4a7c15ULL;
static uint64_t
rnd(void)
{
	rng ^= rng >> 12; rng ^= rng << 25; rng ^= rng >> 27;
	return (rng * 0x2545f4914f6cdd1dULL);
}

/* A record stream: mostly valid, nested, with a mutation now and then. */
static size_t
gen_stream(uint8_t *p, size_t cap, int depth)
{
	size_t off = 0;
	int n = (int)(rnd() % 6);

	while (n-- > 0 && off + 4 <= cap) {
		int box = depth < SPIKE_MAX_DEPTH + 1 && rnd() % 4 == 0;
		size_t room = cap - off - 4, vlen;

		if (box) {
			vlen = gen_stream(p + off + 4, room > 512 ? 512 : room,
			    depth + 1);
			p[off] = 0xff;
		} else {
			vlen = rnd() % 48;
			if (vlen > room)
				vlen = room;
			for (size_t i = 0; i < vlen; i++)
				p[off + 4 + i] = (uint8_t)rnd();
			p[off] = (uint8_t)(1 + rnd() % 254);
		}
		p[off + 1] = 0;
		p[off + 2] = (uint8_t)vlen;
		p[off + 3] = (uint8_t)(vlen >> 8);
		off += 4 + vlen;
	}
	return (off);
}

static void
mutate(uint8_t *p, size_t *len, size_t cap)
{
	switch (rnd() % 8) {
	case 0: if (*len) p[rnd() % *len] = (uint8_t)rnd(); break;	/* flip */
	case 1: if (*len) *len = rnd() % *len; break;			/* truncate */
	case 2: if (*len < cap) p[(*len)++] = (uint8_t)rnd(); break;	/* trailing */
	case 3: if (*len >= 4) { p[2] = 0xff; p[3] = 0xff; } break;	/* huge len */
	case 4: if (*len >= 1) p[0] = 0; break;				/* type 0 */
	default: break;							/* valid */
	}
}

struct res { int rc, err; uint32_t a; uint64_t b; };

static int
same(struct res *x, struct res *y)
{
	return (x->rc == y->rc && x->err == y->err && x->a == y->a &&
	    x->b == y->b);
}

static int
test(long iters)
{
	static uint8_t buf[SPIKE_MAX_BLOB + 64], out[2][SPIKE_MAX_BLOB];
	long bad = 0, parse_ok = 0, parse_einval = 0;

	for (long it = 0; it < iters; it++) {
		struct res r[2];
		size_t len;
		int op = (int)(rnd() % 4);

		if (op == 0 || op == 1) {	/* PARSE (twice as often) */
			len = gen_stream(buf, rnd() % 4 == 0 ? sizeof(buf) : 4096, 0);
			mutate(buf, &len, sizeof(buf));
			for (int d = 0; d < 2; d++) {
				struct spike_parse pa = { (uintptr_t)buf, (uint32_t)len, 0, 0 };
				r[d].rc = ioctl(fds[d], SPIKE_IOC_PARSE, &pa);
				r[d].err = r[d].rc ? errno : 0;
				r[d].a = pa.count; r[d].b = pa.sum;
			}
			if (r[0].rc == 0) parse_ok++;
			else if (r[0].err == EINVAL) parse_einval++;
		} else if (op == 2) {		/* SET then GET */
			len = rnd() % 3 == 0 ? SPIKE_MAX_BLOB + (rnd() % 3) : rnd() % 9000;
			for (size_t i = 0; i < len && i < sizeof(buf); i++)
				buf[i] = (uint8_t)rnd();
			uint32_t getlen = (uint32_t)(rnd() % 10000);
			for (int d = 0; d < 2; d++) {
				struct spike_buf b = { (uintptr_t)buf, (uint32_t)len, 0 };
				struct spike_buf g = { (uintptr_t)out[d], getlen, 0 };
				r[d].rc = ioctl(fds[d], SPIKE_IOC_SET, &b);
				r[d].err = r[d].rc ? errno : 0;
				if (r[d].rc == 0) {
					r[d].rc = ioctl(fds[d], SPIKE_IOC_GET, &g);
					r[d].err = r[d].rc ? errno : 0;
				}
				r[d].a = g.len;
				r[d].b = 0;
				if (r[d].rc == 0)
					for (uint32_t i = 0; i < (getlen < g.len ? getlen : g.len); i++)
						r[d].b = r[d].b * 31 + out[d][i];
			}
		} else {			/* bad pointers and flags */
			uint64_t ptr = rnd() % 2 ? 0x10 : 0xffffffffffff0000ULL;
			uint32_t flags = (uint32_t)(rnd() % 2);
			unsigned long which = rnd() % 2 ? SPIKE_IOC_SET : SPIKE_IOC_GET;
			for (int d = 0; d < 2; d++) {
				struct spike_buf b = { ptr, 64, flags };
				struct spike_parse pa = { ptr, 64, 0, 0 };
				r[d].rc = ioctl(fds[d], which, &b);
				r[d].err = r[d].rc ? errno : 0;
				int rc2 = ioctl(fds[d], SPIKE_IOC_PARSE, &pa);
				r[d].a = (uint32_t)rc2; r[d].b = rc2 ? (uint64_t)errno : 0;
			}
		}
		if (!same(&r[0], &r[1])) {
			if (bad++ < 10)
				printf("MISMATCH it=%ld op=%d C(rc=%d err=%d a=%u b=%ju) "
				    "Rust(rc=%d err=%d a=%u b=%ju)\n", it, op,
				    r[0].rc, r[0].err, r[0].a, (uintmax_t)r[0].b,
				    r[1].rc, r[1].err, r[1].a, (uintmax_t)r[1].b);
		}
	}

	/* FIFO: write/read through uiomove. */
	for (int d = 0; d < 2; d++) {
		char w[SPIKE_FIFO_SIZE + 100], rd[SPIKE_FIFO_SIZE + 100];
		ssize_t n;

		while (read(fds[d], rd, sizeof(rd)) > 0)
			;
		memset(w, 'a' + d, sizeof(w));
		n = write(fds[d], w, sizeof(w));
		if (n != SPIKE_FIFO_SIZE) { printf("FIFO %s: short write %zd\n", devs[d], n); bad++; }
		if (write(fds[d], w, 1) != -1 || errno != EAGAIN) { printf("FIFO %s: full write not EAGAIN\n", devs[d]); bad++; }
		n = read(fds[d], rd, 100);
		if (n != 100 || rd[0] != 'a' + d) { printf("FIFO %s: read %zd\n", devs[d], n); bad++; }
		n = read(fds[d], rd, sizeof(rd));
		if (n != SPIKE_FIFO_SIZE - 100) { printf("FIFO %s: drain %zd\n", devs[d], n); bad++; }
	}

	printf("test: %ld iterations, %ld parse ok, %ld parse EINVAL, %ld mismatches\n",
	    iters, parse_ok, parse_einval, bad);
	return (bad == 0 ? 0 : 1);
}

static double
now_ns(void)
{
	struct timespec ts;

	clock_gettime(CLOCK_MONOTONIC, &ts);
	return (ts.tv_sec * 1e9 + ts.tv_nsec);
}

static int
cmpd(const void *a, const void *b)
{
	double x = *(const double *)a, y = *(const double *)b;
	return (x < y ? -1 : x > y);
}

static void
bench(long ops, int runs)
{
	static uint8_t blob[256], stream[256], out[256];
	size_t slen = 0;
	const char *names[] = { "nop", "set64", "get64", "parse256", "write+read64" };
	double *t[5][2];

	for (int i = 0; i < 5; i++)
		for (int d = 0; d < 2; d++)
			t[i][d] = calloc(runs, sizeof(double));
	memset(blob, 7, sizeof(blob));
	while (slen + 4 + 28 <= sizeof(stream)) {	/* 8 leaves of 28 bytes */
		stream[slen] = 1; stream[slen + 1] = 0;
		stream[slen + 2] = 28; stream[slen + 3] = 0;
		memset(stream + slen + 4, 3, 28);
		slen += 32;
	}

	for (int r = 0; r < runs; r++) {
		for (int dd = 0; dd < 2; dd++) {
			int d = (r % 2) ? 1 - dd : dd;	/* alternate who goes first */
			int fd = fds[d];
			struct spike_buf b = { (uintptr_t)blob, 64, 0 };
			struct spike_buf g = { (uintptr_t)out, 64, 0 };
			struct spike_parse pa = { (uintptr_t)stream, (uint32_t)slen, 0, 0 };
			char c64[64];
			double t0;

			t0 = now_ns();
			for (long i = 0; i < ops; i++)
				if (ioctl(fd, SPIKE_IOC_NOP) != 0) err(1, "nop");
			t[0][d][r] = (now_ns() - t0) / ops;
			t0 = now_ns();
			for (long i = 0; i < ops; i++)
				if (ioctl(fd, SPIKE_IOC_SET, &b) != 0) err(1, "set");
			t[1][d][r] = (now_ns() - t0) / ops;
			t0 = now_ns();
			for (long i = 0; i < ops; i++) {
				g.len = 64;
				if (ioctl(fd, SPIKE_IOC_GET, &g) != 0) err(1, "get");
			}
			t[2][d][r] = (now_ns() - t0) / ops;
			t0 = now_ns();
			for (long i = 0; i < ops; i++)
				if (ioctl(fd, SPIKE_IOC_PARSE, &pa) != 0 || pa.count != 8)
					errx(1, "parse");
			t[3][d][r] = (now_ns() - t0) / ops;
			t0 = now_ns();
			for (long i = 0; i < ops; i++)
				if (write(fd, c64, 64) != 64 || read(fd, c64, 64) != 64)
					err(1, "fifo");
			t[4][d][r] = (now_ns() - t0) / ops;
		}
	}
	printf("%-14s %12s %12s %12s %12s %9s\n", "op (ns/op)", "C median",
	    "Rust median", "C min", "Rust min", "Rust/C");
	for (int i = 0; i < 5; i++) {
		qsort(t[i][0], runs, sizeof(double), cmpd);
		qsort(t[i][1], runs, sizeof(double), cmpd);
		double mc = t[i][0][runs / 2], mr = t[i][1][runs / 2];
		printf("%-14s %12.1f %12.1f %12.1f %12.1f %8.3fx\n", names[i],
		    mc, mr, t[i][0][0], t[i][1][0], mr / mc);
	}
}

int
main(int argc, char **argv)
{
	for (int d = 0; d < 2; d++)
		if ((fds[d] = open(devs[d], O_RDWR)) < 0)
			err(1, "%s", devs[d]);
	if (argc >= 2 && strcmp(argv[1], "test") == 0)
		return (test(argc >= 3 ? atol(argv[2]) : 20000));
	if (argc >= 2 && strcmp(argv[1], "bench") == 0) {
		bench(argc >= 3 ? atol(argv[2]) : 200000,
		    argc >= 4 ? atoi(argv[3]) : 11);
		return (0);
	}
	fprintf(stderr, "usage: spike-bench test [iters] | bench [ops] [runs]\n");
	return (2);
}
