/*
 * Tests for tessera/crypt.h (docs/spec/tessera-encryption.md §4.2, §5.1, §6.1)
 * on the OpenSSL backend (src/crypt_openssl.c):
 *
 *   1. HMAC-SHA-256: RFC 4231 test cases 1, 2 and 6 (key longer than a block).
 *   2. HKDF-SHA-256: RFC 5869 test cases 1 and 3.
 *   3. HChaCha20: draft-irtf-cfrg-xchacha-03 §2.2.1.
 *   4. XAES-256-GCM: both c2sp.org/XAES-256-GCM vectors (L, K1, Kx and the
 *      ciphertext), and its accumulated randomized test (10 000 iterations).
 *   5. XChaCha20-Poly1305: draft-irtf-cfrg-xchacha-03 §A.3.1.
 *   6. Both suites: round trips 0..1100 bytes, and every tamper (ciphertext,
 *      tag, nonce, AAD, wrong key, wrong suite) is TESSERA_EAUTH with the
 *      output zeroed.
 *   7. Subkeys, key check and keyed ids: distinct labels, uuids and groups
 *      give distinct keys; keyed ids differ from plain hashes and by key.
 */
#include "tessera/crypt.h"
#include "tessera/error.h"
#include "tessera/hash.h"

#include <openssl/evp.h>

#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

void tessera_crypt_openssl_install(void);

static int failures = 0;
#define CHECK(cond) do {                                                 \
	if (!(cond)) {                                                   \
		fprintf(stderr, "FAIL %s:%d: %s\n",                      \
		    __FILE__, __LINE__, #cond);                          \
		failures++;                                              \
	}                                                                \
} while (0)

static size_t
unhex(uint8_t *o, const char *h)
{
	size_t n = 0;
	for (; h[0] && h[1]; h += 2)
		sscanf(h, "%2hhx", &o[n++]);
	return n;
}

static int
eqhex(const uint8_t *b, const char *h)
{
	uint8_t w[512];
	size_t n = unhex(w, h);
	return memcmp(b, w, n) == 0;
}

/* 1 */
static void
test_hmac(void)
{
	uint8_t key[131], out[32];

	memset(key, 0x0b, 20);
	tessera_hmac_sha256(key, 20, (const uint8_t *)"Hi There", 8, out);
	CHECK(eqhex(out, "b0344c61d8db38535ca8afceaf0bf12b881dc200c9833da726e9376c2e32cff7"));

	tessera_hmac_sha256((const uint8_t *)"Jefe", 4,
	    (const uint8_t *)"what do ya want for nothing?", 28, out);
	CHECK(eqhex(out, "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843"));

	memset(key, 0xaa, 131);
	const char *m = "Test Using Larger Than Block-Size Key - Hash Key First";
	tessera_hmac_sha256(key, 131, (const uint8_t *)m, strlen(m), out);
	CHECK(eqhex(out, "60e431591ee0b67f0d8a26aacbf5b77f8e0bc6213728c5140546040f0ee37f54"));
}

/* 2 */
static void
test_hkdf(void)
{
	uint8_t ikm[22], salt[13], info[10], okm[42];

	memset(ikm, 0x0b, 22);
	for (int i = 0; i < 13; i++) salt[i] = (uint8_t)i;
	for (int i = 0; i < 10; i++) info[i] = (uint8_t)(0xf0 + i);
	CHECK(tessera_hkdf_sha256(salt, 13, ikm, 22, info, 10, okm, 42) == TESSERA_OK);
	CHECK(eqhex(okm, "3cb25f25faacd57a90434f64d0362f2a2d2d0a90cf1a5a4c5db02d56ecc4c5bf"
	    "34007208d5b887185865"));

	/* RFC 5869 A.3: no salt, no info. */
	CHECK(tessera_hkdf_sha256(NULL, 0, ikm, 22, NULL, 0, okm, 42) == TESSERA_OK);
	CHECK(eqhex(okm, "8da4e775a563c18f715f802a063c5a31b8a11f5c5ee1879ec3454e5f3c738d2d"
	    "9d201395faa4b61a96c8"));
	CHECK(tessera_hkdf_sha256(NULL, 0, ikm, 22, NULL, 0, okm, 255 * 32 + 1) ==
	    TESSERA_EINVAL);
}

