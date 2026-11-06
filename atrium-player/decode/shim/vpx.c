#include <stdlib.h>
#include <string.h>
#include <vpx/vpx_decoder.h>
#include <vpx/vp8dx.h>
#include "shim.h"

typedef struct {
    vpx_codec_ctx_t c;
    vpx_codec_iface_t *ifc;
    vpx_codec_iter_t iter;
    uint32_t max_w, max_h;
    int vp9;
    /* pts per decode call; libvpx gives one shown frame per call for
     * VP8 and VP9 (hidden frames in a superframe aren't output). */
    int64_t pts[16];
    unsigned head, tail;
} st;

void *atr_vpx_new(int vp9, int threads, uint32_t max_w, uint32_t max_h) {
    st *s = calloc(1, sizeof *s);
    if (!s) return NULL;
    vpx_codec_dec_cfg_t cfg = { .threads = threads > 0 ? (unsigned)threads : 1 };
    vpx_codec_iface_t *ifc = vp9 ? vpx_codec_vp9_dx() : vpx_codec_vp8_dx();
    if (vpx_codec_dec_init(&s->c, ifc, &cfg, 0) != VPX_CODEC_OK) { free(s); return NULL; }
    s->ifc = ifc;
    s->vp9 = vp9;
    s->max_w = max_w;
    s->max_h = max_h;
    return s;
}

int atr_vpx_send(void *d, const uint8_t *buf, size_t len, int64_t pts) {
    st *s = d;
    if (!s || !buf || len == 0 || len > 0x7fffffff) return ATR_ERR;
    if (s->head - s->tail >= 16) return ATR_AGAIN;
    /* The decoder parses sizes itself; check them before it allocates for
     * a keyframe. */
    vpx_codec_stream_info_t si = { .sz = sizeof si };
    if (vpx_codec_peek_stream_info(s->ifc, buf, (unsigned)len, &si) == VPX_CODEC_OK
        && si.is_kf && (si.w > s->max_w || si.h > s->max_h))
        return ATR_TOO_BIG;
    s->iter = NULL;
    if (vpx_codec_decode(&s->c, buf, (unsigned)len, NULL, 0) != VPX_CODEC_OK) return ATR_ERR;
    s->pts[s->head++ % 16] = pts;
    return ATR_OK;
}

int atr_vpx_get(void *d, atr_picture *out) {
    st *s = d;
    if (!s || !out) return ATR_ERR;
    vpx_image_t *img = vpx_codec_get_frame(&s->c, &s->iter);
    if (!img) {
        /* No frame for that packet (a hidden one): drop its pts. */
        if (s->head != s->tail) s->tail = s->head;
        return ATR_NONE;
    }
    if (img->d_w > s->max_w || img->d_h > s->max_h) return ATR_TOO_BIG;
    out->width = img->d_w;
    out->height = img->d_h;
    out->bit_depth = img->bit_depth ? img->bit_depth : 8;
    switch (img->fmt & ~VPX_IMG_FMT_HIGHBITDEPTH) {
    case VPX_IMG_FMT_I420: out->layout = 1; break;
    case VPX_IMG_FMT_I422: out->layout = 2; break;
    case VPX_IMG_FMT_I444: out->layout = 3; break;
    case VPX_IMG_FMT_I440: return ATR_ERR;
    default: return ATR_ERR;
    }
    for (int i = 0; i < 3; i++) {
        out->plane[i] = img->planes[i];
        out->stride[i] = img->stride[i];
    }
    out->pts = s->head != s->tail ? s->pts[s->tail++ % 16] : 0;
    /* VP9 says only the matrix and the range; VP8 says neither. */
    out->primaries = out->transfer = 2;
    switch (img->cs) {
    case VPX_CS_BT_601: case VPX_CS_SMPTE_170: out->matrix = 6; break;
    case VPX_CS_BT_709: out->matrix = 1; break;
    case VPX_CS_SMPTE_240: out->matrix = 7; break;
    case VPX_CS_BT_2020: out->matrix = 9; break;
    case VPX_CS_SRGB: out->matrix = 0; break;
    default: out->matrix = 2; break;
    }
    out->range = s->vp9 ? (img->range == VPX_CR_FULL_RANGE ? 2 : 1) : 0;
    out->has_hdr = 0;
    for (int i = 0; i < 12; i++) out->hdr[i] = 0;
    return ATR_GOT;
}

void atr_vpx_free(void *d) {
    st *s = d;
    if (!s) return;
    vpx_codec_destroy(&s->c);
    free(s);
}
