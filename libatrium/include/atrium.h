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

/* -----------------------------------------------------------
 * Network — outbound connections via the broker
 * (atrium-netd-macos). Returned fd is byte-proxied by the
 * broker to an upstream TCP socket.
 * -----------------------------------------------------------
 */

#define ATRIUM_NET_TCP   0u
#define ATRIUM_NET_UDP   1u  /* reserved; v0 broker does not yet implement */

#define ATRIUM_ERR_NO_NETD       -20
#define ATRIUM_ERR_NETD_DENIED   -21
#define ATRIUM_ERR_NETD_DNS      -22
#define ATRIUM_ERR_NETD_CONNECT  -23
#define ATRIUM_ERR_NETD_RPC      -24

/* Open an outbound connection to host:port via the broker.
 * Returns the OS file descriptor (a unix socket byte-proxied
 * to the upstream TCP connection by the daemon), suitable for
 * read(2) / write(2) / close(2). Negative on error. */
int32_t atrium_net_connect(const char* host, uint16_t port, uint32_t proto);

/* -----------------------------------------------------------
 * Notifications — POST to Praeco daemon.
 * -----------------------------------------------------------
 */

#define ATRIUM_NOTIFY_LOW       0u
#define ATRIUM_NOTIFY_NORMAL    1u
#define ATRIUM_NOTIFY_HIGH      2u

#define ATRIUM_ERR_NO_PRAECO    -30
#define ATRIUM_ERR_PRAECO_RPC   -31

/* Post a notification. Returns the assigned notification id
 * on success (positive int64_t), or a negative error code. */
int64_t atrium_notify_post(
    const char* title, const char* body, uint32_t urgency);

/* -----------------------------------------------------------
 * Tabellarius — push delivery (subscribe / unsubscribe / count).
 * Phase A surface; relay traffic + wake-on-push are Phase B.
 * -----------------------------------------------------------
 */

#define ATRIUM_TABELLARIUS_PUBKEY_LEN  32
#define ATRIUM_TABELLARIUS_KEY_ID_MAX  64

#define ATRIUM_ERR_NO_TABELLARIUS        -40
#define ATRIUM_ERR_TABELLARIUS_RPC       -41
#define ATRIUM_ERR_TABELLARIUS_UNKNOWN_KEY -42

/* Subscribe under `purpose` (e.g. "primary"). Writes the
 * caller-visible key_id (NUL-terminated, up to key_id_cap-1
 * chars) and the 32-byte pubkey to publish to the app's
 * backend. Returns the key_id length on success, negative
 * on error. */
int32_t atrium_tabellarius_subscribe(
    const char* purpose,
    char* key_id_out, size_t key_id_cap,
    uint8_t* pubkey_out);

/* Unsubscribe by key_id. Returns 0 on success,
 * ATRIUM_ERR_TABELLARIUS_UNKNOWN_KEY if no such sub. */
int32_t atrium_tabellarius_unsubscribe(const char* key_id);

/* How many subscriptions are active on this device.
 * Returns the count on success, negative on error. */
int32_t atrium_tabellarius_count(void);

/* Recommended minimum ciphertext buffer size. */
#define ATRIUM_TABELLARIUS_MAX_PUSH  65536

typedef struct {
    char     key_id[64];      /* NUL-terminated */
    uint64_t timestamp;
    uint32_t ciphertext_len;  /* true length, even if > cap */
} atrium_push_header_t;

/* Drain the next queued push. Returns 1 (push written),
 * 0 (queue empty), or negative on error. On 1, *hdr is
 * filled and up to ciphertext_cap blob bytes are copied
 * to ciphertext_out. */
int32_t atrium_tabellarius_get_push(
    atrium_push_header_t* hdr,
    uint8_t* ciphertext_out, size_t ciphertext_cap);

/* -----------------------------------------------------------
 * Window — open / destroy a top-level window via Fresco.
 * v0 surface: scene-graph emission for painting into the
 * window lands in subsequent slices.
 * -----------------------------------------------------------
 */

#define ATRIUM_ERR_NO_FRESCO    -50
#define ATRIUM_ERR_FRESCO_RPC   -51

/* Open a top-level window. Returns the assigned window_id
 * (positive) on success, negative on error. */
int32_t atrium_window_open(const char* title, uint32_t width, uint32_t height);

/* Destroy a previously-opened window. */
int32_t atrium_window_destroy(uint32_t window_id);

/* Single-call convenience: emit a frame containing one
 * RECT node into window_id and present. First-pixel API. */
int32_t atrium_window_fill_rect(
    uint32_t window_id,
    float x, float y, float w, float h,
    float r, float g, float b, float a);

/* Multi-node frame builder. Apps that paint more than
 * a single rect bracket their per-frame work between
 * frame_begin / frame_end calls. */
int32_t atrium_window_frame_begin(uint32_t window_id);
int32_t atrium_window_frame_rect(
    uint32_t node_id,
    float x, float y, float w, float h,
    float r, float g, float b, float a);
int32_t atrium_window_frame_path(
    uint32_t node_id,
    float cx, float cy, float length, float width, float angle,
    float r, float g, float b, float a);

/* Texture format selectors for atrium_window_upload_texture. */
#define ATRIUM_TEX_FORMAT_RGBA8_SRGB  0
#define ATRIUM_TEX_FORMAT_R8_UNORM    1

/* Upload pixel data to the scene server's CAS + bind to
 * `slot_id` for `window_id`. Returns 0 on success. */
int32_t atrium_window_upload_texture(
    uint32_t window_id, uint32_t slot_id,
    const uint8_t* bytes, size_t len,
    uint32_t width, uint32_t height, uint32_t format);

/* Emit a texture node referencing a previously-uploaded
 * slot. Must be called between frame_begin / frame_end. */
int32_t atrium_window_frame_texture(
    uint32_t node_id, uint32_t slot_id,
    float x, float y, float w, float h);

/* One pre-shaped glyph instance for atrium_window_frame_glyph_run. */
typedef struct {
    float    dx, dy;
    uint32_t atlas_u, atlas_v, atlas_w, atlas_h;
    float    bearing_x, bearing_y;
} atrium_glyph_t;

/* Emit a glyph-run node. atlas_slot_id must be a slot
 * uploaded with R8_UNORM (typical for glyph coverage
 * atlases). Apps provide pre-shaped glyphs; libatrium
 * does not depend on any shaping library. */
int32_t atrium_window_frame_glyph_run(
    uint32_t node_id,
    uint32_t atlas_slot_id, uint32_t atlas_width, uint32_t atlas_height,
    float x, float y,
    float r, float g, float b, float a,
    const atrium_glyph_t* glyphs, size_t n_glyphs);

int32_t atrium_window_frame_end(void);

/* Window event kinds (mirror fresco_protocol::control::EV_*). */
#define ATRIUM_EV_WINDOW_RESIZED         0x0580
#define ATRIUM_EV_WINDOW_FOCUS_CHANGED   0x0581
#define ATRIUM_EV_WINDOW_CLOSE_REQUESTED 0x0582

typedef struct {
    uint16_t kind;
    uint16_t _pad;
    uint32_t window_id;
    uint32_t arg1;        /* width / gained */
    uint32_t arg2;        /* height */
} atrium_window_event_t;

/* Non-blocking poll for the next window event.
 * Returns 1 if an event was written to *out, 0 if none,
 * negative on error. */
int32_t atrium_window_poll_event(atrium_window_event_t* out);

/* Drop the persistent Fresco connection (test helper /
 * shutdown). */
void atrium_window_disconnect(void);

#ifdef __cplusplus
}
#endif

#endif /* ATRIUM_H */