/* 3 */
static void
test_hchacha20(void)
{
	uint8_t key[32], in[16], out[32];

	unhex(key, "000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f");
	unhex(in, "000000090000004a0000000031415927");
	tessera_hchacha20(out, key, in);
	CHECK(eqhex(out, "82413b4227b27bfed30e42508a877d73a0f9e4d58a74a853c12ec41326d3ecdc"));
}

/* 4 */
static void
test_xaes_vectors(void)
{
	static const struct {
		uint8_t kb;
		const char *k1, *kx, *aad, *ct;
	} v[2] = {
		{ 0x01, "e531954aca063d5b8d9c47a47d4cc6f0",
		  "c8612c9ed53fe43e8e005b828a1631a0bbcb6ab2f46514ec4f439fcfd0fa969b",
		  "", "ce546ef63c9cc60765923609b33a9a1974e96e52daf2fcf7075e2271" },
		{ 0x03, "23810ec50edb99f374409466ed1f4b7b",
		  "e9c621d4cdd9b11b00a6427ad7e559aeedd66b3857646677748f8ca796cb3fd8",
		  "c2sp.org/XAES-256-GCM",
		  "986ec1832593df5443a179437fd083bf3fdb41abd740a21f71eb769d" },
	};
	const uint8_t *nonce = (const uint8_t *)"ABCDEFGHIJKLMNOPQRSTUVWX";

	for (int i = 0; i < 2; i++) {
		uint8_t key[32], kx[32], ct[12], tag[16], back[12];
		tessera_aead_key_t k;

		memset(key, v[i].kb, 32);
		CHECK(tessera_aead_key_init(&k, TESSERA_SUITE_XAES_256_GCM, key) ==
		    TESSERA_OK);
		CHECK(eqhex(k.k1, v[i].k1));
		CHECK(tessera_xaes_derive(&k, nonce, kx) == TESSERA_OK);
		CHECK(eqhex(kx, v[i].kx));
		CHECK(tessera_aead_seal_nonce(&k, nonce, (const uint8_t *)v[i].aad,
		    strlen(v[i].aad), (const uint8_t *)"XAES-256-GCM", 12, ct, tag) ==
		    TESSERA_OK);
		uint8_t both[28];
		memcpy(both, ct, 12);
		memcpy(both + 12, tag, 16);
		CHECK(eqhex(both, v[i].ct));
		CHECK(tessera_aead_open(&k, nonce, (const uint8_t *)v[i].aad,
		    strlen(v[i].aad), ct, 12, tag, back) == TESSERA_OK);
		CHECK(memcmp(back, "XAES-256-GCM", 12) == 0);
		tessera_aead_key_clear(&k);
	}
}

/*
 * c2sp's accumulated randomized test: a SHAKE-128 stream (empty input)
 * supplies key[32], nonce[24], plaintext<0..255>, aad<0..255> per
 * iteration (TLS presentation: one length byte, then the bytes); every
 * sealed output (ciphertext || tag) is absorbed by a second SHAKE-128.
 */
static void
test_xaes_accumulated(void)
{
	const int iters = 10000;
	const size_t per = 32 + 24 + 1 + 255 + 1 + 255;
	size_t cap = (size_t)iters * per, off = 0;
	uint8_t *rng = malloc(cap);
	EVP_MD_CTX *s = EVP_MD_CTX_new(), *h = EVP_MD_CTX_new();
	uint8_t out[32];

	CHECK(rng != NULL && s != NULL && h != NULL);
	EVP_DigestInit_ex(s, EVP_shake128(), NULL);
	EVP_DigestFinalXOF(s, rng, cap);
	CHECK(eqhex(rng, "7f9c2ba4e88f827d616045507605853e"));
	EVP_DigestInit_ex(h, EVP_shake128(), NULL);

	for (int i = 0; i < iters; i++) {
		const uint8_t *key = rng + off;            off += 32;
		const uint8_t *nonce = rng + off;          off += 24;
		size_t pl = rng[off++];
		const uint8_t *pt = rng + off;             off += pl;
		size_t al = rng[off++];
		const uint8_t *aad = rng + off;            off += al;
		uint8_t ct[256 + 16];
		tessera_aead_key_t k;

		if (tessera_aead_key_init(&k, TESSERA_SUITE_XAES_256_GCM, key) !=
		    TESSERA_OK ||
		    tessera_aead_seal_nonce(&k, nonce, aad, al, pt, pl, ct,
		    ct + pl) != TESSERA_OK) {
			CHECK(0);
			break;
		}
		EVP_DigestUpdate(h, ct, pl + 16);
	}
	EVP_DigestFinalXOF(h, out, 32);
	CHECK(eqhex(out, "e6b9edf2df6cec60c8cbd864e2211b597fb69a529160cd040d56c0c210081939"));
	EVP_MD_CTX_free(s);
	EVP_MD_CTX_free(h);
	free(rng);
}

