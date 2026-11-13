/*-
 * SPDX-License-Identifier: BSD-2-Clause
 *
 * aura_wlan.ko: Aura's Wi-Fi kernel core (docs/spec/wifi.md §3).
 *
 * This file is the FreeBSD side only: a radio registry for the hardware
 * drivers, two character devices per radio, the wlanN ifnet, mbufs and
 * locking. Every decision about a byte that came from user space or the air
 * is made by the Rust core (aura_wlan_rs.rs over aura-wlan-core and
 * aura-proto); this file copies bytes and follows the verdicts.
 *
 *   /dev/aura/ctlN  aura-mlme: scans, BSS, management frames, regulatory
 *   /dev/aura/keyN  aura-keys: keys, controlled port, EAPOL
 *
 * One read(2) or write(2) is one aura-proto message. Each node has one
 * opener at a time; closing either leaves the BSS.
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
#include <sys/event.h>
#include <sys/fcntl.h>
#include <sys/mbuf.h>
#include <sys/socket.h>
#include <sys/sockio.h>
#include <sys/sysctl.h>
#include <sys/counter.h>

#include <net/if.h>
#include <net/if_var.h>
#include <net/if_types.h>
#include <net/if_media.h>
#include <net/ethernet.h>

#include "../aura_wlan.h"

MALLOC_DEFINE(M_AURA, "aura_wlan", "Aura Wi-Fi core");

#define	AURA_CHAN_CTL	0
#define	AURA_CHAN_KEY	1
#define	AURA_NCHAN	2
#define	AURA_QMAX	512	/* queued messages per node */
#define	AURA_MAX_MSDU	64
#define	AURA_NDROP	32

/* Mirrors of the Rust seam's #[repr(C)] structs (aura_wlan_rs.rs). */
struct aura_msdu {
	uint8_t		dest;		/* 0 stack, 1 aura-keys */
	uint8_t		da[6];
	uint8_t		sa[6];
	uint16_t	ethertype;
	uint32_t	off;
	uint32_t	len;
};

struct aura_tx {
	uint8_t		hdr[34];
	uint8_t		hdr_len;
	uint8_t		tid;
	uint8_t		encrypt;
	uint8_t		send;
	uint32_t	body_off;
	uint32_t	body_len;
};

size_t	aura_rs_size(void);
size_t	aura_rs_align(void);
int	aura_rs_init(void *mem, void *ctx, const struct aura_caps *caps,
	    uint8_t *scratch);
void	aura_rs_fini(void *mem);
int	aura_rs_write(void *mem, uint32_t chan, const uint8_t *buf, size_t len,
	    uint8_t *out, size_t cap, size_t *out_len, struct aura_tx *tx);
void	aura_rs_close(void *mem, uint32_t chan);
int	aura_rs_port_open(void *mem);
int	aura_rs_tx_hdr(void *mem, const uint8_t *dst, const uint8_t *src,
	    uint16_t ethertype, uint8_t prio, struct aura_tx *tx);
int	aura_rs_rx_data(void *mem, const uint8_t *frame, size_t len,
	    const struct aura_rx_info *info, struct aura_msdu *out, size_t max,
	    size_t *n_out);
size_t	aura_rs_enc_mgmt_rx(uint16_t freq, int8_t rssi, uint8_t flags,
	    uint64_t tsf_us, const uint8_t *frame, size_t len, uint8_t *out,
	    size_t cap);
size_t	aura_rs_enc_eapol_rx(const uint8_t *src, const uint8_t *body,
	    size_t len, uint8_t *out, size_t cap);
int	aura_rs_event(void *mem, const uint8_t *msg, size_t len);
void	aura_rs_abi_sizes(size_t *caps, size_t *rxinfo, size_t *msdu,
	    size_t *tx);

struct aura_qmsg {
	STAILQ_ENTRY(aura_qmsg) link;
	size_t		len;
	uint8_t		data[];
};

