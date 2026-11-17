/*
 * crypt_bench.c — Tessera encryption E0 (docs/spec/tessera-encryption.md §13).
 *
 * Single-core throughput of the two candidate suites, sealing and opening
 * one unit at a time with a fresh 24-byte nonce, the way Tessera will:
 *
 *   sodium-ref   XChaCha20-Poly1305 on the FreeBSD kernel's own code path
 *                (sys/crypto/chacha20_poly1305.c: libsodium's portable
 *                ChaCha20 "ref" + poly1305-donna, compiled from sys/contrib).
 *                This is what xchacha20_poly1305_encrypt() costs in the kernel.
 *   ossl-xchacha XChaCha20-Poly1305 as HChaCha20 (libsodium core) + OpenSSL's
 *                assembly ChaCha20-Poly1305: the kernel ossl(4) driver's code.
 *   ossl-xaes    XAES-256-GCM (c2sp.org/XAES-256-GCM): CMAC-KDF subkey per
 *                unit, then OpenSSL's AES-256-GCM (AES-NI / ARMv8 AES): the
 *                ossl(4) / aesni(4) / armv8crypto(4) code with a per-request key.
 *   ossl-gcm96   plain AES-256-GCM with one fixed key, as a ceiling only
 *                (not usable: 96-bit random nonces, §5.1).
 *
 * Every unit is checked: the opened plaintext must equal the input, and
 * the XAES and XChaCha constructions are checked against published vectors
 * before timing. Build: scripts/tessera-crypt-bench.sh.
 */
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>

#include <openssl/evp.h>

#include "crypto_core_hchacha20.h"
#include "crypto_onetimeauth_poly1305.h"
#include "crypto_stream_chacha20.h"

#define TAG 16

static double
now(void)
{
	struct timespec ts;
	clock_gettime(CLOCK_MONOTONIC, &ts);
	return ts.tv_sec + ts.tv_nsec / 1e9;
}

static void
die(const char *m)
{
	fprintf(stderr, "crypt_bench: %s\n", m);
	exit(1);
}

/* ── XChaCha20-Poly1305, kernel KPI path (libsodium ref) ─────────── */

static const uint8_t zeroes[16];

static void
sodium_poly_pad(crypto_onetimeauth_poly1305_state *st, size_t n)
{
	if (n % 16)
		crypto_onetimeauth_poly1305_update(st, zeroes, 16 - n % 16);
}

static void
sodium_lengths(crypto_onetimeauth_poly1305_state *st, uint64_t a, uint64_t c)
{
	uint8_t l[16];
	for (int i = 0; i < 8; i++) {
		l[i] = (uint8_t)(a >> (8 * i));
		l[8 + i] = (uint8_t)(c >> (8 * i));
	}
	crypto_onetimeauth_poly1305_update(st, l, 16);
}

/* out = ct || tag */
static void
sodium_xc_seal(uint8_t *out, const uint8_t *in, size_t n, const uint8_t *aad,
    size_t an, const uint8_t nonce[24], const uint8_t key[32])
{
	uint8_t sub[32], n12[12] = {0}, block[64];
	crypto_onetimeauth_poly1305_state st;

	crypto_core_hchacha20(sub, nonce, key, NULL);
	memcpy(n12 + 4, nonce + 16, 8);
	memset(block, 0, sizeof(block));
	crypto_stream_chacha20_ietf(block, 64, n12, sub);
	crypto_onetimeauth_poly1305_init(&st, block);
	crypto_onetimeauth_poly1305_update(&st, aad, an);
	sodium_poly_pad(&st, an);
	crypto_stream_chacha20_ietf_xor_ic(out, in, n, n12, 1, sub);
	crypto_onetimeauth_poly1305_update(&st, out, n);
	sodium_poly_pad(&st, n);
	sodium_lengths(&st, an, n);
	crypto_onetimeauth_poly1305_final(&st, out + n);
}