/* 5 */
static void
test_xchacha_vector(void)
{
	const char *pt = "Ladies and Gentlemen of the class of '99: If I could "
	    "offer you only one tip for the future, sunscreen would be it.";
	size_t n = strlen(pt);
	uint8_t key[32], nonce[24], aad[12], ct[200], tag[16], back[200];
	tessera_aead_key_t k;

	unhex(key, "808182838485868788898a8b8c8d8e8f909192939495969798999a9b9c9d9e9f");
	unhex(nonce, "404142434445464748494a4b4c4d4e4f5051525354555657");
	unhex(aad, "50515253c0c1c2c3c4c5c6c7");
	CHECK(tessera_aead_key_init(&k, TESSERA_SUITE_XCHACHA20_POLY1305, key) ==
	    TESSERA_OK);
	CHECK(tessera_aead_seal_nonce(&k, nonce, aad, 12, (const uint8_t *)pt, n,
	    ct, tag) == TESSERA_OK);
	CHECK(eqhex(ct,
	    "bd6d179d3e83d43b9576579493c0e939572a1700252bfaccbed2902c21396cbb"
	    "731c7f1b0b4aa6440bf3a82f4eda7e39ae64c6708c54c216cb96b72e1213b452"
	    "2f8c9ba40db5d945b11b69b982c1bb9e3f3fac2bc369488f76b2383565d3fff9"
	    "21f9664c97637da9768812f615c68b13b52e"));
	CHECK(eqhex(tag, "c0875924c1c7987947deafd8780acf49"));
	CHECK(tessera_aead_open(&k, nonce, aad, 12, ct, n, tag, back) == TESSERA_OK);
	CHECK(memcmp(back, pt, n) == 0);
}

/* 6 */
static int
all_zero(const uint8_t *p, size_t n)
{
	uint8_t a = 0;
	for (size_t i = 0; i < n; i++) a |= p[i];
	return a == 0;
}

