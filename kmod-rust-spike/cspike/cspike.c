/*
 * cspike.c -- the C twin of rspike.rs: /dev/cspike, same ABI (spike_abi.h).
 *
 * Written the way Atrium's C kmods are written (cdevsw, sx/mtx, copyin into
 * malloc(9) memory) so the benchmark compares like with like: both modules
 * zero their allocations (M_ZERO here, a zero fill in Rust) and take the same
 * locks in the same order.
 *
 * SPDX-License-Identifier: BSD-2-Clause
 */
#include <sys/param.h>
#include <sys/systm.h>
#include <sys/conf.h>
#include <sys/kernel.h>
#include <sys/lock.h>
#include <sys/malloc.h>
#include <sys/module.h>
#include <sys/mutex.h>
#include <sys/sx.h>
#include <sys/uio.h>

#include "../spike_abi.h"

static MALLOC_DEFINE(M_CSPIKE, "cspike", "C spike module");

struct cspike_softc {
	struct sx		 lock;		/* blob and fifo */
	uint8_t			*blob;
	size_t			 blob_len;
	uint8_t			 fifo[SPIKE_FIFO_SIZE];
	size_t			 fifo_len;
	struct mtx		 stats_mtx;
	struct spike_stats	 stats;
};

static struct cdev *cspike_dev;

static void
stats_add(struct cspike_softc *sc, uint64_t *field, uint64_t n)
{
	mtx_lock(&sc->stats_mtx);
	*field += n;
	mtx_unlock(&sc->stats_mtx);
}

/* Decode one record stream; see spike_abi.h. */
static int
parse_stream(const uint8_t *p, size_t len, int depth, uint32_t *count,
    uint64_t *sum)
{
	size_t off = 0, vlen, i;
	uint8_t type;
	int error;

	if (depth > SPIKE_MAX_DEPTH)
		return (EINVAL);
	while (off < len) {
		if (len - off < 4)
			return (EINVAL);
		type = p[off];
		if (type == 0 || p[off + 1] != 0)
			return (EINVAL);
		vlen = (size_t)p[off + 2] | ((size_t)p[off + 3] << 8);
		off += 4;
		if (vlen > len - off)
			return (EINVAL);
		if (type == 0xff) {
			error = parse_stream(p + off, vlen, depth + 1, count, sum);
			if (error != 0)
				return (error);
		} else {
			for (i = 0; i < vlen; i++)
				*sum += p[off + i];
			(*count)++;
		}
		off += vlen;
	}
	return (0);
}

static int
cspike_ioctl(struct cdev *dev, u_long cmd, caddr_t data, int fflag,
    struct thread *td)
{
	struct cspike_softc *sc = dev->si_drv1;
	int error;

	switch (cmd) {
	case SPIKE_IOC_NOP:
		stats_add(sc, &sc->stats.nops, 1);
		return (0);

	case SPIKE_IOC_SET: {
		struct spike_buf *b = (struct spike_buf *)data;
		uint8_t *nb, *old;

		if (b->flags != 0 || b->len > SPIKE_MAX_BLOB)
			return (EINVAL);
		nb = malloc(b->len == 0 ? 1 : b->len, M_CSPIKE,
		    M_WAITOK | M_ZERO);
		error = copyin((const void *)(uintptr_t)b->ptr, nb, b->len);
		if (error != 0) {
			free(nb, M_CSPIKE);
			return (error);
		}
		sx_xlock(&sc->lock);
		old = sc->blob;
		sc->blob = nb;
		sc->blob_len = b->len;
		sx_xunlock(&sc->lock);
		free(old, M_CSPIKE);
		stats_add(sc, &sc->stats.sets, 1);
		stats_add(sc, &sc->stats.bytes_in, b->len);
		return (0);
	}

	case SPIKE_IOC_GET: {
		struct spike_buf *b = (struct spike_buf *)data;
		size_t n;

		if (b->flags != 0)
			return (EINVAL);
		sx_slock(&sc->lock);
		n = MIN(b->len, sc->blob_len);
		error = n == 0 ? 0 :
		    copyout(sc->blob, (void *)(uintptr_t)b->ptr, n);
		if (error == 0)
			b->len = sc->blob_len;
		sx_sunlock(&sc->lock);
		if (error == 0) {
			stats_add(sc, &sc->stats.gets, 1);
			stats_add(sc, &sc->stats.bytes_out, n);
		}
		return (error);
	}

	case SPIKE_IOC_PARSE: {
		struct spike_parse *pa = (struct spike_parse *)data;
		uint8_t *buf;
		uint32_t count = 0;
		uint64_t sum = 0;

		if (pa->len > SPIKE_MAX_BLOB)
			return (EINVAL);
		buf = malloc(pa->len == 0 ? 1 : pa->len, M_CSPIKE,
		    M_WAITOK | M_ZERO);
		error = copyin((const void *)(uintptr_t)pa->ptr, buf, pa->len);
		if (error == 0)
			error = parse_stream(buf, pa->len, 0, &count, &sum);
		free(buf, M_CSPIKE);
		stats_add(sc, &sc->stats.parses, 1);
		if (error != 0) {
			stats_add(sc, &sc->stats.parse_errors, 1);
			return (error);
		}
		pa->count = count;
		pa->sum = sum;
		return (0);
	}

	case SPIKE_IOC_STATS: {
		struct spike_stats *st = (struct spike_stats *)data;

		mtx_lock(&sc->stats_mtx);
		*st = sc->stats;
		mtx_unlock(&sc->stats_mtx);
		return (0);
	}

	default:
		return (ENOTTY);
	}
}