struct aura_node {
	struct aura_wlan *w;
	int		which;
	struct cdev	*dev;
	bool		open;
	STAILQ_HEAD(, aura_qmsg) q;
	int		qlen;
	uint64_t	qdrops;
	struct selinfo	sel;
};

struct aura_wlan {
	struct mtx	mtx;
	int		unit;
	void		*drv;
	const struct aura_driver_ops *ops;
	struct aura_caps caps;
	void		*rs;		/* Rust core, aura_rs_size() bytes */
	void		*rs_alloc;
	uint8_t		*scratch;	/* AURA_MSG_MAX, for the core */
	uint8_t		*rbuf;		/* AURA_MSG_MAX, a reply / event */
	uint8_t		*rxbuf;		/* AURA_MAX_RX_FRAME, receive path */
	struct aura_msdu msdu[AURA_MAX_MSDU];
	if_t		ifp;
	struct ifmedia	media;
	struct aura_node node[AURA_NCHAN];
	bool		dying;
	counter_u64_t	drops[AURA_NDROP];
	counter_u64_t	rx_ok, tx_ok;
	struct sysctl_ctx_list sctx;
	LIST_ENTRY(aura_wlan) link;
};

static LIST_HEAD(, aura_wlan) aura_radios = LIST_HEAD_INITIALIZER(aura_radios);
static struct mtx aura_list_mtx;
MTX_SYSINIT(aura_list, &aura_list_mtx, "aura radios", MTX_DEF);
static struct unrhdr *aura_units;

SYSCTL_NODE(_net, OID_AUTO, aura, CTLFLAG_RD | CTLFLAG_MPSAFE, 0,
    "Aura Wi-Fi");

#define	AURA_LOCK(w)	mtx_lock(&(w)->mtx)
#define	AURA_UNLOCK(w)	mtx_unlock(&(w)->mtx)
#define	AURA_ASSERT(w)	mtx_assert(&(w)->mtx, MA_OWNED)

/* Called by the Rust core, under the radio lock. */
int	aura_c_request(void *ctx, const uint8_t *msg, size_t len);

int
aura_c_request(void *ctx, const uint8_t *msg, size_t len)
{
	struct aura_wlan *w = ctx;

	AURA_ASSERT(w);
	return (w->ops->request(w->drv, msg, len));
}

static void
aura_drop(struct aura_wlan *w, int code)
{
	if (code > 0 && code < AURA_NDROP)
		counter_u64_add(w->drops[code], 1);
	else
		counter_u64_add(w->drops[0], 1);
}

/* Queue a message for a node's reader. Lock held. */
static void
aura_enqueue(struct aura_wlan *w, int which, const uint8_t *msg, size_t len)
{
	struct aura_node *n = &w->node[which];
	struct aura_qmsg *m;

	AURA_ASSERT(w);
	if (!n->open || len == 0)
		return;
	if (n->qlen >= AURA_QMAX) {
		n->qdrops++;
		return;
	}
	m = malloc(sizeof(*m) + len, M_AURA, M_NOWAIT);
	if (m == NULL) {
		n->qdrops++;
		return;
	}
	m->len = len;
	memcpy(m->data, msg, len);
	STAILQ_INSERT_TAIL(&n->q, m, link);
	n->qlen++;
	wakeup(n);
	selwakeuppri(&n->sel, PZERO);
	KNOTE_LOCKED(&n->sel.si_note, 0);
}

static void
aura_flush(struct aura_node *n)
{
	struct aura_qmsg *m;

	while ((m = STAILQ_FIRST(&n->q)) != NULL) {
		STAILQ_REMOVE_HEAD(&n->q, link);
		free(m, M_AURA);
	}
	n->qlen = 0;
}

static void
aura_link_update(struct aura_wlan *w)
{
	int up;

	AURA_ASSERT(w);
	up = aura_rs_port_open(w->rs);
	if_link_state_change(w->ifp, up ? LINK_STATE_UP : LINK_STATE_DOWN);
}

/* ---------------------------------------------------------------- cdevs */