static void
test_round_trip_and_tamper(uint32_t suite)
{
	static const size_t lens[] = { 0, 1, 15, 16, 17, 63, 64, 65, 255, 4056, 1100 };
	uint8_t key[32], key2[32], aad[48];
	tessera_aead_key_t k, k2;

	for (int i = 0; i < 32; i++) { key[i] = (uint8_t)(i * 7 + 1); key2[i] = key[i]; }
	key2[31] ^= 0x80;
	memset(aad, 0x5c, sizeof(aad));
	CHECK(tessera_aead_key_init(&k, suite, key) == TESSERA_OK);
	CHECK(tessera_aead_key_init(&k2, suite, key2) == TESSERA_OK);

	for (size_t li = 0; li < sizeof(lens) / sizeof(lens[0]); li++) {
		size_t n = lens[li];
		uint8_t *pt = malloc(n + 1), *ct = malloc(n + 1), *back = malloc(n + 1);
		uint8_t nonce[24], nonce2[24], tag[16], bad[16];

		for (size_t j = 0; j < n; j++) pt[j] = (uint8_t)(j * 31 + 3);
		CHECK(tessera_aead_seal(&k, nonce, aad, sizeof(aad), pt, n, ct, tag) ==
		    TESSERA_OK);
		CHECK(tessera_aead_seal(&k, nonce2, aad, sizeof(aad), pt, n, ct + 0, bad) ==
		    TESSERA_OK);
		CHECK(memcmp(nonce, nonce2, 24) != 0);      /* fresh random nonces */
		/* reseal with the first nonce so ct/tag match again */
		CHECK(tessera_aead_seal_nonce(&k, nonce, aad, sizeof(aad), pt, n, ct, tag) ==
		    TESSERA_OK);
		if (n > 0)
			CHECK(memcmp(ct, pt, n) != 0 || n < 4);

		memset(back, 0xee, n + 1);
		CHECK(tessera_aead_open(&k, nonce, aad, sizeof(aad), ct, n, tag, back) ==
		    TESSERA_OK);
		CHECK(n == 0 || memcmp(back, pt, n) == 0);

		/* in place */
		if (n > 0) {
			uint8_t *buf = malloc(n);
			memcpy(buf, pt, n);
			CHECK(tessera_aead_seal_nonce(&k, nonce, aad, sizeof(aad), buf, n,
			    buf, bad) == TESSERA_OK);
			CHECK(memcmp(buf, ct, n) == 0 && memcmp(bad, tag, 16) == 0);
			CHECK(tessera_aead_open(&k, nonce, aad, sizeof(aad), buf, n, bad,
			    buf) == TESSERA_OK);
			CHECK(memcmp(buf, pt, n) == 0);
			free(buf);
		}

		/* tampering */
		if (n > 0) {
			ct[n / 2] ^= 0x01;
			memset(back, 0xee, n);
			CHECK(tessera_aead_open(&k, nonce, aad, sizeof(aad), ct, n, tag,
			    back) == TESSERA_EAUTH);
			CHECK(all_zero(back, n));
			ct[n / 2] ^= 0x01;
		}
		memcpy(bad, tag, 16); bad[0] ^= 0x01;
		CHECK(tessera_aead_open(&k, nonce, aad, sizeof(aad), ct, n, bad, back) ==
		    TESSERA_EAUTH);
		nonce[0] ^= 0x01;     /* first half: XAES subkey, HChaCha input */
		CHECK(tessera_aead_open(&k, nonce, aad, sizeof(aad), ct, n, tag, back) ==
		    TESSERA_EAUTH);
		nonce[0] ^= 0x01; nonce[23] ^= 0x01;   /* second half */
		CHECK(tessera_aead_open(&k, nonce, aad, sizeof(aad), ct, n, tag, back) ==
		    TESSERA_EAUTH);
		nonce[23] ^= 0x01;
		aad[47] ^= 0x01;
		CHECK(tessera_aead_open(&k, nonce, aad, sizeof(aad), ct, n, tag, back) ==
		    TESSERA_EAUTH);
		aad[47] ^= 0x01;
		CHECK(tessera_aead_open(&k, nonce, aad, sizeof(aad) - 1, ct, n, tag, back) ==
		    TESSERA_EAUTH);
		CHECK(tessera_aead_open(&k2, nonce, aad, sizeof(aad), ct, n, tag, back) ==
		    TESSERA_EAUTH);
		/* still opens after all that */
		CHECK(tessera_aead_open(&k, nonce, aad, sizeof(aad), ct, n, tag, back) ==
		    TESSERA_OK);
		free(pt); free(ct); free(back);
	}

	/* the other suite, same key bytes, cannot open it */
	{
		tessera_aead_key_t o;
		uint8_t nonce[24], tag[16], ct[32], back[32], pt[32] = { 1, 2, 3 };
		uint32_t other = suite == TESSERA_SUITE_XAES_256_GCM ?
		    TESSERA_SUITE_XCHACHA20_POLY1305 : TESSERA_SUITE_XAES_256_GCM;
		CHECK(tessera_aead_key_init(&o, other, key) == TESSERA_OK);
		CHECK(tessera_aead_seal(&k, nonce, NULL, 0, pt, 32, ct, tag) == TESSERA_OK);
		CHECK(tessera_aead_open(&o, nonce, NULL, 0, ct, 32, tag, back) ==
		    TESSERA_EAUTH);
		tessera_aead_key_clear(&o);
	}
	tessera_aead_key_clear(&k);
	tessera_aead_key_clear(&k2);
	CHECK(all_zero((const uint8_t *)&k, sizeof(k)));
}

static void
test_bad_args(void)
{
	tessera_aead_key_t k;
	uint8_t key[32] = { 0 };

	CHECK(tessera_aead_key_init(&k, 0, key) == TESSERA_EINCOMPAT);
	CHECK(tessera_aead_key_init(&k, 3, key) == TESSERA_EINCOMPAT);
	CHECK(tessera_aead_key_init(NULL, 1, key) == TESSERA_EINVAL);
}

