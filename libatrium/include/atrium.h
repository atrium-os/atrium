/* libatrium — the Insula platform library, C ABI.
 *
 * v0 surface: init / log / exit. Subsequent versions
 * add atrium_fresco_*, atrium_storage_*, atrium_net_*,
 * atrium_limen_*, atrium_keychain_*, ...
 *
 * Reference: docs/spec/insula.md §2.3.
 */

#ifndef ATRIUM_H
#define ATRIUM_H

#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

/* Log severity levels (syslog-shaped ordering). */
#define ATRIUM_LOG_TRACE   0u
#define ATRIUM_LOG_DEBUG   1u
#define ATRIUM_LOG_INFO    2u
#define ATRIUM_LOG_WARN    3u
#define ATRIUM_LOG_ERROR   4u

/* Status codes from atrium_init. */
#define ATRIUM_OK                         0
#define ATRIUM_ERR_SDK_VERSION           -1
#define ATRIUM_ERR_PLATFORM_UNREACHABLE  -2

/* Initialize the platform. Call once early in main. */
int32_t atrium_init(uint32_t sdk_major, uint32_t sdk_minor);

/* Emit a log line at the given level. msg is a NUL-
 * terminated UTF-8 string; NULL is tolerated. */
void atrium_log(uint32_t level, const char* msg);

/* Cleanly exit the process. */
void atrium_exit(int32_t code) __attribute__((noreturn));

/* -----------------------------------------------------------
 * Storage — access to the app's sandbox container.
 * -----------------------------------------------------------
 */

/* Storage open modes. */
#define ATRIUM_STORAGE_READ    0u
#define ATRIUM_STORAGE_WRITE   1u  /* truncate-on-open */
#define ATRIUM_STORAGE_APPEND  2u

/* Negative return codes from atrium_container_path /
 * atrium_storage_open. */
#define ATRIUM_ERR_NO_CONTAINER    -1
#define ATRIUM_ERR_BUF_TOO_SMALL   -2
#define ATRIUM_ERR_IO              -3
#define ATRIUM_ERR_INVALID_MODE    -4
#define ATRIUM_ERR_INVALID_PATH    -5

#include <stddef.h>

/* Write the absolute path of the app's container into `buf`
 * (NUL-terminated). Returns the path length (excluding NUL)
 * on success, or a negative ATRIUM_ERR_*. */
int32_t atrium_container_path(char* buf, size_t buf_len);

/* Open a file at container-relative `path` with the given
 * mode. Returns a normal file descriptor for use with libc
 * read/write/close, or a negative ATRIUM_ERR_*. */
int32_t atrium_storage_open(const char* path, uint32_t mode);

/* -----------------------------------------------------------
 * Keychain — per-(service, persona) ed25519 keypairs managed
 * by Vestibulum. Private keys never cross this boundary.
 * -----------------------------------------------------------
 */

#define ATRIUM_KEYCHAIN_PUBKEY_LEN  32
#define ATRIUM_KEYCHAIN_SIG_LEN     64

#define ATRIUM_ERR_NO_VESTIBULUM   -10
#define ATRIUM_ERR_VESTIBULUM_RPC  -11

/* Write the 32-byte ed25519 public key for `service` into
 * `out` (which must have room for ATRIUM_KEYCHAIN_PUBKEY_LEN).
 * Mints the keypair on first call. Returns
 * ATRIUM_KEYCHAIN_PUBKEY_LEN on success, negative on error. */
int32_t atrium_keychain_pubkey(
    const char* service,
    uint8_t* out, size_t out_len);

/* Sign `challenge_len` bytes at `challenge` under `service`'s
 * keypair; write the 64-byte ed25519 signature into `sig_out`.
 * Returns ATRIUM_KEYCHAIN_SIG_LEN on success, negative on
 * error. */
int32_t atrium_keychain_sign(
    const char* service,
    const uint8_t* challenge, size_t challenge_len,
    uint8_t* sig_out, size_t sig_out_len);

#ifdef __cplusplus
}
#endif

#endif /* ATRIUM_H */