static d_open_t		aura_open;
static d_close_t	aura_close;
static d_read_t		aura_read;
static d_write_t	aura_write;
static d_poll_t		aura_poll;
static d_kqfilter_t	aura_kqfilter;

static struct cdevsw aura_cdevsw = {
	.d_version =	D_VERSION,
	.d_name =	"aura",
	.d_open =	aura_open,
	.d_close =	aura_close,
	.d_read =	aura_read,
	.d_write =	aura_write,
	.d_poll =	aura_poll,
	.d_kqfilter =	aura_kqfilter,
};

static int
aura_open(struct cdev *dev, int flags, int fmt, struct thread *td)
{
	struct aura_node *n = dev->si_drv1;
	struct aura_wlan *w = n->w;
	int error = 0;

	AURA_LOCK(w);
	if (w->dying)
		error = ENXIO;
	else if (n->open)
		error = EBUSY;
	else {
		n->open = true;
		n->qdrops = 0;
	}
	AURA_UNLOCK(w);
	return (error);
}

static int
aura_close(struct cdev *dev, int flags, int fmt, struct thread *td)
{
	struct aura_node *n = dev->si_drv1;
	struct aura_wlan *w = n->w;

	AURA_LOCK(w);
	n->open = false;
	aura_flush(n);
	aura_rs_close(w->rs, n->which);
	aura_link_update(w);
	AURA_UNLOCK(w);
	return (0);
}

static int
aura_read(struct cdev *dev, struct uio *uio, int ioflag)
{
	struct aura_node *n = dev->si_drv1;
	struct aura_wlan *w = n->w;
	struct aura_qmsg *m;
	int error;

	AURA_LOCK(w);
	while ((m = STAILQ_FIRST(&n->q)) == NULL) {
		if (w->dying) {
			AURA_UNLOCK(w);
			return (ENXIO);
		}
		if (ioflag & O_NONBLOCK) {
			AURA_UNLOCK(w);
			return (EWOULDBLOCK);
		}
		error = msleep(n, &w->mtx, PCATCH, "aurard", 0);
		if (error != 0) {
			AURA_UNLOCK(w);
			return (error);
		}
	}
	if (uio->uio_resid < (ssize_t)m->len) {
		/* One message per read: the buffer must hold it whole. */
		AURA_UNLOCK(w);
		return (EMSGSIZE);
	}
	STAILQ_REMOVE_HEAD(&n->q, link);
	n->qlen--;
	AURA_UNLOCK(w);
	error = uiomove(m->data, m->len, uio);
	free(m, M_AURA);
	return (error);
}

static void
aura_send_frame(struct aura_wlan *w, const struct aura_tx *tx,
    const uint8_t *body, size_t len)
{
	struct mbuf *m;

	m = m_getm2(NULL, tx->hdr_len + len, M_NOWAIT, MT_DATA, M_PKTHDR);
	if (m == NULL) {
		if_inc_counter(w->ifp, IFCOUNTER_OERRORS, 1);
		return;
	}
	m->m_pkthdr.len = 0;
	m->m_len = 0;
	if (!m_append(m, tx->hdr_len, (c_caddr_t)tx->hdr) ||
	    !m_append(m, len, (c_caddr_t)body)) {
		m_freem(m);
		if_inc_counter(w->ifp, IFCOUNTER_OERRORS, 1);
		return;
	}
	w->ops->tx(w->drv, m, tx->tid, tx->encrypt != 0);
}

