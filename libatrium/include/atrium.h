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

#ifdef __cplusplus
}
#endif

#endif /* ATRIUM_H */
