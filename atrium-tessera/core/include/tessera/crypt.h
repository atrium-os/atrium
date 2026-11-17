/*
 * tessera/crypt.h — at-rest encryption primitives (docs/spec/tessera-encryption.md).
 *
 * What lives here, in portable C shared by the kernel module and the tools:
 *
 *   - the two AEAD suites with 192-bit random nonces (§5.1):
 *       suite 1  XAES-256-GCM        (c2sp.org/XAES-256-GCM)
 *       suite 2  XChaCha20-Poly1305  (draft-irtf-cfrg-xchacha)
 *     built on a *backend* that supplies AES-256 block encryption and the
 *     two 96-bit-nonce AEADs (AES-256-GCM, ChaCha20-Poly1305). The kernel's
 *     backend is OCF on ossl(4); the tools' backend is OpenSSL's libcrypto
 *     (crypt_openssl.c). This file never implements AES, GCM, ChaCha20 or
 *     Poly1305 itself;
 *   - HChaCha20 (the XChaCha subkey step: 20 ChaCha rounds, no tables);
 *   - HMAC-SHA-256 and HKDF-SHA-256 (RFC 2104 / RFC 5869) over the core's
 *     SHA-256, and the spec's labelled subkeys (§4.2);
 *   - keyed content ids (§6.1): keyed BLAKE3 or HMAC-SHA-256 by hash_alg.
 *
 * Nothing here touches the disk format; the sealed format (§5.2–§5.5) is
 * built on these calls. Every function either succeeds or returns a
 * tessera_errno_t; open() returns TESSERA_EAUTH, never partial plaintext.
 */
#ifndef TESSERA_CRYPT_H_
#define TESSERA_CRYPT_H_

#ifdef _KERNEL
#  include <sys/types.h>
#else
#  include <stdint.h>
#  include <stddef.h>
#endif
#include "tessera/format.h"