static int
aura_write(struct cdev *dev, struct uio *uio, int ioflag)
{
	struct aura_node *n = dev->si_drv1;
	struct aura_wlan *w = n->w;
	struct aura_tx tx;
	size_t len, rlen;
	uint8_t *in, *body = NULL;
	int error;

	len = uio->uio_resid;
	if (len < AURA_HEADER_LEN || len > AURA_MSG_MAX)
		return (EINVAL);
	/* Copy in before taking the lock (uiomove may fault and sleep). */
	in = malloc(len, M_AURA, M_WAITOK);
	error = uiomove(in, len, uio);
	if (error != 0) {
		free(in, M_AURA);
		return (error);
	}
	AURA_LOCK(w);
	if (w->dying) {
		AURA_UNLOCK(w);
		free(in, M_AURA);
		return (ENXIO);
	}
	error = aura_rs_write(w->rs, n->which, in, len, w->rbuf, AURA_MSG_MAX,
	    &rlen, &tx);
	if (error == 0) {
		aura_enqueue(w, n->which, w->rbuf, rlen);
		if (n->which == AURA_CHAN_KEY)
			aura_link_update(w);
		if (tx.send && tx.body_off + (size_t)tx.body_len <= len)
			body = in + tx.body_off;
	}
	AURA_UNLOCK(w);
	if (body != NULL)
		aura_send_frame(w, &tx, body, tx.body_len);
	free(in, M_AURA);
	return (error);
}

static int
aura_poll(struct cdev *dev, int events, struct thread *td)
{
	struct aura_node *n = dev->si_drv1;
	struct aura_wlan *w = n->w;
	int revents = events & (POLLOUT | POLLWRNORM);

	AURA_LOCK(w);
	if (events & (POLLIN | POLLRDNORM)) {
		if (!STAILQ_EMPTY(&n->q) || w->dying)
			revents |= events & (POLLIN | POLLRDNORM);
		else
			selrecord(td, &n->sel);
	}
	AURA_UNLOCK(w);
	return (revents);
}

static void
aura_kq_detach(struct knote *kn)
{
	struct aura_node *n = kn->kn_hook;

	knlist_remove(&n->sel.si_note, kn, 0);
}

static int
aura_kq_read(struct knote *kn, long hint)
{
	struct aura_node *n = kn->kn_hook;

	kn->kn_data = n->qlen;
	return (n->qlen > 0 || n->w->dying);
}

static const struct filterops aura_rfilt = {
	.f_isfd = 1,
	.f_detach = aura_kq_detach,
	.f_event = aura_kq_read,
};

static int
aura_kqfilter(struct cdev *dev, struct knote *kn)
{
	struct aura_node *n = dev->si_drv1;

	if (kn->kn_filter != EVFILT_READ)
		return (EINVAL);
	kn->kn_fop = &aura_rfilt;
	kn->kn_hook = n;
	knlist_add(&n->sel.si_note, kn, 0);
	return (0);
}

/* ---------------------------------------------------------------- ifnet */

static int
aura_transmit(if_t ifp, struct mbuf *m)
{
	struct aura_wlan *w = if_getsoftc(ifp);
	struct ether_header eh;
	struct aura_tx tx;
	int code;

	if (m->m_pkthdr.len < ETHER_HDR_LEN) {
		m_freem(m);
		if_inc_counter(ifp, IFCOUNTER_OERRORS, 1);
		return (EINVAL);
	}
	m_copydata(m, 0, ETHER_HDR_LEN, (caddr_t)&eh);
	AURA_LOCK(w);
	code = aura_rs_tx_hdr(w->rs, eh.ether_dhost, eh.ether_shost,
	    ntohs(eh.ether_type), 0, &tx);
	AURA_UNLOCK(w);
	if (code != 0) {
		aura_drop(w, code);
		m_freem(m);
		if_inc_counter(ifp, IFCOUNTER_OQDROPS, 1);
		return (0);
	}
	m_adj(m, ETHER_HDR_LEN);
	M_PREPEND(m, tx.hdr_len, M_NOWAIT);
	if (m == NULL) {
		if_inc_counter(ifp, IFCOUNTER_OERRORS, 1);
		return (ENOBUFS);
	}
	m_copyback(m, 0, tx.hdr_len, tx.hdr);
	if_inc_counter(ifp, IFCOUNTER_OPACKETS, 1);
	counter_u64_add(w->tx_ok, 1);
	w->ops->tx(w->drv, m, tx.tid, tx.encrypt != 0);
	return (0);
}

