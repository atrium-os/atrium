/*
 * crypt.c — at-rest encryption primitives (tessera/crypt.h,
 * docs/spec/tessera-encryption.md §4.2, §5.1, §6.1).
 *
 * The block ciphers and AEADs come from the installed backend (OCF/ossl in
 * the kernel, libcrypto in the tools). Here: the 192-bit-nonce suites built
 * on them (XAES-256-GCM's CMAC-KDF, XChaCha20's HChaCha20), HMAC/HKDF over
 * the core's SHA-256, the spec's labelled subkeys, and keyed content ids.
 * Checked against published vectors in tests/test_crypt.c.
 */
#include "tessera_compat.h"

#include "tessera/crypt.h"
#include "tessera/error.h"
#include "tessera/hash.h"

static const tessera_crypt_backend_t *g_be;

void
tessera_crypt_set_backend(const tessera_crypt_backend_t *be)
{
	g_be = be;
}

const tessera_crypt_backend_t *
tessera_crypt_backend(void)
{
	return (g_be);
}

void
tessera_wipe(void *p, size_t n)
{
	volatile uint8_t *v = p;

	while (n--)
		*v++ = 0;
}

/* ── HChaCha20 ──────────────────────────────────────────────────────── */

static inline uint32_t
rotl32(uint32_t x, int n)
{
	return ((x << n) | (x >> (32 - n)));
}

static inline uint32_t
le32(const uint8_t *p)
{
	return ((uint32_t)p[0] | (uint32_t)p[1] << 8 | (uint32_t)p[2] << 16 |
	    (uint32_t)p[3] << 24);
}

static inline void
put_le32(uint8_t *p, uint32_t v)
{
	p[0] = (uint8_t)v; p[1] = (uint8_t)(v >> 8);
	p[2] = (uint8_t)(v >> 16); p[3] = (uint8_t)(v >> 24);
}

#define QR(a, b, c, d) do {                                            \
	a += b; d ^= a; d = rotl32(d, 16);                             \
	c += d; b ^= c; b = rotl32(b, 12);                             \
	a += b; d ^= a; d = rotl32(d, 8);                              \
	c += d; b ^= c; b = rotl32(b, 7);                              \
} while (0)

void
tessera_hchacha20(uint8_t out[32], const uint8_t key[32], const uint8_t in[16])
{
	uint32_t x[16];

	x[0] = 0x61707865; x[1] = 0x3320646e; x[2] = 0x79622d32; x[3] = 0x6b206574;
	for (int i = 0; i < 8; i++)
		x[4 + i] = le32(key + 4 * i);
	for (int i = 0; i < 4; i++)
		x[12 + i] = le32(in + 4 * i);
	for (int r = 0; r < 10; r++) {
		QR(x[0], x[4], x[8],  x[12]);
		QR(x[1], x[5], x[9],  x[13]);
		QR(x[2], x[6], x[10], x[14]);
		QR(x[3], x[7], x[11], x[15]);
		QR(x[0], x[5], x[10], x[15]);
		QR(x[1], x[6], x[11], x[12]);
		QR(x[2], x[7], x[8],  x[13]);
		QR(x[3], x[4], x[9],  x[14]);
	}
	for (int i = 0; i < 4; i++)
		put_le32(out + 4 * i, x[i]);
	for (int i = 0; i < 4; i++)
		put_le32(out + 16 + 4 * i, x[12 + i]);
	tessera_wipe(x, sizeof(x));
}

/* ── keys ───────────────────────────────────────────────────────────── */

