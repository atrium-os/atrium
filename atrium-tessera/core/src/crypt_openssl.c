/*
 * crypt_openssl.c — userspace backend for tessera/crypt.h over OpenSSL's
 * libcrypto (FreeBSD base, Linux libssl-dev, Homebrew openssl@3).
 *
 * Not part of libtessera_core (the kernel installs an OCF backend, the
 * loader none): tools and tests compile this file beside the library and
 * call tessera_crypt_openssl_install() once. libcrypto's AES-GCM and
 * ChaCha20-Poly1305 are the same assembly the kernel's ossl(4) driver
 * carries (docs/spec/tessera-encryption.md §5.1, E0).
 */
#include <stdint.h>
#include <stdlib.h>
#include <string.h>

#include <openssl/evp.h>
#include <openssl/rand.h>

#include "tessera/crypt.h"
#include "tessera/error.h"

static int
ossl_aes256_ecb(const uint8_t key[32], const uint8_t *in, uint8_t *out,
    size_t nblocks)
{
	EVP_CIPHER_CTX *c = EVP_CIPHER_CTX_new();
	int l = 0, ok;

	if (c == NULL)
		return (TESSERA_ENOMEM);
	ok = EVP_EncryptInit_ex(c, EVP_aes_256_ecb(), NULL, key, NULL) &&
	    EVP_CIPHER_CTX_set_padding(c, 0) &&
	    EVP_EncryptUpdate(c, out, &l, in, (int)(nblocks * 16)) &&
	    (size_t)l == nblocks * 16;
	EVP_CIPHER_CTX_free(c);
	return (ok ? TESSERA_OK : TESSERA_EIO);
}

static const EVP_CIPHER *
aead_cipher(uint32_t alg)
{
	switch (alg) {
	case TESSERA_AEAD96_AES_256_GCM:       return (EVP_aes_256_gcm());
	case TESSERA_AEAD96_CHACHA20_POLY1305: return (EVP_chacha20_poly1305());
	default:                               return (NULL);
	}
}

static int
ossl_seal(uint32_t alg, const uint8_t key[32], const uint8_t nonce[12],
    const uint8_t *aad, size_t aad_len, const uint8_t *pt, size_t len,
    uint8_t *ct, uint8_t tag[16])
{
	const EVP_CIPHER *cipher = aead_cipher(alg);
	EVP_CIPHER_CTX *c;
	int l = 0, ok;

	if (cipher == NULL)
		return (TESSERA_EINCOMPAT);
	if (len > INT32_MAX || aad_len > INT32_MAX)
		return (TESSERA_ETOOBIG);
	if ((c = EVP_CIPHER_CTX_new()) == NULL)
		return (TESSERA_ENOMEM);
	ok = EVP_EncryptInit_ex(c, cipher, NULL, key, nonce) &&
	    (aad_len == 0 || EVP_EncryptUpdate(c, NULL, &l, aad, (int)aad_len)) &&
	    (len == 0 || EVP_EncryptUpdate(c, ct, &l, pt, (int)len)) &&
	    EVP_EncryptFinal_ex(c, ct + (len == 0 ? 0 : (size_t)l), &l) &&
	    EVP_CIPHER_CTX_ctrl(c, EVP_CTRL_AEAD_GET_TAG, 16, tag);
	EVP_CIPHER_CTX_free(c);
	return (ok ? TESSERA_OK : TESSERA_EIO);
}

static int
ossl_open(uint32_t alg, const uint8_t key[32], const uint8_t nonce[12],
    const uint8_t *aad, size_t aad_len, const uint8_t *ct, size_t len,
    const uint8_t tag[16], uint8_t *pt)
{
	const EVP_CIPHER *cipher = aead_cipher(alg);
	EVP_CIPHER_CTX *c;
	uint8_t t[16];
	int l = 0, ok;

	if (cipher == NULL)
		return (TESSERA_EINCOMPAT);
	if (len > INT32_MAX || aad_len > INT32_MAX)
		return (TESSERA_ETOOBIG);
	if ((c = EVP_CIPHER_CTX_new()) == NULL)
		return (TESSERA_ENOMEM);
	memcpy(t, tag, 16);
	ok = EVP_DecryptInit_ex(c, cipher, NULL, key, nonce) &&
	    (aad_len == 0 || EVP_DecryptUpdate(c, NULL, &l, aad, (int)aad_len)) &&
	    (len == 0 || EVP_DecryptUpdate(c, pt, &l, ct, (int)len)) &&
	    EVP_CIPHER_CTX_ctrl(c, EVP_CTRL_AEAD_SET_TAG, 16, t) &&
	    EVP_DecryptFinal_ex(c, pt + (len == 0 ? 0 : (size_t)l), &l) == 1;
	EVP_CIPHER_CTX_free(c);
	if (!ok) {
		if (len > 0)
			tessera_wipe(pt, len);
		return (TESSERA_EAUTH);
	}
	return (TESSERA_OK);
}

static void
ossl_random(uint8_t *buf, size_t len)
{
	if (RAND_bytes(buf, (int)len) != 1)
		abort();	/* never seal with a predictable nonce */
}

static const tessera_crypt_backend_t ossl_backend = {
	.aes256_ecb  = ossl_aes256_ecb,
	.aead96_seal = ossl_seal,
	.aead96_open = ossl_open,
	.random      = ossl_random,
};

void tessera_crypt_openssl_install(void);

void
tessera_crypt_openssl_install(void)
{
	tessera_crypt_set_backend(&ossl_backend);
}