#ifdef __cplusplus
extern "C" {
#endif

#define TESSERA_KEY_SIZE        32u
#define TESSERA_NONCE_SIZE      24u   /* every sealed unit, both suites */
#define TESSERA_TAG_SIZE        16u
#define TESSERA_SEAL_OVERHEAD   (TESSERA_NONCE_SIZE + TESSERA_TAG_SIZE)

/* Suite byte (superblock reserved_e0, key-group records). 0 = not sealed. */
#define TESSERA_SUITE_XAES_256_GCM        1u
#define TESSERA_SUITE_XCHACHA20_POLY1305  2u

/* ── backend ───────────────────────────────────────────────────────── */

/* The 96-bit-nonce AEADs a backend provides. */
#define TESSERA_AEAD96_AES_256_GCM        1u
#define TESSERA_AEAD96_CHACHA20_POLY1305  2u

typedef struct tessera_crypt_backend {
	/* AES-256 encryption of nblocks independent 16-byte blocks (ECB).
	 * Used only for XAES's key derivation, a few blocks per unit. Must be
	 * constant-time (AES instructions, or a bitsliced/vector AES). */
	int (*aes256_ecb)(const uint8_t key[32], const uint8_t *in,
	    uint8_t *out, size_t nblocks);
	/* Seal: ct (len bytes) and tag. ct may equal pt. */
	int (*aead96_seal)(uint32_t alg, const uint8_t key[32],
	    const uint8_t nonce[12], const uint8_t *aad, size_t aad_len,
	    const uint8_t *pt, size_t len, uint8_t *ct, uint8_t tag[16]);
	/* Open: TESSERA_OK and pt, or TESSERA_EAUTH and pt untouched or zeroed.
	 * pt may equal ct. */
	int (*aead96_open)(uint32_t alg, const uint8_t key[32],
	    const uint8_t nonce[12], const uint8_t *aad, size_t aad_len,
	    const uint8_t *ct, size_t len, const uint8_t tag[16], uint8_t *pt);
	/* Fill buf with len bytes from a CSPRNG (kernel: arc4random_buf). */
	void (*random)(uint8_t *buf, size_t len);
} tessera_crypt_backend_t;

/* Install the backend (once, at module load / tool start). Every sealing
 * call returns TESSERA_ENOTIMPL until one is installed. */
void tessera_crypt_set_backend(const tessera_crypt_backend_t *be);
const tessera_crypt_backend_t *tessera_crypt_backend(void);

/* ── keys and sealing ──────────────────────────────────────────────── */

/* A loaded AEAD key: the 32 key bytes, the suite, and per-key precomputed
 * state (XAES's K1). Zeroed by tessera_aead_key_clear. */
typedef struct tessera_aead_key {
	uint8_t  key[32];
	uint8_t  k1[16];        /* XAES: CMAC subkey K1 of key */
	uint32_t suite;
} tessera_aead_key_t;

int  tessera_aead_key_init(tessera_aead_key_t *k, uint32_t suite,
         const uint8_t key[32]);
void tessera_aead_key_clear(tessera_aead_key_t *k);

/* Seal with a caller-chosen nonce (tests, vectors). ct may equal pt. */
int tessera_aead_seal_nonce(const tessera_aead_key_t *k,
        const uint8_t nonce[24], const uint8_t *aad, size_t aad_len,
        const uint8_t *pt, size_t len, uint8_t *ct, uint8_t tag[16]);

/* Seal with a fresh random nonce, written to nonce_out. */
int tessera_aead_seal(const tessera_aead_key_t *k, uint8_t nonce_out[24],
        const uint8_t *aad, size_t aad_len, const uint8_t *pt, size_t len,
        uint8_t *ct, uint8_t tag[16]);

/* Open: TESSERA_OK, or TESSERA_EAUTH with pt zeroed. pt may equal ct. */
int tessera_aead_open(const tessera_aead_key_t *k, const uint8_t nonce[24],
        const uint8_t *aad, size_t aad_len, const uint8_t *ct, size_t len,
        const uint8_t tag[16], uint8_t *pt);

/* XAES-256-GCM key derivation alone (exposed for the c2sp vectors). */
int tessera_xaes_derive(const tessera_aead_key_t *k, const uint8_t nonce[24],
        uint8_t kx[32]);

/* HChaCha20 (draft-irtf-cfrg-xchacha §2.2). */
void tessera_hchacha20(uint8_t out[32], const uint8_t key[32],
         const uint8_t in[16]);

/* ── MAC, KDF, keyed ids ───────────────────────────────────────────── */

/* HMAC-SHA-256: TESSERA_OK, or TESSERA_ENOMEM (hash state allocation). */
int tessera_hmac_sha256(const uint8_t *key, size_t key_len,
         const uint8_t *msg, size_t len, uint8_t out[32]);
/* HMAC over the concatenation of nparts parts (0-length parts skipped). */
int tessera_hmac_sha256_v(const uint8_t *key, size_t key_len,
         const uint8_t *const parts[], const size_t lens[], int nparts,
         uint8_t out[32]);

/* RFC 5869. okm_len <= 255 * 32. */
int tessera_hkdf_sha256(const uint8_t *salt, size_t salt_len,
        const uint8_t *ikm, size_t ikm_len, const uint8_t *info,
        size_t info_len, uint8_t *okm, size_t okm_len);

/* Labelled subkey (spec §4.2):
 *   HKDF-SHA-256(salt = "tessera-encryption v1", ikm = root key,
 *                info = label || volume_uuid[16] [|| group_id le64])
 * group_id = 0 means "no group" and is left out of info. */
int tessera_subkey(const uint8_t root[32], const char *label,
        const uint8_t volume_uuid[16], uint64_t group_id, uint8_t out[32]);

/* Spec §4.2 labels. */
#define TESSERA_KDF_META    "meta"
#define TESSERA_KDF_BLOB    "blob"
#define TESSERA_KDF_ID      "id"
#define TESSERA_KDF_GMETA   "gmeta"
#define TESSERA_KDF_GBLOB   "gblob"
#define TESSERA_KDF_GID     "gid"
#define TESSERA_KDF_NAME    "name"
#define TESSERA_KDF_CDC     "cdc"
#define TESSERA_KDF_CHECK   "check"

/* check = HMAC(K_check, "tessera key check") (§4.2). */
int tessera_key_check(const uint8_t root[32], const uint8_t volume_uuid[16],
        uint64_t group_id, uint8_t out[32]);

/* Keyed content id (§6.1): keyed BLAKE3 when alg is BLAKE3, else
 * HMAC-SHA-256, under `key` (K_id or K_gid[d]). */
int tessera_keyed_id(uint32_t alg, const uint8_t key[32],
         const uint8_t *data, size_t len, tessera_hash_t out);

/* Keyed BLAKE3-256 (BLAKE3's own keyed mode). */
void tessera_blake3_256_keyed(const uint8_t key[32], const uint8_t *data,
         size_t len, tessera_hash_t out);

/* Zero memory in a way the compiler may not elide. */
void tessera_wipe(void *p, size_t n);

#ifdef __cplusplus
}
#endif

#endif /* TESSERA_CRYPT_H_ */