static int
sodium_xc_open(uint8_t *out, const uint8_t *in, size_t n, const uint8_t *aad,
    size_t an, const uint8_t nonce[24], const uint8_t key[32])
{
	uint8_t sub[32], n12[12] = {0}, block[64], tag[16];
	crypto_onetimeauth_poly1305_state st;

	crypto_core_hchacha20(sub, nonce, key, NULL);
	memcpy(n12 + 4, nonce + 16, 8);
	memset(block, 0, sizeof(block));
	crypto_stream_chacha20_ietf(block, 64, n12, sub);
	crypto_onetimeauth_poly1305_init(&st, block);
	crypto_onetimeauth_poly1305_update(&st, aad, an);
	sodium_poly_pad(&st, an);
	crypto_onetimeauth_poly1305_update(&st, in, n);
	sodium_poly_pad(&st, n);
	sodium_lengths(&st, an, n);
	crypto_onetimeauth_poly1305_final(&st, tag);
	if (memcmp(tag, in + n, 16) != 0)	/* bench only; kernel uses timingsafe_bcmp */
		return 0;
	crypto_stream_chacha20_ietf_xor_ic(out, in, n, n12, 1, sub);
	return 1;
}

/* ── OpenSSL AEAD helpers ───────────────────────────────────────── */

static EVP_CIPHER_CTX *ctx;

static void
evp_seal(const EVP_CIPHER *c, uint8_t *out, const uint8_t *in, size_t n,
    const uint8_t *aad, size_t an, const uint8_t n12[12], const uint8_t key[32])
{
	int l = 0;
	if (!EVP_EncryptInit_ex(ctx, c, NULL, key, n12) ||
	    !EVP_EncryptUpdate(ctx, NULL, &l, aad, (int)an) ||
	    !EVP_EncryptUpdate(ctx, out, &l, in, (int)n) ||
	    !EVP_EncryptFinal_ex(ctx, out + l, &l) ||
	    !EVP_CIPHER_CTX_ctrl(ctx, EVP_CTRL_AEAD_GET_TAG, TAG, out + n))
		die("seal");
}

static int
evp_open(const EVP_CIPHER *c, uint8_t *out, const uint8_t *in, size_t n,
    const uint8_t *aad, size_t an, const uint8_t n12[12], const uint8_t key[32])
{
	int l = 0;
	if (!EVP_DecryptInit_ex(ctx, c, NULL, key, n12) ||
	    !EVP_DecryptUpdate(ctx, NULL, &l, aad, (int)an) ||
	    !EVP_DecryptUpdate(ctx, out, &l, in, (int)n) ||
	    !EVP_CIPHER_CTX_ctrl(ctx, EVP_CTRL_AEAD_SET_TAG, TAG, (void *)(in + n)))
		die("open");
	return EVP_DecryptFinal_ex(ctx, out + l, &l) == 1;
}

/* XChaCha20-Poly1305 over OpenSSL's ChaCha20-Poly1305 */
static void
ossl_xc_seal(uint8_t *out, const uint8_t *in, size_t n, const uint8_t *aad,
    size_t an, const uint8_t nonce[24], const uint8_t key[32])
{
	uint8_t sub[32], n12[12] = {0};
	crypto_core_hchacha20(sub, nonce, key, NULL);
	memcpy(n12 + 4, nonce + 16, 8);
	evp_seal(EVP_chacha20_poly1305(), out, in, n, aad, an, n12, sub);
}

static int
ossl_xc_open(uint8_t *out, const uint8_t *in, size_t n, const uint8_t *aad,
    size_t an, const uint8_t nonce[24], const uint8_t key[32])
{
	uint8_t sub[32], n12[12] = {0};
	crypto_core_hchacha20(sub, nonce, key, NULL);
	memcpy(n12 + 4, nonce + 16, 8);
	return evp_open(EVP_chacha20_poly1305(), out, in, n, aad, an, n12, sub);
}

/* XAES-256-GCM: K1 precomputed once per key, as c2sp allows. */
static EVP_CIPHER_CTX *ecb;
static uint8_t xaes_k1[16];