static int
cspike_write(struct cdev *dev, struct uio *uio, int ioflag)
{
	struct cspike_softc *sc = dev->si_drv1;
	size_t n;
	int error;

	sx_xlock(&sc->lock);
	n = MIN((size_t)uio->uio_resid, SPIKE_FIFO_SIZE - sc->fifo_len);
	if (n == 0 && uio->uio_resid != 0) {
		sx_xunlock(&sc->lock);
		return (EAGAIN);
	}
	error = uiomove(sc->fifo + sc->fifo_len, n, uio);
	if (error == 0)
		sc->fifo_len += n;
	sx_xunlock(&sc->lock);
	return (error);
}

static int
cspike_read(struct cdev *dev, struct uio *uio, int ioflag)
{
	struct cspike_softc *sc = dev->si_drv1;
	size_t n;
	int error;

	sx_xlock(&sc->lock);
	n = MIN((size_t)uio->uio_resid, sc->fifo_len);
	error = uiomove(sc->fifo, n, uio);
	if (error == 0) {
		memmove(sc->fifo, sc->fifo + n, sc->fifo_len - n);
		sc->fifo_len -= n;
	}
	sx_xunlock(&sc->lock);
	return (error);
}

static struct cdevsw cspike_cdevsw = {
	.d_version =	D_VERSION,
	.d_name =	"cspike",
	.d_ioctl =	cspike_ioctl,
	.d_read =	cspike_read,
	.d_write =	cspike_write,
};

static int
cspike_modevent(module_t mod, int type, void *arg)
{
	struct make_dev_args args;
	struct cspike_softc *sc;
	int error;

	switch (type) {
	case MOD_LOAD:
		sc = malloc(sizeof(*sc), M_CSPIKE, M_WAITOK | M_ZERO);
		sx_init(&sc->lock, "cspike");
		mtx_init(&sc->stats_mtx, "cspike stats", NULL, MTX_DEF);
		make_dev_args_init(&args);
		args.mda_devsw = &cspike_cdevsw;
		args.mda_uid = UID_ROOT;
		args.mda_gid = GID_WHEEL;
		args.mda_mode = 0600;
		args.mda_si_drv1 = sc;
		error = make_dev_s(&args, &cspike_dev, "cspike");
		if (error != 0) {
			mtx_destroy(&sc->stats_mtx);
			sx_destroy(&sc->lock);
			free(sc, M_CSPIKE);
		}
		return (error);
	case MOD_UNLOAD:
		sc = cspike_dev->si_drv1;
		destroy_dev(cspike_dev);	/* waits for threads in the cdevsw */
		free(sc->blob, M_CSPIKE);
		mtx_destroy(&sc->stats_mtx);
		sx_destroy(&sc->lock);
		free(sc, M_CSPIKE);
		return (0);
	default:
		return (EOPNOTSUPP);
	}
}

DEV_MODULE(cspike, cspike_modevent, NULL);
MODULE_VERSION(cspike, 1);