static void
aura_qflush(if_t ifp)
{
}

static void
aura_init(void *arg)
{
	struct aura_wlan *w = arg;

	if_setdrvflagbits(w->ifp, IFF_DRV_RUNNING, 0);
}

/* Ethernet media; "active" exactly while the controlled port is open. */
static int
aura_media_change(if_t ifp)
{
	return (0);
}

static void
aura_media_status(if_t ifp, struct ifmediareq *imr)
{
	struct aura_wlan *w = if_getsoftc(ifp);

	imr->ifm_status = IFM_AVALID;
	imr->ifm_active = IFM_ETHER | IFM_AUTO;
	AURA_LOCK(w);
	if (aura_rs_port_open(w->rs))
		imr->ifm_status |= IFM_ACTIVE;
	AURA_UNLOCK(w);
}

static int
aura_ioctl(if_t ifp, u_long cmd, caddr_t data)
{
	struct aura_wlan *w = if_getsoftc(ifp);

	switch (cmd) {
	case SIOCGIFMEDIA:
	case SIOCSIFMEDIA:
		return (ifmedia_ioctl(ifp, (struct ifreq *)data, &w->media, cmd));
	case SIOCSIFFLAGS:
		if (if_getflags(ifp) & IFF_UP)
			if_setdrvflagbits(ifp, IFF_DRV_RUNNING, 0);
		else
			if_setdrvflagbits(ifp, 0, IFF_DRV_RUNNING);
		return (0);
	case SIOCADDMULTI:
	case SIOCDELMULTI:
		return (0);
	case SIOCSIFLLADDR:
		/* The address is aura-mlme's to set (SetMac, randomized per network). */
		return (EPERM);
	default:
		return (ether_ioctl(ifp, cmd, data));
	}
}

/* ---------------------------------------------------------------- driver API */

void
aura_wlan_rx_mgmt(struct aura_wlan *w, uint16_t freq, int8_t rssi,
    uint8_t flags, uint64_t tsf_us, const uint8_t *frame, size_t len)
{
	size_t n;

	AURA_LOCK(w);
	n = aura_rs_enc_mgmt_rx(freq, rssi, flags, tsf_us, frame, len, w->rbuf,
	    AURA_MSG_MAX);
	if (n == 0)
		aura_drop(w, 1);
	else
		aura_enqueue(w, AURA_CHAN_CTL, w->rbuf, n);
	AURA_UNLOCK(w);
}

void
aura_wlan_rx_data(struct aura_wlan *w, const uint8_t *frame, size_t len,
    const struct aura_rx_info *info)
{
	if_t ifp = w->ifp;
	struct ether_header eh;
	struct mbuf *m, *head = NULL, **tail = &head;
	size_t i, n = 0, elen;
	int code;

	if (len > AURA_MAX_RX_FRAME) {
		aura_drop(w, 1);
		return;
	}
	AURA_LOCK(w);
	/*
	 * Classify a private copy: the MSDU offsets refer to it, and the
	 * driver's buffer may be reused as soon as we return. Everything that
	 * uses rxbuf and msdu[] happens before the lock is dropped.
	 */
	memcpy(w->rxbuf, frame, len);
	code = aura_rs_rx_data(w->rs, w->rxbuf, len, info, w->msdu,
	    AURA_MAX_MSDU, &n);
	if (code != 0) {
		aura_drop(w, code);
		AURA_UNLOCK(w);
		if_inc_counter(ifp, IFCOUNTER_IQDROPS, 1);
		return;
	}
	for (i = 0; i < n; i++) {
		struct aura_msdu *d = &w->msdu[i];

		if (d->dest == 1) {
			/* EAPOL: to aura-keys, never to the stack. */
			elen = aura_rs_enc_eapol_rx(d->sa, w->rxbuf + d->off,
			    d->len, w->rbuf, AURA_MSG_MAX);
			if (elen != 0)
				aura_enqueue(w, AURA_CHAN_KEY, w->rbuf, elen);
			else
				aura_drop(w, 1);
			continue;
		}
		memcpy(eh.ether_dhost, d->da, ETHER_ADDR_LEN);
		memcpy(eh.ether_shost, d->sa, ETHER_ADDR_LEN);
		eh.ether_type = htons(d->ethertype);
		m = m_getm2(NULL, ETHER_HDR_LEN + d->len, M_NOWAIT, MT_DATA,
		    M_PKTHDR);
		if (m == NULL) {
			if_inc_counter(ifp, IFCOUNTER_IQDROPS, 1);
			continue;
		}
		m->m_pkthdr.len = m->m_len = 0;
		if (!m_append(m, ETHER_HDR_LEN, (c_caddr_t)&eh) ||
		    !m_append(m, d->len, (c_caddr_t)(w->rxbuf + d->off))) {
			m_freem(m);
			if_inc_counter(ifp, IFCOUNTER_IQDROPS, 1);
			continue;
		}
		m->m_pkthdr.rcvif = ifp;
		*tail = m;
		tail = &m->m_nextpkt;
	}
	AURA_UNLOCK(w);
	/* Up the stack without the lock. */
	while ((m = head) != NULL) {
		head = m->m_nextpkt;
		m->m_nextpkt = NULL;
		if_inc_counter(ifp, IFCOUNTER_IPACKETS, 1);
		counter_u64_add(w->rx_ok, 1);
		if_input(ifp, m);
	}
}