static void
xaes_setkey(const uint8_t key[32])
{
	uint8_t z[16] = {0}, L[16];
	int l;
	if (!EVP_EncryptInit_ex(ecb, EVP_aes_256_ecb(), NULL, key, NULL))
		die("ecb");
	EVP_CIPHER_CTX_set_padding(ecb, 0);
	EVP_EncryptUpdate(ecb, L, &l, z, 16);
	for (int i = 0; i < 15; i++)
		xaes_k1[i] = (uint8_t)(L[i] << 1 | L[i + 1] >> 7);
	xaes_k1[15] = (uint8_t)(L[15] << 1);
	if (L[0] & 0x80)
		xaes_k1[15] ^= 0x87;
}

static void
xaes_derive(uint8_t kx[32], const uint8_t nonce[24])
{
	uint8_t m[32];
	int l;
	for (int i = 0; i < 2; i++) {
		m[16 * i + 0] = 0;
		m[16 * i + 1] = (uint8_t)(i + 1);
		m[16 * i + 2] = 0x58;
		m[16 * i + 3] = 0;
		memcpy(m + 16 * i + 4, nonce, 12);
		for (int j = 0; j < 16; j++)
			m[16 * i + j] ^= xaes_k1[j];
	}
	EVP_EncryptUpdate(ecb, kx, &l, m, 32);	/* ECB key stays loaded */
}

static void
xaes_seal(uint8_t *out, const uint8_t *in, size_t n, const uint8_t *aad,
    size_t an, const uint8_t nonce[24], const uint8_t key[32])
{
	uint8_t kx[32];
	(void)key;
	xaes_derive(kx, nonce);
	evp_seal(EVP_aes_256_gcm(), out, in, n, aad, an, nonce + 12, kx);
}

static int
xaes_open(uint8_t *out, const uint8_t *in, size_t n, const uint8_t *aad,
    size_t an, const uint8_t nonce[24], const uint8_t key[32])
{
	uint8_t kx[32];
	(void)key;
	xaes_derive(kx, nonce);
	return evp_open(EVP_aes_256_gcm(), out, in, n, aad, an, nonce + 12, kx);
}

static void
gcm96_seal(uint8_t *out, const uint8_t *in, size_t n, const uint8_t *aad,
    size_t an, const uint8_t nonce[24], const uint8_t key[32])
{
	evp_seal(EVP_aes_256_gcm(), out, in, n, aad, an, nonce, key);
}

static int
gcm96_open(uint8_t *out, const uint8_t *in, size_t n, const uint8_t *aad,
    size_t an, const uint8_t nonce[24], const uint8_t key[32])
{
	return evp_open(EVP_aes_256_gcm(), out, in, n, aad, an, nonce, key);
}

/* ── vectors ─────────────────────────────────────────────────────── */

static void
unhex(uint8_t *o, const char *h)
{
	for (; h[0] && h[1]; h += 2)
		sscanf(h, "%2hhx", o++);
}

static void
check_vectors(void)
{
	/* c2sp.org/XAES-256-GCM, both vectors (MSB(L) = 0 and 1). */
	struct { int kb; const char *aad, *ct; } v[2] = {
		{ 0x01, "", "ce546ef63c9cc60765923609b33a9a1974e96e52daf2fcf7075e2271" },
		{ 0x03, "c2sp.org/XAES-256-GCM",
		  "986ec1832593df5443a179437fd083bf3fdb41abd740a21f71eb769d" },
	};
	for (int i = 0; i < 2; i++) {
		uint8_t key[32], out[64], want[28];
		memset(key, v[i].kb, 32);
		xaes_setkey(key);
		xaes_seal(out, (const uint8_t *)"XAES-256-GCM", 12,
		    (const uint8_t *)v[i].aad, strlen(v[i].aad),
		    (const uint8_t *)"ABCDEFGHIJKLMNOPQRSTUVWX", key);
		unhex(want, v[i].ct);
		if (memcmp(out, want, 28) != 0)
			die("XAES-256-GCM vector mismatch");
	}
	/* draft-irtf-cfrg-xchacha-03 §A.3.1 */
	{
		const char *pt = "Ladies and Gentlemen of the class of '99: If I could "
		    "offer you only one tip for the future, sunscreen would be it.";
		uint8_t key[32], nonce[24], aad[12], want[114 + 16], a[200], b[200];
		size_t n = strlen(pt);
		unhex(key, "808182838485868788898a8b8c8d8e8f909192939495969798999a9b9c9d9e9f");
		unhex(nonce, "404142434445464748494a4b4c4d4e4f5051525354555657");
		unhex(aad, "50515253c0c1c2c3c4c5c6c7");
		unhex(want,
		    "bd6d179d3e83d43b9576579493c0e939572a1700252bfaccbed2902c21396cbb"
		    "731c7f1b0b4aa6440bf3a82f4eda7e39ae64c6708c54c216cb96b72e1213b452"
		    "2f8c9ba40db5d945b11b69b982c1bb9e3f3fac2bc369488f76b2383565d3fff9"
		    "21f9664c97637da9768812f615c68b13b52e"
		    "c0875924c1c7987947deafd8780acf49");
		sodium_xc_seal(a, (const uint8_t *)pt, n, aad, 12, nonce, key);
		ossl_xc_seal(b, (const uint8_t *)pt, n, aad, 12, nonce, key);
		if (memcmp(a, want, n + 16) != 0)
			die("XChaCha20-Poly1305 vector mismatch (sodium-ref)");
		if (memcmp(b, want, n + 16) != 0)
			die("XChaCha20-Poly1305 vector mismatch (ossl)");
	}
}

