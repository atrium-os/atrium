/*
 * libfresco — userspace C library for the Fresco scenegraph protocol
 * on FreeBSD. Wraps /dev/fresco0 (provided by fresco.ko) and speaks
 * the wire protocol shared with the reference server (`fresco-server`).
 *
 * This is the public API. Phase 1 covers transport + raw command/
 * completion. Higher-level slot graph, CAS, and event helpers land
 * in later phases.
 *
 * Thread safety: a fresco_t handle is NOT thread-safe. Use one per
 * thread, or guard with your own mutex.
 */

#ifndef _FRESCO_H_
#define _FRESCO_H_

#include <stddef.h>
#include <stdint.h>
#include <stdbool.h>

#ifdef __cplusplus
extern "C" {
#endif

/* ---------------------------------------------------------------- */
/* Connection lifecycle                                              */
/* ---------------------------------------------------------------- */

typedef struct fresco fresco_t;

/* SHA-256 hash; opaque to callers. CAS blob identifier. */
typedef uint8_t fresco_hash_t[32];

/* Open a connection to the Fresco transport.
 *   dev_path: device path, or NULL for the default "/dev/fresco0".
 * Returns NULL on error and sets errno. */
fresco_t *fresco_open(const char *dev_path);

/* Close and release. Safe on NULL. */
void      fresco_close(fresco_t *f);

/* The cdev fd, for caller-owned kqueue integration.
 * Register EVFILT_READ on it; the kernel wakes when host advances
 * the completion or input ring write pointer. */
int       fresco_fd(const fresco_t *f);

/* ---------------------------------------------------------------- */
/* Display + system info (from shmem control regs)                   */
/* ---------------------------------------------------------------- */

typedef struct {
    uint32_t width;
    uint32_t height;
    uint32_t refresh_hz;
} fresco_display_t;

int  fresco_get_display(fresco_t *f, fresco_display_t *out);

/* Hash of the system font blob the host has published into the CAS,
 * if any. Writes 32 bytes. Returns 1 if non-zero hash present, 0
 * otherwise. */
int  fresco_get_system_font(fresco_t *f, fresco_hash_t out);

/* Convenience: kqueue wait on the fd. ms < 0 = forever.
 * Returns 1 on wake, 0 on timeout, -1 on error (errno set). */
int  fresco_wait(fresco_t *f, int ms);

/* ---------------------------------------------------------------- */
/* CAS — content-addressed blob store                                */
/* ---------------------------------------------------------------- */

/*
 * Upload a blob. Computes SHA-256 locally with libmd, deduplicates
 * against an in-process cache of previously-uploaded hashes, and
 * sends the BEGIN/DATA/FINISH chain only on first upload. On success,
 * writes the 32-byte hash to `out`.
 *
 * Returns 0 on success, -1 on error (errno set).
 */
int  fresco_cas_put(fresco_t *f, const void *data, size_t len, fresco_hash_t out);

/*
 * Ask the server whether a blob with `hash` exists in the host CAS.
 * Useful for verifying persistence or polling for a known blob.
 * Returns 1 if present, 0 if absent, -1 on error.
 */
int  fresco_cas_query(fresco_t *f, const fresco_hash_t hash);

/* ---------------------------------------------------------------- */
/* Typed blob builders (write into caller buffer, return blob length) */
/* ---------------------------------------------------------------- */

/*
 * Solid color material blob (NODE_MATERIAL_SOLID, 0x0200).
 * Color components in [0..1]. Output is 16 bytes (8 header + 8 payload —
 * server requires payload ≥ 8 bytes, only first 4 read as RGBA).
 */
size_t fresco_blob_material_solid(uint8_t *out, float r, float g, float b, float a);

/*
 * Raw vertex/index blob (NODE_VERTEX_DATA 0x0110, NODE_INDEX_DATA 0x0111).
 * `out` must point to ≥ (8 + len) bytes. Returns total blob length. */
size_t fresco_blob_vertex_data(uint8_t *out, const float *verts, size_t verts_count_floats);
size_t fresco_blob_index_data (uint8_t *out, const uint16_t *idx, size_t idx_count);

/*
 * NODE_MESH (0x0100) header pointing at vertex+index blob hashes.
 * Output is 80 bytes (8 header + 72 payload).
 *   vertex_count: number of vertices (NOT floats)
 *   index_count:  number of indices (0 for non-indexed)
 *   flags:        vertex format bits (bit 0x0100 = POSITION f32x3 — required)
 */
size_t fresco_blob_mesh(uint8_t *out,
                        uint32_t vertex_count, uint32_t index_count,
                        uint32_t vertex_format_flags,
                        const fresco_hash_t vertex_hash,
                        const fresco_hash_t index_hash);

/*
 * NODE_RENDERABLE (0x0005) header binding mesh + material.
 * Output is 72 bytes (8 header + 64 payload). */
size_t fresco_blob_renderable(uint8_t *out,
                              const fresco_hash_t mesh_hash,
                              const fresco_hash_t material_hash);

/*
 * NODE_TRANSFORM (0x0004): a 4x4 matrix as 16 floats.
 * Used both for slot transforms (when referenced by hash) and for
 * camera placement (camera-to-world matrix; renderer inverts it to
 * get the view matrix). Output is 72 bytes (8 header + 64 payload). */
size_t fresco_blob_transform(uint8_t *out, const float matrix[16]);

/*
 * NODE_CAMERA (0x0003) — perspective camera with a view transform.
 *   fov_y       vertical field of view in radians
 *   aspect      display width / height
 *   near, far   clip planes
 *   view_xform  hash of a NODE_TRANSFORM blob giving camera-to-world
 * Output is 56 bytes (8 header + 48 payload). */
size_t fresco_blob_camera(uint8_t *out,
                          float fov_y, float aspect,
                          float near_plane, float far_plane,
                          const fresco_hash_t view_xform);

/* Set the scene camera (CMD_SET_CAMERA, 0x0101). FRAME_END uses this
 * if no slot-table camera is active. */
int  fresco_set_camera(fresco_t *f, const fresco_hash_t camera_hash);

/* ---------------------------------------------------------------- */
/* Textures (NODE_TEXTURE 0x0400 + NODE_PIXEL_DATA 0x0401)           */
/* ---------------------------------------------------------------- */

/* Build a NODE_PIXEL_DATA blob from raw RGBA8 bytes.
 * `out` must point to ≥ (8 + len) bytes. Returns total blob length. */
size_t fresco_blob_pixel_data(uint8_t *out, const void *rgba8, size_t len);

/* Build a NODE_TEXTURE header blob referencing a pixel-data hash.
 * Output is 56 bytes (8 header + 48 payload). format: 0=RGBA8.
 * filter: 0=linear, 1=nearest. wrap: 0=clamp, 1=repeat. */
size_t fresco_blob_texture(uint8_t *out,
                           uint32_t width, uint32_t height,
                           uint8_t format, uint8_t filter, uint8_t wrap,
                           const fresco_hash_t pixel_data_hash);

/* NODE_MATERIAL_TEXTURED (0x0203): a material that samples albedo
 * from a NODE_TEXTURE blob. tint_rgba is multiplied with the sampled
 * color (use 0xffffffff for no tint). 44 bytes. */
size_t fresco_blob_material_textured(uint8_t *out,
                                     const fresco_hash_t texture_hash,
                                     uint32_t tint_rgba);

/* High-level helper: upload pixel data + texture header in two
 * cas_put calls and return the texture-header hash. The pixel hash
 * is dedup'd separately, so reusing the same image elsewhere skips
 * the bulk re-upload automatically. */
int fresco_cas_put_texture(fresco_t *f,
                           uint32_t width, uint32_t height,
                           const void *rgba8, size_t bytes,
                           fresco_hash_t out);

/* Wire-protocol blob type IDs */
#define FRESCO_NODE_SCENE_ROOT          0x0001
#define FRESCO_NODE_SCENE_NODE          0x0002
#define FRESCO_NODE_CAMERA              0x0003
#define FRESCO_NODE_TRANSFORM           0x0004
#define FRESCO_NODE_RENDERABLE          0x0005
#define FRESCO_NODE_NODE_LIST           0x0009
#define FRESCO_NODE_MESH                0x0100
#define FRESCO_NODE_PATH                0x0101
#define FRESCO_NODE_PATH_SEGMENTS       0x0102
#define FRESCO_NODE_VERTEX_DATA         0x0110
#define FRESCO_NODE_INDEX_DATA          0x0111
#define FRESCO_NODE_MATERIAL_SOLID      0x0200
#define FRESCO_NODE_MATERIAL_GRADIENT   0x0201
#define FRESCO_NODE_MATERIAL_PBR        0x0202
#define FRESCO_NODE_MATERIAL_TEXTURED   0x0203
#define FRESCO_NODE_TEXT                0x0300
#define FRESCO_NODE_FONT                0x0301
#define FRESCO_NODE_TEXTURE             0x0400
#define FRESCO_NODE_PIXEL_DATA          0x0401

/* ---------------------------------------------------------------- */
/* Slot graph (the primary scene API — opcodes 0x0110-0x0117)        */
/* ---------------------------------------------------------------- */

typedef uint16_t fresco_slot_t;

#define FRESCO_SLOT_FLAG_VISIBLE   0x01u
#define FRESCO_SLOT_FLAG_CLIP      0x08u

/* Allocate a slot. Slot IDs are guest-allocated; pick one yourself.
 * `node_type` is opaque to the slot table — pass FRESCO_NODE_RENDERABLE
 * for renderable slots. `flags` should include FRESCO_SLOT_FLAG_VISIBLE
 * for the slot to be drawn. */
int  fresco_slot_alloc      (fresco_t *f, fresco_slot_t slot_id,
                             uint16_t node_type, uint32_t flags);
int  fresco_slot_free       (fresco_t *f, fresco_slot_t slot_id);

/* Set the slot's local 4x4 transform. `xform` is 16 floats, written
 * to the wire in source order. */
int  fresco_slot_set_xform_inline(fresco_t *f, fresco_slot_t slot_id,
                                  const float xform[16]);

/* Set the slot's content (a renderable blob hash). */
int  fresco_slot_set_content(fresco_t *f, fresco_slot_t slot_id,
                             const fresco_hash_t content);

/* Mark `slot_id` as the scene root for the next frame. */
int  fresco_slot_set_root   (fresco_t *f, fresco_slot_t slot_id);

/* Replace the slot's child list with `children`. The host walks the
 * tree starting from the root slot; children inherit transform via
 * the slot graph. Up to 56 children fit per command. */
int  fresco_slot_set_children(fresco_t *f, fresco_slot_t slot_id,
                              const fresco_slot_t *children, size_t n);

/* ──────────────────────────────────────────────────────────────── */
/* Multi-window protocol — phase B1                                  */
/* ──────────────────────────────────────────────────────────────── */

/* Window identifier — server-assigned by CMD_CREATE_WINDOW. */
typedef uint16_t fresco_window_t;

/* Create a server-side window of size `width × height`. `title` is
 * an optional UTF-8 short title (≤ 15 bytes); use NULL to leave
 * unset. Blocks until the server replies, returns the assigned
 * window_id in `*out`. Returns 0 on success, -1 on error. */
int fresco_create_window(fresco_t *f,
                         uint32_t width, uint32_t height,
                         uint32_t flags, const char *title,
                         fresco_window_t *out);

/* Destroy a window. Idempotent; succeeds even if the id is unknown. */
int fresco_destroy_window(fresco_t *f, fresco_window_t window_id);

/* Update a window's title (UTF-8, up to 116 bytes). */
int fresco_window_set_title(fresco_t *f, fresco_window_t window_id,
                            const char *title);

/* Position a window on the screen, in world units (the same units
 * the camera projects). The compose pass translates the window's
 * render items by (x, y) before merging into the screen scene. */
int fresco_window_set_pos(fresco_t *f, fresco_window_t window_id,
                          float x, float y);

/* Resize a window to (width, height) in logical pixels. Triggers a
 * server-side title re-layout and a RESIZED window event back to
 * the client. */
int fresco_window_set_size(fresco_t *f, fresco_window_t window_id,
                           uint32_t width, uint32_t height);

/* Per-open client slot index assigned by the kmod when this
 * connection was opened. Useful for diagnostics — apps shouldn't
 * normally need it since the wire protocol routes by window_id. */
uint32_t fresco_client_slot(const fresco_t *f);

/* Select the target window for subsequent routable ops (slot/frame/
 * scene). Default is window 0 (the screen). All slot_*, frame_*,
 * set_root, set_camera, and render calls use this window until
 * changed. */
void fresco_set_default_window(fresco_t *f, fresco_window_t window_id);

/* Frame control — slot graph traversal happens in CMD_FRAME_END. */
int  fresco_frame_begin     (fresco_t *f, uint32_t frame_number);
int  fresco_frame_end       (fresco_t *f);

/* Helper: write a 4x4 identity matrix. */
void fresco_matrix_identity (float out[16]);

/* ---------------------------------------------------------------- */
/* Input events — host-pushed pointer / keyboard / resize stream     */
/* ---------------------------------------------------------------- */

/* Raw input event (mirrors fresco-server/src/input/capture.rs).
 * 64-byte wire records get decoded into this 16-byte caller struct.
 *
 *  type             code             value_a            value_b
 *  ----             ----             -------            -------
 *  KEY              keysym           1=press 0=release  -
 *  MOUSE_MOVE       0                cursor_x (logical) cursor_y (logical)
 *  MOUSE_BUTTON     button index     1=press 0=release  -
 *  SCROLL           0                dx                 dy
 *  RESIZE           0                width              height
 */
typedef struct {
    uint16_t event_type;
    uint16_t code;
    int32_t  value_a;
    int32_t  value_b;
    /* Server-tagged target window (0 = screen / no specific window).
     * Pointer events: window under the cursor. Key events: focused
     * window. Apps filter input by this when they own multiple
     * windows. Older guests that don't read this field still see
     * coherent events; the tag is purely additive. */
    uint32_t target_window;
} fresco_input_t;

#define FRESCO_INPUT_KEY            1
#define FRESCO_INPUT_MOUSE_MOVE     2
#define FRESCO_INPUT_MOUSE_BUTTON   3
#define FRESCO_INPUT_SCROLL         4
#define FRESCO_INPUT_RESIZE         5

/* Pop one input event from the ring. Returns 1 on read, 0 if empty. */
int  fresco_input_poll(fresco_t *f, fresco_input_t *out);

/* Block until an input event is available or timeout.
 *   ms < 0 = forever, 0 = nonblocking (= fresco_input_poll).
 * Returns 1 on event, 0 on timeout, -1 on error. Drains intermediate
 * completion-ring wakes (caller should poll those separately if interested). */
int  fresco_input_wait(fresco_t *f, fresco_input_t *out, int ms);

/* ---------------------------------------------------------------- */
/* Phase-1 raw transport (escape hatch)                              */
/* ---------------------------------------------------------------- */

/* Submit one wire-format command and ring the doorbell.
 *   payload may be NULL if payload_len == 0.
 *   payload_len must be ≤ 120 (Command's 30×u32 payload region).
 * Returns 0 on success, -1 on error (errno set: EINVAL, EAGAIN if
 * the ring is full). */
int  fresco_raw_submit(fresco_t *f,
                       uint16_t opcode, uint16_t flags, uint32_t sequence_id,
                       const void *payload, size_t payload_len);

/* Pop one Completion record from the ring (128 bytes total).
 * Returns 1 on read, 0 if ring is empty, -1 on error. */
typedef struct {
    uint16_t comp_type;
    uint16_t status;
    uint32_t id;
    uint8_t  result_hash[32];
    uint8_t  _pad[88];   /* pad to exactly 128 bytes */
} fresco_completion_t;

int  fresco_raw_completion_poll(fresco_t *f, fresco_completion_t *out);

/* ---------------------------------------------------------------- */
/* Window-lifecycle events (async, queued)                          */
/* ---------------------------------------------------------------- */

/* Async events the server pushes onto the completion ring as
 * windows change state. CLOSE_REQUESTED fires when the user clicks
 * the titlebar close button; RESIZED on size changes; FOCUS on
 * focus/blur. CREATED is NOT surfaced here — it's a synchronous
 * response to fresco_create_window. */
typedef struct {
    uint16_t kind;          /* FRESCO_WIN_EVENT_* — same values as comp_type */
    uint16_t _pad0;
    uint32_t window_id;
    int32_t  value_a;       /* RESIZED: width  ; FOCUS: 1=focused 0=blurred */
    int32_t  value_b;       /* RESIZED: height ; FOCUS: 0 */
} fresco_window_event_t;

#define FRESCO_WIN_EVENT_RESIZED          0x11
#define FRESCO_WIN_EVENT_CLOSE_REQUESTED  0x12
#define FRESCO_WIN_EVENT_FOCUS            0x13

/* Pop one async window event. Returns 1 on read, 0 if no events
 * pending. Drains the completion ring as a side effect, queueing
 * any further window events. */
int  fresco_window_event_poll(fresco_t *f, fresco_window_event_t *out);

/* Block until a window event is available or timeout elapses.
 *   ms < 0 = forever, 0 = nonblocking. Returns 1 / 0 / -1. */
int  fresco_window_event_wait(fresco_t *f, fresco_window_event_t *out, int ms);

/* Unified wait: returns whichever ring has data first.
 *   ms < 0 = forever, 0 = nonblocking.
 * Return values:
 *   1 = input event read into *in_out (window_out untouched)
 *   2 = window event read into *window_out (in_out untouched)
 *   0 = timeout
 *  -1 = error
 * Apps with both input and window-lifecycle handling should call
 * this in their event loop instead of fresco_input_wait, which only
 * surfaces input and ignores the completion ring's window events. */
int  fresco_event_wait(fresco_t *f,
                       fresco_input_t *in_out,
                       fresco_window_event_t *window_out,
                       int ms);

/* ---------------------------------------------------------------- */
/* Wire-protocol opcodes (mirrors fresco-server/src/command/protocol.rs) */
/* ---------------------------------------------------------------- */

/* Resource upload */
#define FRESCO_CMD_UPLOAD_BEGIN     0x0001
#define FRESCO_CMD_UPLOAD_DATA      0x0002
#define FRESCO_CMD_UPLOAD_FINISH    0x0003
#define FRESCO_CMD_UPLOAD_DMA       0x0004

/* Legacy scene-graph (used here only for CMD_SET_CAMERA). */
#define FRESCO_CMD_SET_ROOT         0x0100
#define FRESCO_CMD_SET_CAMERA       0x0101

/* Slot-based scene graph (v2 — primary) */
#define FRESCO_CMD_SLOT_ALLOC               0x0110
#define FRESCO_CMD_SLOT_FREE                0x0111
#define FRESCO_CMD_SLOT_SET_XFORM           0x0112
#define FRESCO_CMD_SLOT_SET_CONTENT         0x0113
#define FRESCO_CMD_SLOT_SET_CHILDREN        0x0114
#define FRESCO_CMD_SLOT_SET_FLAGS           0x0115
#define FRESCO_CMD_SLOT_SET_ROOT            0x0116
#define FRESCO_CMD_SLOT_SET_TEXT            0x0117
#define FRESCO_CMD_SLOT_SET_CAS_CHILDREN    0x0118

/* Control */
#define FRESCO_CMD_RENDER           0x0300
#define FRESCO_CMD_FENCE            0x0301
#define FRESCO_CMD_QUERY_HASH       0x0302
#define FRESCO_CMD_FRAME_BEGIN      0x0303
#define FRESCO_CMD_FRAME_END        0x0304

/* Multi-window — phase B1 */
#define FRESCO_CMD_CREATE_WINDOW    0x0500
#define FRESCO_CMD_DESTROY_WINDOW   0x0501
#define FRESCO_CMD_WINDOW_SET_ROOT  0x0502
#define FRESCO_CMD_WINDOW_SET_TITLE 0x0503
#define FRESCO_CMD_WINDOW_PRESENT   0x0504
#define FRESCO_CMD_WINDOW_SET_POS   0x0505
#define FRESCO_CMD_WINDOW_SET_SIZE  0x0506

#define FRESCO_COMP_WINDOW_CREATED         0x10
#define FRESCO_COMP_WINDOW_RESIZED         0x11
#define FRESCO_COMP_WINDOW_CLOSE_REQUESTED 0x12
#define FRESCO_COMP_WINDOW_FOCUS           0x13

/* Completion comp_type values */
#define FRESCO_COMP_UPLOAD_COMPLETE     0x01
#define FRESCO_COMP_FENCE               0x02
#define FRESCO_COMP_QUERY_RESULT        0x03
#define FRESCO_COMP_ERROR               0xFF

/* Status codes (in Completion.status) */
#define FRESCO_STATUS_OK                0x00
#define FRESCO_STATUS_CAS_FULL          0x01
#define FRESCO_STATUS_INVALID_HASH      0x02
#define FRESCO_STATUS_EXISTS            0x03
#define FRESCO_STATUS_NOT_FOUND         0x04

#ifdef __cplusplus
}
#endif

#endif /* _FRESCO_H_ */
