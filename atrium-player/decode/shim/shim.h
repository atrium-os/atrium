/* atrium-media-decode's C shims: a small, fixed ABI over dav1d, libvpx,
 * libopus and libvorbis, so the Rust side never copies a library's struct
 * layout by hand. Every function checks its arguments; none keeps a pointer
 * the caller passed after it returns.
 */
#ifndef ATR_SHIM_H
#define ATR_SHIM_H
#include <stddef.h>
#include <stdint.h>

/* A decoded picture, valid until the next get or free on its decoder. */
typedef struct {
    uint32_t width, height;
    uint32_t bit_depth;   /* 8, 10 or 12 */
    uint32_t layout;      /* 0: 4:0:0, 1: 4:2:0, 2: 4:2:2, 3: 4:4:4 */
    const uint8_t *plane[3];
    ptrdiff_t stride[3];  /* bytes */
    int64_t pts;
    /* Colour from the bitstream, H.273 code points (2: unspecified);
     * range 0 unspecified, 1 limited, 2 full. */
    uint8_t primaries, transfer, matrix, range;
    /* Static HDR metadata in CTA-861.3 units (has_hdr != 0): R, G, B x
     * then y (0.00002), white x, y, max (cd/m2), min (0.0001 cd/m2),
     * max CLL, max FALL. */
    uint32_t has_hdr;
    uint16_t hdr[12];
} atr_picture;

enum { ATR_OK = 0, ATR_AGAIN = 1, ATR_NONE = 0, ATR_GOT = 1, ATR_ERR = -1, ATR_TOO_BIG = -2, ATR_NOMEM = -3 };

/* AV1 */
void *atr_dav1d_new(int threads, uint32_t max_pixels);
int atr_dav1d_send(void *d, const uint8_t *buf, size_t len, int64_t pts);
int atr_dav1d_get(void *d, atr_picture *out);
void atr_dav1d_flush(void *d);
void atr_dav1d_free(void *d);

/* VP8 (vp9 = 0) and VP9 (vp9 = 1) */
void *atr_vpx_new(int vp9, int threads, uint32_t max_w, uint32_t max_h);
int atr_vpx_send(void *d, const uint8_t *buf, size_t len, int64_t pts);
int atr_vpx_get(void *d, atr_picture *out);
void atr_vpx_free(void *d);

/* Opus: always 48 kHz out. head is an OpusHead. */
void *atr_opus_new(const uint8_t *head, size_t len, int *channels);
/* Returns frames written (interleaved f32, at most cap frames), or < 0. */
int atr_opus_decode(void *d, const uint8_t *pkt, size_t len, float *out, int cap);
void atr_opus_reset(void *d);
void atr_opus_free(void *d);

/* Vorbis: three headers, each a u32 LE length then the bytes. */
void *atr_vorbis_new(const uint8_t *hdrs, size_t len, int *rate, int *channels);
int atr_vorbis_decode(void *d, const uint8_t *pkt, size_t len, float *out, int cap);
void atr_vorbis_reset(void *d);
void atr_vorbis_free(void *d);

#endif
