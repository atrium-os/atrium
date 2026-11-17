/*
 * tessera_crypt_ocf.c — the kernel backend for tessera/crypt.h over OCF
 * (docs/spec/tessera-encryption.md §5.1, E1).
 *
 * Sets of three OCF sessions, made once at module load with no key (each
 * request carries its own in crp_cipher_key, which ossl(4), armv8crypto/aesni
 * and cryptosoft all honour):
 *
 *   - AES-256-GCM, 12-byte IV, 16-byte tag (XAES's inner AEAD; the key is
 *     the per-nonce derived Kx, so a per-request key is the only way);
 *   - ChaCha20-Poly1305, 12-byte IV (XChaCha's inner AEAD after HChaCha20);
 *   - AES-256-CBC, used one block at a time with a zero IV, which is AES-256
 *     on that block: XAES's key derivation needs two such blocks per unit.
 *
 * The drivers are the constant-time ones OCF picks (ossl(4)'s OpenSSL
 * assembly, the AES instructions); nothing here implements a cipher.
 * Requests are dispatched and waited for (the fs calls seal/open from
 * contexts that may sleep), so the backend works with a synchronous or an
 * asynchronous driver.
 *
 * ★ ONE REQUEST PER SESSION AT A TIME. armv8crypto and aesni apply a
 * request's crp_cipher_key by rewriting the SESSION's key schedule
 * (armv8_crypto_cipher_setup(ses, ...)), unlocked, and then encrypt from
 * it. Every XAES unit has its own derived key, so two threads sharing a
 * session seal or open under each other's key: a node written that way
 * carries a tag no reader can reproduce, and an open of a good node fails.
 * Measured as EAUTH on metadata the writer itself had just written and on
 * pack fronts nothing overwrote, only under concurrent load. So the
 * sessions come in slots (one per CPU, capped), each slot held under its
 * own sx lock for the whole request.
 *
 * Before installing, the backend runs the XAES-256-GCM (c2sp.org) and
 * XChaCha20-Poly1305 (draft-irtf-cfrg-xchacha §A.3.1) known answers through
 * the full tessera_aead_* path. If OCF has no driver or a vector fails, no
 * backend is installed and a sealed mount gets ENOTIMPL; unsealed volumes
 * are unaffected.
 */
#include <sys/param.h>
#include <sys/systm.h>
#include <sys/bus.h>
#include <sys/kernel.h>
#include <sys/lock.h>
#include <sys/malloc.h>
#include <sys/mutex.h>
#include <sys/sx.h>
#include <sys/smp.h>
#include <sys/pcpu.h>
#include <sys/libkern.h>

#include <opencrypto/cryptodev.h>

#include "tessera/crypt.h"
#include "tessera/error.h"

#include "tessera_crypt_ocf.h"

static MALLOC_DEFINE(M_TESSERA_CRYPT, "tessera_crypt", "Tessera OCF buffers");

#define OCF_SLOTS_MAX 16

struct ocf_slot {
	struct sx        lock;
	crypto_session_t gcm, cp, cbc;
};

static struct ocf_slot ocf_slots[OCF_SLOTS_MAX];
static int ocf_nslots;
static int ocf_ready;

enum { OCF_GCM, OCF_CP, OCF_CBC };

/* A free slot, locked: try each from this CPU's, else wait on this CPU's. */
static struct ocf_slot *
ocf_slot_get(void)
{
	int n = ocf_nslots, start = (int)(curcpu % (u_int)n);

	for (int i = 0; i < n; i++) {
		struct ocf_slot *sl = &ocf_slots[(start + i) % n];

		if (sx_try_xlock(&sl->lock))
			return (sl);
	}
	sx_xlock(&ocf_slots[start].lock);
	return (&ocf_slots[start]);
}

static void
ocf_slot_put(struct ocf_slot *sl)
{
	sx_xunlock(&sl->lock);
}

static crypto_session_t
ocf_slot_session(const struct ocf_slot *sl, int which)
{
	switch (which) {
	case OCF_GCM: return (sl->gcm);
	case OCF_CP:  return (sl->cp);
	default:      return (sl->cbc);
	}
}

struct ocf_wait {
	struct mtx lock;
	int        done;
};

static int
ocf_cb(struct cryptop *crp)
{
	struct ocf_wait *w = crp->crp_opaque;

	mtx_lock(&w->lock);
	w->done = 1;
	mtx_unlock(&w->lock);
	wakeup(w);
	return (0);
}