void
aura_wlan_event(struct aura_wlan *w, const uint8_t *msg, size_t len)
{
	int to;

	AURA_LOCK(w);
	to = aura_rs_event(w->rs, msg, len);
	if (to < 0)
		aura_drop(w, 1);
	else {
		if (to & 1)
			aura_enqueue(w, AURA_CHAN_CTL, msg, len);
		if (to & 2)
			aura_enqueue(w, AURA_CHAN_KEY, msg, len);
		aura_link_update(w);
	}
	AURA_UNLOCK(w);
}

static int
aura_sysctl_drops(SYSCTL_HANDLER_ARGS)
{
	struct aura_wlan *w = arg1;
	uint64_t v[AURA_NDROP];
	int i;

	for (i = 0; i < AURA_NDROP; i++)
		v[i] = counter_u64_fetch(w->drops[i]);
	return (SYSCTL_OUT(req, v, sizeof(v)));
}

int
aura_wlan_attach(void *drv, const struct aura_driver_ops *ops,
    const struct aura_caps *caps, struct aura_wlan **out)
{
	struct aura_wlan *w;
	struct sysctl_oid *oid;
	if_t ifp;
	size_t sz, al, a, b, c, d;
	char name[16];
	int error, i;

	if (ops == NULL || ops->abi != AURA_WLAN_ABI || ops->request == NULL ||
	    ops->tx == NULL)
		return (EINVAL);
	aura_rs_abi_sizes(&a, &b, &c, &d);
	if (a != sizeof(struct aura_caps) || b != sizeof(struct aura_rx_info) ||
	    c != sizeof(struct aura_msdu) || d != sizeof(struct aura_tx)) {
		printf("aura_wlan: C/Rust struct mismatch %zu/%zu %zu/%zu "
		    "%zu/%zu %zu/%zu\n", a, sizeof(struct aura_caps), b,
		    sizeof(struct aura_rx_info), c, sizeof(struct aura_msdu),
		    d, sizeof(struct aura_tx));
		return (EINVAL);
	}

	w = malloc(sizeof(*w), M_AURA, M_WAITOK | M_ZERO);
	mtx_init(&w->mtx, "aura_wlan", NULL, MTX_DEF);
	w->drv = drv;
	w->ops = ops;
	w->caps = *caps;
	w->scratch = malloc(AURA_MSG_MAX, M_AURA, M_WAITOK);
	w->rbuf = malloc(AURA_MSG_MAX, M_AURA, M_WAITOK);
	w->rxbuf = malloc(AURA_MAX_RX_FRAME, M_AURA, M_WAITOK);
	sz = aura_rs_size();
	al = aura_rs_align();
	w->rs_alloc = malloc(sz + al, M_AURA, M_WAITOK | M_ZERO);
	w->rs = (void *)roundup2((uintptr_t)w->rs_alloc, al);
	for (i = 0; i < AURA_NDROP; i++)
		w->drops[i] = counter_u64_alloc(M_WAITOK);
	w->rx_ok = counter_u64_alloc(M_WAITOK);
	w->tx_ok = counter_u64_alloc(M_WAITOK);

	error = aura_rs_init(w->rs, w, &w->caps, w->scratch);
	if (error != 0)
		goto fail;

	w->unit = alloc_unr(aura_units);
	for (i = 0; i < AURA_NCHAN; i++) {
		struct aura_node *n = &w->node[i];

		n->w = w;
		n->which = i;
		STAILQ_INIT(&n->q);
		knlist_init_mtx(&n->sel.si_note, &w->mtx);
	}
	/* Owner root, mode 0600: dispensatord grants each node to its jail. */
	w->node[AURA_CHAN_CTL].dev = make_dev(&aura_cdevsw, w->unit, UID_ROOT,
	    GID_WHEEL, 0600, "aura/ctl%d", w->unit);
	w->node[AURA_CHAN_KEY].dev = make_dev(&aura_cdevsw, w->unit, UID_ROOT,
	    GID_WHEEL, 0600, "aura/key%d", w->unit);
	for (i = 0; i < AURA_NCHAN; i++)
		w->node[i].dev->si_drv1 = &w->node[i];

	ifp = w->ifp = if_alloc(IFT_ETHER);
	if_setsoftc(ifp, w);
	if_initname(ifp, "wlan", w->unit);
	if_setflags(ifp, IFF_BROADCAST | IFF_SIMPLEX | IFF_MULTICAST);
	if_setinitfn(ifp, aura_init);
	if_setioctlfn(ifp, aura_ioctl);
	if_settransmitfn(ifp, aura_transmit);
	if_setqflushfn(ifp, aura_qflush);
	ifmedia_init(&w->media, 0, aura_media_change, aura_media_status);
	ifmedia_add(&w->media, IFM_ETHER | IFM_AUTO, 0, NULL);
	ifmedia_set(&w->media, IFM_ETHER | IFM_AUTO);
	ether_ifattach(ifp, caps->mac);
	if_link_state_change(ifp, LINK_STATE_DOWN);

	sysctl_ctx_init(&w->sctx);
	snprintf(name, sizeof(name), "wlan%d", w->unit);
	oid = SYSCTL_ADD_NODE(&w->sctx, SYSCTL_STATIC_CHILDREN(_net_aura),
	    OID_AUTO, name, CTLFLAG_RD | CTLFLAG_MPSAFE, NULL, "radio");
	SYSCTL_ADD_PROC(&w->sctx, SYSCTL_CHILDREN(oid), OID_AUTO, "drops",
	    CTLTYPE_OPAQUE | CTLFLAG_RD | CTLFLAG_MPSAFE, w, 0,
	    aura_sysctl_drops, "QU", "frames dropped, by aura-wlan-core Drop code + 1");
	SYSCTL_ADD_COUNTER_U64(&w->sctx, SYSCTL_CHILDREN(oid), OID_AUTO,
	    "rx_ok", CTLFLAG_RD, &w->rx_ok, "MSDUs passed up");
	SYSCTL_ADD_COUNTER_U64(&w->sctx, SYSCTL_CHILDREN(oid), OID_AUTO,
	    "tx_ok", CTLFLAG_RD, &w->tx_ok, "MSDUs sent");
	SYSCTL_ADD_U64(&w->sctx, SYSCTL_CHILDREN(oid), OID_AUTO, "ctl_qdrops",
	    CTLFLAG_RD, &w->node[AURA_CHAN_CTL].qdrops, 0,
	    "messages for aura-mlme dropped (queue full)");
	SYSCTL_ADD_U64(&w->sctx, SYSCTL_CHILDREN(oid), OID_AUTO, "key_qdrops",
	    CTLFLAG_RD, &w->node[AURA_CHAN_KEY].qdrops, 0,
	    "messages for aura-keys dropped (queue full)");

	mtx_lock(&aura_list_mtx);
	LIST_INSERT_HEAD(&aura_radios, w, link);
	mtx_unlock(&aura_list_mtx);
	*out = w;
	return (0);
fail:
	for (i = 0; i < AURA_NDROP; i++)
		counter_u64_free(w->drops[i]);
	counter_u64_free(w->rx_ok);
	counter_u64_free(w->tx_ok);
	free(w->rs_alloc, M_AURA);
	free(w->rxbuf, M_AURA);
	free(w->rbuf, M_AURA);
	free(w->scratch, M_AURA);
	mtx_destroy(&w->mtx);
	free(w, M_AURA);
	return (error);
}

