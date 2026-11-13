/*-
 * SPDX-License-Identifier: BSD-2-Clause
 *
 * aura_sim.ko: a simulated radio for aura_wlan (docs/spec/wifi.md §6.2).
 * Test only. It registers one radio with the core and turns /dev/aura/sim0
 * into a pipe to a user-space bridge that plays the air: requests and
 * transmitted frames go out to the bridge as they are; what the bridge
 * writes comes back in as received frames and events. Nothing is parsed
 * here beyond the fixed sim headers: the core checks requests before they
 * reach us and checks every received frame we pass up.
 */

#include <sys/param.h>
#include <sys/systm.h>
#include <sys/kernel.h>
#include <sys/module.h>
#include <sys/conf.h>
#include <sys/uio.h>
#include <sys/malloc.h>
#include <sys/lock.h>
#include <sys/mutex.h>
#include <sys/queue.h>
#include <sys/selinfo.h>
#include <sys/poll.h>
#include <sys/fcntl.h>
#include <sys/mbuf.h>

#include "../aura_wlan.h"
#include "aura_sim.h"

MALLOC_DEFINE(M_AURASIM, "aura_sim", "Aura simulated radio");

#define	SIM_QMAX	1024

struct sim_msg {
	STAILQ_ENTRY(sim_msg) link;
	size_t		len;
	uint8_t		data[];
};

static struct {
	struct mtx	mtx;
	struct cdev	*dev;
	struct aura_wlan *w;
	bool		open;
	bool		dying;
	STAILQ_HEAD(, sim_msg) q;
	int		qlen;
	uint64_t	qdrops;
	struct selinfo	sel;
} sim;

static struct aura_caps sim_caps;

static void
sim_put(uint32_t type, const void *a, size_t alen, const void *b, size_t blen)
{
	struct aura_sim_hdr h = { .type = type, .len = alen + blen };
	struct sim_msg *m;
	size_t len = sizeof(h) + alen + blen;

	mtx_lock(&sim.mtx);
	if (!sim.open || sim.qlen >= SIM_QMAX) {
		sim.qdrops++;
		mtx_unlock(&sim.mtx);
		return;
	}
	m = malloc(sizeof(*m) + len, M_AURASIM, M_NOWAIT);
	if (m == NULL) {
		sim.qdrops++;
		mtx_unlock(&sim.mtx);
		return;
	}
	m->len = len;
	memcpy(m->data, &h, sizeof(h));
	if (alen)
		memcpy(m->data + sizeof(h), a, alen);
	if (blen)
		memcpy(m->data + sizeof(h) + alen, b, blen);
	STAILQ_INSERT_TAIL(&sim.q, m, link);
	sim.qlen++;
	wakeup(&sim);
	selwakeuppri(&sim.sel, PZERO);
	mtx_unlock(&sim.mtx);
}

/* Under the core's lock: queue, never call back. */
static int
sim_request(void *drv, const uint8_t *msg, size_t len)
{
	sim_put(AURA_SIM_REQ, msg, len, NULL, 0);
	return (0);
}

static void
sim_tx(void *drv, struct mbuf *m, uint8_t tid, bool encrypt)
{
	struct aura_sim_tx t = { .tid = tid, .encrypt = encrypt };
	uint8_t *buf;
	int len = m->m_pkthdr.len;

	if (len <= 0 || len > AURA_MAX_RX_FRAME) {
		m_freem(m);
		return;
	}
	buf = malloc(len, M_AURASIM, M_NOWAIT);
	if (buf != NULL) {
		m_copydata(m, 0, len, buf);
		sim_put(AURA_SIM_TX, &t, sizeof(t), buf, len);
		free(buf, M_AURASIM);
	}
	m_freem(m);
}

static const struct aura_driver_ops sim_ops = {
	.abi = AURA_WLAN_ABI,
	.request = sim_request,
	.tx = sim_tx,
};

static d_open_t		sim_open;
static d_close_t	sim_close;
static d_read_t		sim_read;
static d_write_t	sim_write;
static d_poll_t		sim_poll;

static struct cdevsw sim_cdevsw = {
	.d_version =	D_VERSION,
	.d_name =	"aura_sim",
	.d_open =	sim_open,
	.d_close =	sim_close,
	.d_read =	sim_read,
	.d_write =	sim_write,
	.d_poll =	sim_poll,
};

static int
sim_open(struct cdev *dev, int flags, int fmt, struct thread *td)
{
	int error = 0;

	mtx_lock(&sim.mtx);
	if (sim.open)
		error = EBUSY;
	else
		sim.open = true;
	mtx_unlock(&sim.mtx);
	return (error);
}

static int
sim_close(struct cdev *dev, int flags, int fmt, struct thread *td)
{
	struct sim_msg *m;

	mtx_lock(&sim.mtx);
	sim.open = false;
	while ((m = STAILQ_FIRST(&sim.q)) != NULL) {
		STAILQ_REMOVE_HEAD(&sim.q, link);
		free(m, M_AURASIM);
	}
	sim.qlen = 0;
	mtx_unlock(&sim.mtx);
	return (0);
}