int
tessera_aead_key_init(tessera_aead_key_t *k, uint32_t suite,
    const uint8_t key[32])
{
	if (k == NULL || key == NULL)
		return (TESSERA_EINVAL);
	memset(k, 0, sizeof(*k));
	switch (suite) {
	case TESSERA_SUITE_XAES_256_GCM: {
		static const uint8_t zero[16];
		uint8_t L[16];
		int err;

		if (g_be == NULL || g_be->aes256_ecb == NULL)
			return (TESSERA_ENOTIMPL);
		/* c2sp steps 1–2: L = AES_K(0^128); K1 = L << 1, ^ 0x87 if MSB. */
		err = g_be->aes256_ecb(key, zero, L, 1);
		if (err != TESSERA_OK)
			return (err);
		for (int i = 0; i < 15; i++)
			k->k1[i] = (uint8_t)(L[i] << 1 | L[i + 1] >> 7);
		/* Constant-time conditional xor of Rb. */
		k->k1[15] = (uint8_t)(L[15] << 1) ^
		    (uint8_t)(0x87 & (uint8_t)-(L[0] >> 7));
		tessera_wipe(L, sizeof(L));
		break;
	}
	case TESSERA_SUITE_XCHACHA20_POLY1305:
		break;
	default:
		return (TESSERA_EINCOMPAT);
	}
	memcpy(k->key, key, 32);
	k->suite = suite;
	return (TESSERA_OK);
}

void
tessera_aead_key_clear(tessera_aead_key_t *k)
{
	if (k != NULL)
		tessera_wipe(k, sizeof(*k));
}

int
tessera_xaes_derive(const tessera_aead_key_t *k, const uint8_t nonce[24],
    uint8_t kx[32])
{
	uint8_t m[32];
	int err;

	if (k == NULL || k->suite != TESSERA_SUITE_XAES_256_GCM)
		return (TESSERA_EINVAL);
	if (g_be == NULL || g_be->aes256_ecb == NULL)
		return (TESSERA_ENOTIMPL);
	/* c2sp steps 3–5: Mi = 0x00 || i || 'X' || 0x00 || N[:12];
	 * Kx = AES_K(M1 ^ K1) || AES_K(M2 ^ K1). */
	for (int i = 0; i < 2; i++) {
		uint8_t *b = m + 16 * i;

		b[0] = 0x00;
		b[1] = (uint8_t)(i + 1);
		b[2] = 0x58;
		b[3] = 0x00;
		memcpy(b + 4, nonce, 12);
		for (int j = 0; j < 16; j++)
			b[j] ^= k->k1[j];
	}
	err = g_be->aes256_ecb(k->key, m, kx, 2);
	tessera_wipe(m, sizeof(m));
	return (err);
}

/* Per-unit 96-bit-nonce key and nonce for either suite. */
static int
unit_key(const tessera_aead_key_t *k, const uint8_t nonce[24],
    uint8_t subkey[32], uint8_t n12[12], uint32_t *alg)
{
	switch (k->suite) {
	case TESSERA_SUITE_XAES_256_GCM:
		*alg = TESSERA_AEAD96_AES_256_GCM;
		memcpy(n12, nonce + 12, 12);
		return (tessera_xaes_derive(k, nonce, subkey));
	case TESSERA_SUITE_XCHACHA20_POLY1305:
		*alg = TESSERA_AEAD96_CHACHA20_POLY1305;
		tessera_hchacha20(subkey, k->key, nonce);
		memset(n12, 0, 4);
		memcpy(n12 + 4, nonce + 16, 8);
		return (TESSERA_OK);
	default:
		return (TESSERA_EINCOMPAT);
	}
}

int
tessera_aead_seal_nonce(const tessera_aead_key_t *k, const uint8_t nonce[24],
    const uint8_t *aad, size_t aad_len, const uint8_t *pt, size_t len,
    uint8_t *ct, uint8_t tag[16])
{
	uint8_t sub[32], n12[12];
	uint32_t alg;
	int err;

	if (k == NULL || nonce == NULL || tag == NULL ||
	    (len > 0 && (pt == NULL || ct == NULL)) ||
	    (aad_len > 0 && aad == NULL))
		return (TESSERA_EINVAL);
	if (g_be == NULL || g_be->aead96_seal == NULL)
		return (TESSERA_ENOTIMPL);
	err = unit_key(k, nonce, sub, n12, &alg);
	if (err == TESSERA_OK)
		err = g_be->aead96_seal(alg, sub, n12, aad, aad_len, pt, len,
		    ct, tag);
	tessera_wipe(sub, sizeof(sub));
	return (err);
}