/* Dispatch and wait; the OCF errno (0, EBADMSG on a tag mismatch, ...). */
static int
ocf_run(struct cryptop *crp)
{
	struct ocf_wait w;
	int error;

	mtx_init(&w.lock, "tessera_ocf", NULL, MTX_DEF);
	crp->crp_callback = ocf_cb;
	crp->crp_opaque = &w;
	crp->crp_flags |= CRYPTO_F_CBIFSYNC;
	for (;;) {
		w.done = 0;
		error = crypto_dispatch(crp);
		if (error != 0)
			break;
		mtx_lock(&w.lock);
		while (!w.done)
			mtx_sleep(&w, &w.lock, 0, "tsocf", 0);
		mtx_unlock(&w.lock);
		if (crp->crp_etype == EAGAIN) {
			/* the driver went away and OCF moved the session */
			crp->crp_etype = 0;
			continue;
		}
		error = crp->crp_etype;
		break;
	}
	mtx_destroy(&w.lock);
	return (error);
}

static int
ocf_aes256_ecb(const uint8_t key[32], const uint8_t *in, uint8_t *out,
    size_t nblocks)
{
	uint8_t blk[16];
	int error = 0;

	if (!ocf_ready)
		return (TESSERA_ENOTIMPL);
	struct ocf_slot *sl = ocf_slot_get();
	for (size_t i = 0; i < nblocks && error == 0; i++) {
		struct cryptop *crp = crypto_getreq(sl->cbc, M_WAITOK);

		memcpy(blk, in + 16 * i, 16);
		crp->crp_op = CRYPTO_OP_ENCRYPT;
		crp->crp_flags = CRYPTO_F_IV_SEPARATE;
		memset(crp->crp_iv, 0, 16);
		crp->crp_cipher_key = key;
		crypto_use_buf(crp, blk, 16);
		crp->crp_payload_start = 0;
		crp->crp_payload_length = 16;
		error = ocf_run(crp);
		crypto_freereq(crp);
		if (error == 0)
			memcpy(out + 16 * i, blk, 16);
	}
	ocf_slot_put(sl);
	explicit_bzero(blk, sizeof blk);
	return (error == 0 ? TESSERA_OK : TESSERA_EIO);
}

/* Which slot session serves `alg`; -1 for none. */
static int
ocf_aead_session(uint32_t alg)
{
	switch (alg) {
	case TESSERA_AEAD96_AES_256_GCM:       return (OCF_GCM);
	case TESSERA_AEAD96_CHACHA20_POLY1305: return (OCF_CP);
	default:                               return (-1);
	}
}

/* One AEAD request over tmp = payload[len] | tag[16], on a slot of its own
 * for the whole request (see the top of this file). */
static int
ocf_aead(int which, int op, const uint8_t key[32],
    const uint8_t nonce[12], const uint8_t *aad, size_t aad_len,
    uint8_t *tmp, size_t len)
{
	struct cryptop *crp;
	int error;

	if (len > INT_MAX - 16 || aad_len > INT_MAX)
		return (EFBIG);
	struct ocf_slot *sl = ocf_slot_get();
	crp = crypto_getreq(ocf_slot_session(sl, which), M_WAITOK);
	crp->crp_op = op;
	crp->crp_flags = CRYPTO_F_IV_SEPARATE;
	memcpy(crp->crp_iv, nonce, 12);
	crp->crp_cipher_key = key;
	crypto_use_buf(crp, tmp, (int)len + 16);
	crp->crp_aad = aad_len > 0 ? __DECONST(void *, aad) : NULL;
	crp->crp_aad_length = (int)aad_len;
	crp->crp_payload_start = 0;
	crp->crp_payload_length = (int)len;
	crp->crp_digest_start = (int)len;
	error = ocf_run(crp);
	crypto_freereq(crp);
	ocf_slot_put(sl);
	return (error);
}

static int
ocf_seal(uint32_t alg, const uint8_t key[32], const uint8_t nonce[12],
    const uint8_t *aad, size_t aad_len, const uint8_t *pt, size_t len,
    uint8_t *ct, uint8_t tag[16])
{
	int s = ocf_aead_session(alg);
	uint8_t *tmp;
	int error;

	if (!ocf_ready)
		return (TESSERA_ENOTIMPL);
	if (s < 0)
		return (TESSERA_EINCOMPAT);
	tmp = malloc(len + 16, M_TESSERA_CRYPT, M_WAITOK);
	if (len > 0)
		memcpy(tmp, pt, len);
	error = ocf_aead(s, CRYPTO_OP_ENCRYPT | CRYPTO_OP_COMPUTE_DIGEST, key,
	    nonce, aad, aad_len, tmp, len);
	if (error == 0) {
		if (len > 0)
			memcpy(ct, tmp, len);
		memcpy(tag, tmp + len, 16);
	}
	zfree(tmp, M_TESSERA_CRYPT);
	return (error == 0 ? TESSERA_OK :
	    error == EFBIG ? TESSERA_ETOOBIG : TESSERA_EIO);
}