static int
sim_read(struct cdev *dev, struct uio *uio, int ioflag)
{
	struct sim_msg *m;
	int error;

	mtx_lock(&sim.mtx);
	while ((m = STAILQ_FIRST(&sim.q)) == NULL) {
		if (sim.dying || (ioflag & O_NONBLOCK)) {
			mtx_unlock(&sim.mtx);
			return (sim.dying ? ENXIO : EWOULDBLOCK);
		}
		error = msleep(&sim, &sim.mtx, PCATCH, "aurasim", 0);
		if (error != 0) {
			mtx_unlock(&sim.mtx);
			return (error);
		}
	}
	if (uio->uio_resid < (ssize_t)m->len) {
		mtx_unlock(&sim.mtx);
		return (EMSGSIZE);
	}
	STAILQ_REMOVE_HEAD(&sim.q, link);
	sim.qlen--;
	mtx_unlock(&sim.mtx);
	error = uiomove(m->data, m->len, uio);
	free(m, M_AURASIM);
	return (error);
}

static int
sim_write(struct cdev *dev, struct uio *uio, int ioflag)
{
	struct aura_sim_hdr h;
	uint8_t *buf, *p;
	size_t len = uio->uio_resid, plen;
	int error;

	if (len < sizeof(h) || len > AURA_SIM_MAX)
		return (EINVAL);
	buf = malloc(len, M_AURASIM, M_WAITOK);
	error = uiomove(buf, len, uio);
	if (error != 0)
		goto out;
	memcpy(&h, buf, sizeof(h));
	p = buf + sizeof(h);
	plen = len - sizeof(h);
	if (h.len != plen) {
		error = EINVAL;
		goto out;
	}
	switch (h.type) {
	case AURA_SIM_RX_MGMT: {
		struct aura_sim_rx_mgmt r;

		if (plen < sizeof(r)) {
			error = EINVAL;
			break;
		}
		memcpy(&r, p, sizeof(r));
		aura_wlan_rx_mgmt(sim.w, r.freq, r.rssi, r.flags, r.tsf_us,
		    p + sizeof(r), plen - sizeof(r));
		break;
	}
	case AURA_SIM_RX_DATA: {
		struct aura_sim_rx_data r;
		struct aura_rx_info info;

		if (plen < sizeof(r)) {
			error = EINVAL;
			break;
		}
		memcpy(&r, p, sizeof(r));
		info.decrypted = r.decrypted;
		info.key_idx = r.key_idx;
		info.pn = r.pn;
		aura_wlan_rx_data(sim.w, p + sizeof(r), plen - sizeof(r), &info);
		break;
	}
	case AURA_SIM_EVENT:
		aura_wlan_event(sim.w, p, plen);
		break;
	default:
		error = EINVAL;
	}
out:
	free(buf, M_AURASIM);
	return (error);
}

static int
sim_poll(struct cdev *dev, int events, struct thread *td)
{
	int revents = events & (POLLOUT | POLLWRNORM);

	mtx_lock(&sim.mtx);
	if (events & (POLLIN | POLLRDNORM)) {
		if (!STAILQ_EMPTY(&sim.q))
			revents |= events & (POLLIN | POLLRDNORM);
		else
			selrecord(td, &sim.sel);
	}
	mtx_unlock(&sim.mtx);
	return (revents);
}

static void
sim_caps_init(void)
{
	static const uint16_t f24[] = { 2412, 2417, 2422, 2427, 2432, 2437,
	    2442, 2447, 2452, 2457, 2462 };
	static const uint16_t f5[] = { 5180, 5200, 5220, 5240 };
	struct aura_caps *c = &sim_caps;
	int i;

	memset(c, 0, sizeof(*c));
	c->mac[0] = 0x02;
	c->mac[1] = 0xa5;
	c->mac[5] = 0x01;
	c->bands = 0x3;			/* 2.4 and 5 GHz */
	c->max_scan_ssids = 4;
	c->ciphers = (1u << 4) | (1u << 6) | (1u << 8) | (1u << 9);
	c->flags = 0x3;			/* PMF, HW_SCAN */
	for (i = 0; i < (int)nitems(f24); i++)
		c->chan[c->nchan++] = (struct aura_chan){ f24[i], 0, 2000 };
	for (i = 0; i < (int)nitems(f5); i++)
		c->chan[c->nchan++] = (struct aura_chan){ f5[i], 0, 2300 };
}

static int
aura_sim_modevent(module_t mod, int type, void *data)
{
	int error;

	switch (type) {
	case MOD_LOAD:
		mtx_init(&sim.mtx, "aura_sim", NULL, MTX_DEF);
		STAILQ_INIT(&sim.q);
		knlist_init_mtx(&sim.sel.si_note, &sim.mtx);
		sim_caps_init();
		error = aura_wlan_attach(NULL, &sim_ops, &sim_caps, &sim.w);
		if (error != 0) {
			knlist_destroy(&sim.sel.si_note);
			mtx_destroy(&sim.mtx);
			return (error);
		}
		sim.dev = make_dev(&sim_cdevsw, 0, UID_ROOT, GID_WHEEL, 0600,
		    "aura/sim0");
		return (0);
	case MOD_UNLOAD:
		mtx_lock(&sim.mtx);
		sim.dying = true;
		wakeup(&sim);
		mtx_unlock(&sim.mtx);
		destroy_dev(sim.dev);
		aura_wlan_detach(sim.w);
		seldrain(&sim.sel);
		knlist_destroy(&sim.sel.si_note);
		mtx_destroy(&sim.mtx);
		return (0);
	default:
		return (EOPNOTSUPP);
	}
}

static moduledata_t aura_sim_mod = {
	"aura_sim",
	aura_sim_modevent,
	NULL
};
DECLARE_MODULE(aura_sim, aura_sim_mod, SI_SUB_DRIVERS, SI_ORDER_ANY);
MODULE_DEPEND(aura_sim, aura_wlan, AURA_WLAN_ABI, AURA_WLAN_ABI, AURA_WLAN_ABI);
MODULE_VERSION(aura_sim, 1);