int
tessera_aead_seal(const tessera_aead_key_t *k, uint8_t nonce_out[24],
    const uint8_t *aad, size_t aad_len, const uint8_t *pt, size_t len,
    uint8_t *ct, uint8_t tag[16])
{
	if (nonce_out == NULL)
		return (TESSERA_EINVAL);
	if (g_be == NULL || g_be->random == NULL)
		return (TESSERA_ENOTIMPL);
	g_be->random(nonce_out, TESSERA_NONCE_SIZE);
	return (tessera_aead_seal_nonce(k, nonce_out, aad, aad_len, pt, len,
	    ct, tag));
}

int
tessera_aead_open(const tessera_aead_key_t *k, const uint8_t nonce[24],
    const uint8_t *aad, size_t aad_len, const uint8_t *ct, size_t len,
    const uint8_t tag[16], uint8_t *pt)
{
	uint8_t sub[32], n12[12];
	uint32_t alg;
	int err;

	if (k == NULL || nonce == NULL || tag == NULL ||
	    (len > 0 && (pt == NULL || ct == NULL)) ||
	    (aad_len > 0 && aad == NULL))
		return (TESSERA_EINVAL);
	if (g_be == NULL || g_be->aead96_open == NULL)
		return (TESSERA_ENOTIMPL);
	err = unit_key(k, nonce, sub, n12, &alg);
	if (err == TESSERA_OK)
		err = g_be->aead96_open(alg, sub, n12, aad, aad_len, ct, len,
		    tag, pt);
	tessera_wipe(sub, sizeof(sub));
	if (err != TESSERA_OK && len > 0)
		tessera_wipe(pt, len);
	return (err == TESSERA_OK ? TESSERA_OK :
	    (err == TESSERA_ENOMEM || err == TESSERA_ENOTIMPL ||
	     err == TESSERA_EINCOMPAT ? err : TESSERA_EAUTH));
}

/* ── HMAC / HKDF ────────────────────────────────────────────────────── */

int
tessera_hmac_sha256_v(const uint8_t *key, size_t key_len,
    const uint8_t *const parts[], const size_t lens[], int nparts,
    uint8_t out[32])
{
	uint8_t k0[64], pad[64], inner[32];
	tessera_sha256_ctx_t *c;

	memset(k0, 0, sizeof(k0));
	if (key_len > 64)
		tessera_sha256(key, key_len, k0);
	else if (key_len > 0)
		memcpy(k0, key, key_len);

	c = tessera_sha256_init();
	if (c == NULL) {
		tessera_wipe(k0, sizeof(k0));
		return (TESSERA_ENOMEM);
	}
	for (int i = 0; i < 64; i++)
		pad[i] = k0[i] ^ 0x36;
	tessera_sha256_update(c, pad, 64);
	for (int i = 0; i < nparts; i++)
		if (lens[i] > 0)
			tessera_sha256_update(c, parts[i], lens[i]);
	tessera_sha256_final(c, inner);
	tessera_sha256_free(c);

	c = tessera_sha256_init();
	if (c == NULL) {
		tessera_wipe(k0, sizeof(k0));
		tessera_wipe(pad, sizeof(pad));
		tessera_wipe(inner, sizeof(inner));
		return (TESSERA_ENOMEM);
	}
	for (int i = 0; i < 64; i++)
		pad[i] = k0[i] ^ 0x5c;
	tessera_sha256_update(c, pad, 64);
	tessera_sha256_update(c, inner, 32);
	tessera_sha256_final(c, out);
	tessera_sha256_free(c);

	tessera_wipe(k0, sizeof(k0));
	tessera_wipe(pad, sizeof(pad));
	tessera_wipe(inner, sizeof(inner));
	return (TESSERA_OK);
}