static int
ocf_open(uint32_t alg, const uint8_t key[32], const uint8_t nonce[12],
    const uint8_t *aad, size_t aad_len, const uint8_t *ct, size_t len,
    const uint8_t tag[16], uint8_t *pt)
{
	int s = ocf_aead_session(alg);
	uint8_t *tmp;
	int error;

	if (!ocf_ready)
		return (TESSERA_ENOTIMPL);
	if (s < 0)
		return (TESSERA_EINCOMPAT);
	tmp = malloc(len + 16, M_TESSERA_CRYPT, M_WAITOK);
	if (len > 0)
		memcpy(tmp, ct, len);
	memcpy(tmp + len, tag, 16);
	error = ocf_aead(s, CRYPTO_OP_DECRYPT | CRYPTO_OP_VERIFY_DIGEST, key,
	    nonce, aad, aad_len, tmp, len);
	if (error == 0 && len > 0)
		memcpy(pt, tmp, len);
	zfree(tmp, M_TESSERA_CRYPT);
	if (error == 0)
		return (TESSERA_OK);
	if (len > 0)
		tessera_wipe(pt, len);
	/* EBADMSG is a tag mismatch; anything else is still not plaintext */
	return (error == EFBIG ? TESSERA_ETOOBIG : TESSERA_EAUTH);
}

static void
ocf_random(uint8_t *buf, size_t len)
{
	arc4random_buf(buf, len);
}

static const tessera_crypt_backend_t ocf_backend = {
	.aes256_ecb  = ocf_aes256_ecb,
	.aead96_seal = ocf_seal,
	.aead96_open = ocf_open,
	.random      = ocf_random,
};

/* ── known answers ─────────────────────────────────────────────────── */

static int
hexval(char c)
{
	if (c >= '0' && c <= '9') return (c - '0');
	if (c >= 'a' && c <= 'f') return (c - 'a' + 10);
	return (-1);
}

static size_t
unhex(uint8_t *o, size_t cap, const char *h)
{
	size_t n = 0;

	while (h[0] != '\0' && h[1] != '\0' && n < cap) {
		o[n++] = (uint8_t)(hexval(h[0]) << 4 | hexval(h[1]));
		h += 2;
	}
	return (n);
}

/* Seal under the vector's nonce, compare ct|tag, open it back, and check a
 * flipped tag bit is refused. 0 when all hold. */
static int
kat_one(uint32_t suite, const uint8_t key[32], const uint8_t nonce[24],
    const uint8_t *aad, size_t aad_len, const uint8_t *pt, size_t len,
    const char *want_hex)
{
	uint8_t want[160], ct[128], tag[16], back[128];
	tessera_aead_key_t k;
	int bad = 0;

	if (len > sizeof ct || unhex(want, sizeof want, want_hex) != len + 16)
		return (1);
	if (tessera_aead_key_init(&k, suite, key) != TESSERA_OK)
		return (1);
	if (tessera_aead_seal_nonce(&k, nonce, aad, aad_len, pt, len, ct,
	    tag) != TESSERA_OK || memcmp(ct, want, len) != 0 ||
	    memcmp(tag, want + len, 16) != 0)
		bad = 1;
	if (!bad && (tessera_aead_open(&k, nonce, aad, aad_len, ct, len, tag,
	    back) != TESSERA_OK || memcmp(back, pt, len) != 0))
		bad = 1;
	tag[5] ^= 0x10;
	if (!bad && tessera_aead_open(&k, nonce, aad, aad_len, ct, len, tag,
	    back) != TESSERA_EAUTH)
		bad = 1;
	tessera_aead_key_clear(&k);
	return (bad);
}