/* 7 */
static void
test_subkeys_and_ids(void)
{
	uint8_t root[32], uuid[16], uuid2[16], a[32], b[32];

	for (int i = 0; i < 32; i++) root[i] = (uint8_t)(0xa0 + i);
	for (int i = 0; i < 16; i++) { uuid[i] = (uint8_t)i; uuid2[i] = (uint8_t)i; }
	uuid2[15] ^= 1;

	CHECK(tessera_subkey(root, TESSERA_KDF_META, uuid, 0, a) == TESSERA_OK);
	/* by definition: HKDF(salt, root, "meta" || uuid) */
	{
		uint8_t info[4 + 16], want[32];
		memcpy(info, "meta", 4);
		memcpy(info + 4, uuid, 16);
		tessera_hkdf_sha256((const uint8_t *)"tessera-encryption v1", 21,
		    root, 32, info, sizeof(info), want, 32);
		CHECK(memcmp(a, want, 32) == 0);
	}
	CHECK(tessera_subkey(root, TESSERA_KDF_BLOB, uuid, 0, b) == TESSERA_OK);
	CHECK(memcmp(a, b, 32) != 0);
	CHECK(tessera_subkey(root, TESSERA_KDF_META, uuid2, 0, b) == TESSERA_OK);
	CHECK(memcmp(a, b, 32) != 0);
	CHECK(tessera_subkey(root, TESSERA_KDF_META, uuid, 7, b) == TESSERA_OK);
	CHECK(memcmp(a, b, 32) != 0);
	CHECK(tessera_subkey(root, TESSERA_KDF_GID, uuid, 7, a) == TESSERA_OK);
	CHECK(tessera_subkey(root, TESSERA_KDF_GID, uuid, 8, b) == TESSERA_OK);
	CHECK(memcmp(a, b, 32) != 0);

	CHECK(tessera_key_check(root, uuid, 0, a) == TESSERA_OK);
	root[0] ^= 1;
	CHECK(tessera_key_check(root, uuid, 0, b) == TESSERA_OK);
	CHECK(memcmp(a, b, 32) != 0);

	const uint8_t data[] = "the same bytes";
	tessera_hash_t plain, k1, k2, s1;
	uint8_t key1[32], key2[32];
	memset(key1, 1, 32);
	memset(key2, 2, 32);
	tessera_blake3_256(data, sizeof(data), plain);
	CHECK(tessera_keyed_id(TESSERA_HASH_ALG_BLAKE3_256, key1, data, sizeof(data), k1) == TESSERA_OK);
	CHECK(tessera_keyed_id(TESSERA_HASH_ALG_BLAKE3_256, key2, data, sizeof(data), k2) == TESSERA_OK);
	CHECK(memcmp(plain, k1, 32) != 0 && memcmp(k1, k2, 32) != 0);
	CHECK(tessera_keyed_id(TESSERA_HASH_ALG_SHA256, key1, data, sizeof(data), s1) == TESSERA_OK);
	{
		uint8_t h[32];
		tessera_hmac_sha256(key1, 32, data, sizeof(data), h);
		CHECK(memcmp(s1, h, 32) == 0);
	}
	CHECK(memcmp(s1, k1, 32) != 0);
	/* BLAKE3 keyed mode, official test vector: input of length 0, key
	 * "whats the Elvish word for friend" (test_vectors.json). */
	{
		tessera_hash_t e;
		tessera_blake3_256_keyed((const uint8_t *)"whats the Elvish word for friend",
		    (const uint8_t *)"", 0, e);
		CHECK(eqhex(e, "92b2b75604ed3c761f9d6f62392c8a9227ad0ea3f09573e783f1498a4ed60d26"));
	}
}

int
main(void)
{
	printf("test_crypt: HMAC, HKDF, HChaCha20, XAES-256-GCM, XChaCha20-Poly1305, "
	    "subkeys, keyed ids\n");
	tessera_crypt_openssl_install();
	test_hmac();
	test_hkdf();
	test_hchacha20();
	test_xaes_vectors();
	test_xaes_accumulated();
	test_xchacha_vector();
	test_round_trip_and_tamper(TESSERA_SUITE_XAES_256_GCM);
	test_round_trip_and_tamper(TESSERA_SUITE_XCHACHA20_POLY1305);
	test_bad_args();
	test_subkeys_and_ids();
	if (failures > 0) {
		fprintf(stderr, "%d failure(s)\n", failures);
		return 1;
	}
	printf("ok\n");
	return 0;
}