int
tessera_hmac_sha256(const uint8_t *key, size_t key_len, const uint8_t *msg,
    size_t len, uint8_t out[32])
{
	const uint8_t *parts[1] = { msg };
	size_t lens[1] = { len };

	return (tessera_hmac_sha256_v(key, key_len, parts, lens, 1, out));
}

int
tessera_hkdf_sha256(const uint8_t *salt, size_t salt_len, const uint8_t *ikm,
    size_t ikm_len, const uint8_t *info, size_t info_len, uint8_t *okm,
    size_t okm_len)
{
	static const uint8_t zero_salt[32];
	uint8_t prk[32], t[32];
	size_t done = 0;
	uint8_t ctr = 1;
	int err;

	if (okm_len > 255u * 32u || (okm_len > 0 && okm == NULL))
		return (TESSERA_EINVAL);
	if (salt == NULL || salt_len == 0) {
		salt = zero_salt;
		salt_len = sizeof(zero_salt);
	}
	err = tessera_hmac_sha256(salt, salt_len, ikm, ikm_len, prk);
	if (err != TESSERA_OK)
		return (err);
	while (done < okm_len) {
		const uint8_t *parts[3] = { t, info, &ctr };
		size_t lens[3] = { done == 0 ? 0 : 32, info_len, 1 };
		size_t n;

		err = tessera_hmac_sha256_v(prk, 32, parts, lens, 3, t);
		if (err != TESSERA_OK)
			break;
		n = okm_len - done < 32 ? okm_len - done : 32;
		memcpy(okm + done, t, n);
		done += n;
		ctr++;
	}
	tessera_wipe(prk, sizeof(prk));
	tessera_wipe(t, sizeof(t));
	return (err);
}

static const char kdf_salt[] = "tessera-encryption v1";

int
tessera_subkey(const uint8_t root[32], const char *label,
    const uint8_t volume_uuid[16], uint64_t group_id, uint8_t out[32])
{
	uint8_t info[64];
	size_t n = 0, ll;

	if (root == NULL || label == NULL || volume_uuid == NULL || out == NULL)
		return (TESSERA_EINVAL);
	ll = strlen(label);
	if (ll > sizeof(info) - 16 - 8)
		return (TESSERA_EINVAL);
	memcpy(info, label, ll);
	n = ll;
	memcpy(info + n, volume_uuid, 16);
	n += 16;
	if (group_id != 0) {
		for (int i = 0; i < 8; i++)
			info[n + i] = (uint8_t)(group_id >> (8 * i));
		n += 8;
	}
	return (tessera_hkdf_sha256((const uint8_t *)kdf_salt,
	    sizeof(kdf_salt) - 1, root, 32, info, n, out, 32));
}

int
tessera_key_check(const uint8_t root[32], const uint8_t volume_uuid[16],
    uint64_t group_id, uint8_t out[32])
{
	static const char msg[] = "tessera key check";
	uint8_t kc[32];
	int err;

	err = tessera_subkey(root, TESSERA_KDF_CHECK, volume_uuid, group_id, kc);
	if (err == TESSERA_OK)
		err = tessera_hmac_sha256(kc, 32, (const uint8_t *)msg,
		    sizeof(msg) - 1, out);
	tessera_wipe(kc, sizeof(kc));
	return (err);
}

/* ── keyed ids ──────────────────────────────────────────────────────── */

int
tessera_keyed_id(uint32_t alg, const uint8_t key[32], const uint8_t *data,
    size_t len, tessera_hash_t out)
{
	if (alg == TESSERA_HASH_ALG_BLAKE3_256) {
		tessera_blake3_256_keyed(key, data, len, out);
		return (TESSERA_OK);
	}
	return (tessera_hmac_sha256(key, 32, data, len, out));
}