static int
ocf_kat(void)
{
	static const char sunscreen[] = "Ladies and Gentlemen of the class of "
	    "'99: If I could offer you only one tip for the future, sunscreen "
	    "would be it.";
	uint8_t key[32], nonce[24], aad[12];
	int bad = 0;

	/* c2sp.org/XAES-256-GCM, both vectors */
	memcpy(nonce, "ABCDEFGHIJKLMNOPQRSTUVWX", 24);
	memset(key, 0x01, 32);
	bad |= kat_one(TESSERA_SUITE_XAES_256_GCM, key, nonce, NULL, 0,
	    (const uint8_t *)"XAES-256-GCM", 12,
	    "ce546ef63c9cc60765923609b33a9a1974e96e52daf2fcf7075e2271");
	memset(key, 0x03, 32);
	bad |= kat_one(TESSERA_SUITE_XAES_256_GCM, key, nonce,
	    (const uint8_t *)"c2sp.org/XAES-256-GCM", 21,
	    (const uint8_t *)"XAES-256-GCM", 12,
	    "986ec1832593df5443a179437fd083bf3fdb41abd740a21f71eb769d") << 1;

	/* draft-irtf-cfrg-xchacha-03 §A.3.1 */
	unhex(key, 32,
	    "808182838485868788898a8b8c8d8e8f909192939495969798999a9b9c9d9e9f");
	unhex(nonce, 24, "404142434445464748494a4b4c4d4e4f5051525354555657");
	unhex(aad, 12, "50515253c0c1c2c3c4c5c6c7");
	bad |= kat_one(TESSERA_SUITE_XCHACHA20_POLY1305, key, nonce, aad, 12,
	    (const uint8_t *)sunscreen, sizeof sunscreen - 1,
	    "bd6d179d3e83d43b9576579493c0e939572a1700252bfaccbed2902c21396cbb"
	    "731c7f1b0b4aa6440bf3a82f4eda7e39ae64c6708c54c216cb96b72e1213b452"
	    "2f8c9ba40db5d945b11b69b982c1bb9e3f3fac2bc369488f76b2383565d3fff9"
	    "21f9664c97637da9768812f615c68b13b52e"
	    "c0875924c1c7987947deafd8780acf49") << 2;
	return (bad);
}

/* ── install / remove ──────────────────────────────────────────────── */

static int
ocf_newsession(crypto_session_t *s, int mode, int alg, int ivlen, int mlen)
{
	struct crypto_session_params csp;

	memset(&csp, 0, sizeof csp);
	csp.csp_mode = mode;
	csp.csp_flags = mode == CSP_MODE_AEAD ? CSP_F_SEPARATE_AAD : 0;
	csp.csp_cipher_alg = alg;
	csp.csp_cipher_klen = 32;
	csp.csp_cipher_key = NULL;      /* per request */
	csp.csp_ivlen = ivlen;
	csp.csp_auth_mlen = mlen;
	return (crypto_newsession(s, &csp,
	    CRYPTOCAP_F_HARDWARE | CRYPTOCAP_F_SOFTWARE));
}

static void
ocf_free_sessions(void)
{
	for (int i = 0; i < ocf_nslots; i++) {
		struct ocf_slot *sl = &ocf_slots[i];

		if (sl->gcm != NULL) crypto_freesession(sl->gcm);
		if (sl->cp != NULL)  crypto_freesession(sl->cp);
		if (sl->cbc != NULL) crypto_freesession(sl->cbc);
		sl->gcm = sl->cp = sl->cbc = NULL;
		sx_destroy(&sl->lock);
	}
	ocf_nslots = 0;
}

static const char *
ocf_driver(crypto_session_t s)
{
	device_t d = crypto_find_device_byhid((int)crypto_ses2hid(s));

	return (d != NULL ? device_get_nameunit(d) : "?");
}

int
tessera_crypt_ocf_install(void)
{
	int error = 0, kat, n;

	n = mp_ncpus < 1 ? 1 : mp_ncpus > OCF_SLOTS_MAX ? OCF_SLOTS_MAX :
	    mp_ncpus;
	for (int i = 0; i < n && error == 0; i++) {
		struct ocf_slot *sl = &ocf_slots[i];

		memset(sl, 0, sizeof *sl);
		sx_init(&sl->lock, "tessera_ocf_slot");
		ocf_nslots = i + 1;
		error = ocf_newsession(&sl->gcm, CSP_MODE_AEAD,
		    CRYPTO_AES_NIST_GCM_16, 12, 16);
		if (error == 0)
			error = ocf_newsession(&sl->cp, CSP_MODE_AEAD,
			    CRYPTO_CHACHA20_POLY1305, 12, 16);
		if (error == 0)
			error = ocf_newsession(&sl->cbc, CSP_MODE_CIPHER,
			    CRYPTO_AES_CBC, 16, 0);
	}
	if (error != 0) {
		printf("tessera_fs: no OCF driver for sealed volumes (%d); "
		    "sealed mounts disabled\n", error);
		ocf_free_sessions();
		return (error);
	}
	ocf_ready = 1;
	tessera_crypt_set_backend(&ocf_backend);
	kat = ocf_kat();
	if (kat != 0) {
		printf("tessera_fs: crypto known-answer tests FAILED (mask %#x); "
		    "sealed mounts disabled\n", kat);
		tessera_crypt_ocf_remove();
		return (EIO);
	}
	printf("tessera_fs: sealed volumes ready (OCF: %s, %s; %d session "
	    "slots)\n", ocf_driver(ocf_slots[0].gcm), ocf_driver(ocf_slots[0].cp),
	    ocf_nslots);
	return (0);
}

void
tessera_crypt_ocf_remove(void)
{
	tessera_crypt_set_backend(NULL);
	ocf_ready = 0;
	ocf_free_sessions();
}