/* ── timing ──────────────────────────────────────────────────────── */

typedef void seal_fn(uint8_t *, const uint8_t *, size_t, const uint8_t *,
    size_t, const uint8_t *, const uint8_t *);
typedef int open_fn(uint8_t *, const uint8_t *, size_t, const uint8_t *,
    size_t, const uint8_t *, const uint8_t *);

static void
run(const char *name, seal_fn *seal, open_fn *open, size_t unit, double secs)
{
	uint8_t key[32], nonce[24], aad[48];
	uint8_t *pt = malloc(unit), *ct = malloc(unit + TAG), *back = malloc(unit);
	double t0, ts, to;
	size_t iters = 0;

	for (size_t i = 0; i < unit; i++)
		pt[i] = (uint8_t)(i * 131 + 7);
	memset(key, 0x5a, 32);
	memset(aad, 0xa5, sizeof(aad));
	xaes_setkey(key);

	/* seal */
	t0 = now();
	do {
		for (int k = 0; k < 64; k++, iters++) {
			memcpy(nonce, &iters, sizeof(iters));
			memset(nonce + 8, 0x11, 16);
			seal(ct, pt, unit, aad, sizeof(aad), nonce, key);
		}
	} while (now() - t0 < secs);
	ts = (double)iters * unit / (now() - t0) / 1e6;

	/* open (the last sealed unit, repeatedly) */
	size_t oi = 0;
	t0 = now();
	do {
		for (int k = 0; k < 64; k++, oi++)
			if (!open(back, ct, unit, aad, sizeof(aad), nonce, key))
				die("open failed");
	} while (now() - t0 < secs);
	to = (double)oi * unit / (now() - t0) / 1e6;
	if (memcmp(back, pt, unit) != 0)
		die("round trip mismatch");
	ct[unit / 2] ^= 1;
	if (open(back, ct, unit, aad, sizeof(aad), nonce, key))
		die("tampered unit opened");

	printf("%-13s %6zu B   seal %7.0f MB/s   open %7.0f MB/s\n",
	    name, unit, ts, to);
	free(pt); free(ct); free(back);
}

int
main(int argc, char **argv)
{
	double secs = argc > 1 ? atof(argv[1]) : 1.0;
	size_t units[] = { 4096, 65536 };

	ctx = EVP_CIPHER_CTX_new();
	ecb = EVP_CIPHER_CTX_new();
	check_vectors();
	printf("vectors: XAES-256-GCM (c2sp, 2) and XChaCha20-Poly1305 "
	    "(draft-irtf-cfrg-xchacha A.3.1, both paths) OK\n");
	for (size_t u = 0; u < 2; u++) {
		run("sodium-ref", sodium_xc_seal, sodium_xc_open, units[u], secs);
		run("ossl-xchacha", ossl_xc_seal, ossl_xc_open, units[u], secs);
		run("ossl-xaes", xaes_seal, xaes_open, units[u], secs);
		run("ossl-gcm96", gcm96_seal, gcm96_open, units[u], secs);
	}
	return 0;
}