void
aura_wlan_detach(struct aura_wlan *w)
{
	int i;

	mtx_lock(&aura_list_mtx);
	LIST_REMOVE(w, link);
	mtx_unlock(&aura_list_mtx);

	AURA_LOCK(w);
	w->dying = true;
	for (i = 0; i < AURA_NCHAN; i++) {
		wakeup(&w->node[i]);
		selwakeuppri(&w->node[i].sel, PZERO);
		KNOTE_LOCKED(&w->node[i].sel.si_note, 0);
	}
	AURA_UNLOCK(w);
	for (i = 0; i < AURA_NCHAN; i++)
		destroy_dev(w->node[i].dev);	/* waits for threads inside */
	sysctl_ctx_free(&w->sctx);
	ether_ifdetach(w->ifp);
	ifmedia_removeall(&w->media);
	if_free(w->ifp);

	AURA_LOCK(w);
	aura_rs_fini(w->rs);
	for (i = 0; i < AURA_NCHAN; i++) {
		aura_flush(&w->node[i]);
		knlist_clear(&w->node[i].sel.si_note, 1);
	}
	AURA_UNLOCK(w);
	for (i = 0; i < AURA_NCHAN; i++) {
		seldrain(&w->node[i].sel);
		knlist_destroy(&w->node[i].sel.si_note);
	}
	free_unr(aura_units, w->unit);
	for (i = 0; i < AURA_NDROP; i++)
		counter_u64_free(w->drops[i]);
	counter_u64_free(w->rx_ok);
	counter_u64_free(w->tx_ok);
	explicit_bzero(w->rs_alloc, aura_rs_size() + aura_rs_align());
	free(w->rs_alloc, M_AURA);
	free(w->rxbuf, M_AURA);
	free(w->rbuf, M_AURA);
	explicit_bzero(w->scratch, AURA_MSG_MAX);
	free(w->scratch, M_AURA);
	mtx_destroy(&w->mtx);
	free(w, M_AURA);
}

static int
aura_wlan_modevent(module_t mod, int type, void *data)
{
	switch (type) {
	case MOD_LOAD:
		aura_units = new_unrhdr(0, 255, NULL);
		return (0);
	case MOD_UNLOAD:
		mtx_lock(&aura_list_mtx);
		if (!LIST_EMPTY(&aura_radios)) {
			mtx_unlock(&aura_list_mtx);
			return (EBUSY);
		}
		mtx_unlock(&aura_list_mtx);
		delete_unrhdr(aura_units);
		return (0);
	default:
		return (EOPNOTSUPP);
	}
}

static moduledata_t aura_wlan_mod = {
	"aura_wlan",
	aura_wlan_modevent,
	NULL
};
DECLARE_MODULE(aura_wlan, aura_wlan_mod, SI_SUB_DRIVERS, SI_ORDER_ANY);
MODULE_VERSION(aura_wlan, AURA_WLAN_ABI);
