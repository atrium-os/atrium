/*
 * tessera_crypt_ocf.c — the kernel backend for tessera/crypt.h over OCF
 * (docs/spec/tessera-encryption.md §5.1, E1).
 *
 * Three OCF sessions, made once at module load with no key (each request
 * carries its own in crp_cipher_key, which ossl(4), armv8crypto/aesni and
 * cryptosoft all honour):
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
#include <sys/libkern.h>

#include <opencrypto/cryptodev.h>

#include "tessera/crypt.h"
#include "tessera/error.h"

#include "tessera_crypt_ocf.h"

static MALLOC_DEFINE(M_TESSERA_CRYPT, "tessera_crypt", "Tessera OCF buffers");

static crypto_session_t ocf_gcm, ocf_cp, ocf_cbc;
static int ocf_ready;

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
	for (size_t i = 0; i < nblocks && error == 0; i++) {
		struct cryptop *crp = crypto_getreq(ocf_cbc, M_WAITOK);

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
	explicit_bzero(blk, sizeof blk);
	return (error == 0 ? TESSERA_OK : TESSERA_EIO);
}

static crypto_session_t
ocf_aead_session(uint32_t alg)
{
	switch (alg) {
	case TESSERA_AEAD96_AES_256_GCM:       return (ocf_gcm);
	case TESSERA_AEAD96_CHACHA20_POLY1305: return (ocf_cp);
	default:                               return (NULL);
	}
}

/* One AEAD request over tmp = payload[len] | tag[16]. */
static int
ocf_aead(crypto_session_t s, int op, const uint8_t key[32],
    const uint8_t nonce[12], const uint8_t *aad, size_t aad_len,
    uint8_t *tmp, size_t len)
{
	struct cryptop *crp;
	int error;

	if (len > INT_MAX - 16 || aad_len > INT_MAX)
		return (EFBIG);
	crp = crypto_getreq(s, M_WAITOK);
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
	return (error);
}

static int
ocf_seal(uint32_t alg, const uint8_t key[32], const uint8_t nonce[12],
    const uint8_t *aad, size_t aad_len, const uint8_t *pt, size_t len,
    uint8_t *ct, uint8_t tag[16])
{
	crypto_session_t s = ocf_aead_session(alg);
	uint8_t *tmp;
	int error;

	if (!ocf_ready)
		return (TESSERA_ENOTIMPL);
	if (s == NULL)
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
	crypto_session_t s = ocf_aead_session(alg);
	uint8_t *tmp;
	int error;

	if (!ocf_ready)
		return (TESSERA_ENOTIMPL);
	if (s == NULL)
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
	if (ocf_gcm != NULL) crypto_freesession(ocf_gcm);
	if (ocf_cp != NULL)  crypto_freesession(ocf_cp);
	if (ocf_cbc != NULL) crypto_freesession(ocf_cbc);
	ocf_gcm = ocf_cp = ocf_cbc = NULL;
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
	int error, kat;

	error = ocf_newsession(&ocf_gcm, CSP_MODE_AEAD, CRYPTO_AES_NIST_GCM_16,
	    12, 16);
	if (error == 0)
		error = ocf_newsession(&ocf_cp, CSP_MODE_AEAD,
		    CRYPTO_CHACHA20_POLY1305, 12, 16);
	if (error == 0)
		error = ocf_newsession(&ocf_cbc, CSP_MODE_CIPHER,
		    CRYPTO_AES_CBC, 16, 0);
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
	printf("tessera_fs: sealed volumes ready (OCF: %s, %s)\n",
	    ocf_driver(ocf_gcm), ocf_driver(ocf_cp));
	return (0);
}

void
tessera_crypt_ocf_remove(void)
{
	tessera_crypt_set_backend(NULL);
	ocf_ready = 0;
	ocf_free_sessions();
}
