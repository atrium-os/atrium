#include <errno.h>
#include <stdlib.h>
#include <string.h>
#include <dav1d/dav1d.h>
#include "shim.h"

typedef struct {
    Dav1dContext *c;
    Dav1dData pending;  /* data dav1d hasn't taken yet */
    Dav1dPicture pic;   /* the picture last handed out */
    int have_pic;
} st;

static void quiet(void *cookie, const char *fmt, va_list ap) { (void)cookie; (void)fmt; (void)ap; }

void *atr_dav1d_new(int threads, uint32_t max_pixels) {
    st *s = calloc(1, sizeof *s);
    if (!s) return NULL;
    Dav1dSettings set;
    dav1d_default_settings(&set);
    set.n_threads = threads;
    set.max_frame_delay = 0;
    set.frame_size_limit = max_pixels;
    set.all_layers = 0;        /* only the highest spatial layer */
    set.apply_grain = 1;
    set.logger.callback = quiet;
    if (dav1d_open(&s->c, &set) < 0) { free(s); return NULL; }
    return s;
}

int atr_dav1d_send(void *d, const uint8_t *buf, size_t len, int64_t pts) {
    st *s = d;
    if (!s || (!buf && len) || len == 0) return ATR_ERR;
    if (s->pending.sz) return ATR_AGAIN;  /* get pictures first */
    uint8_t *p = dav1d_data_create(&s->pending, len);
    if (!p) return ATR_NOMEM;
    memcpy(p, buf, len);
    s->pending.m.timestamp = pts;
    int r = dav1d_send_data(s->c, &s->pending);
    if (r < 0 && r != DAV1D_ERR(EAGAIN)) { dav1d_data_unref(&s->pending); return ATR_ERR; }
    return ATR_OK;
}

int atr_dav1d_get(void *d, atr_picture *out) {
    st *s = d;
    if (!s || !out) return ATR_ERR;
    if (s->have_pic) { dav1d_picture_unref(&s->pic); s->have_pic = 0; }
    if (s->pending.sz) {
        int r = dav1d_send_data(s->c, &s->pending);
        if (r < 0 && r != DAV1D_ERR(EAGAIN)) { dav1d_data_unref(&s->pending); return ATR_ERR; }
    }
    int r = dav1d_get_picture(s->c, &s->pic);
    if (r == DAV1D_ERR(EAGAIN)) return ATR_NONE;
    if (r == DAV1D_ERR(ENOMEM)) return ATR_NOMEM;
    if (r < 0) return ATR_ERR;
    s->have_pic = 1;
    const Dav1dPictureParameters *pp = &s->pic.p;
    out->width = (uint32_t)pp->w;
    out->height = (uint32_t)pp->h;
    out->bit_depth = (uint32_t)pp->bpc;
    switch (pp->layout) {
    case DAV1D_PIXEL_LAYOUT_I400: out->layout = 0; break;
    case DAV1D_PIXEL_LAYOUT_I420: out->layout = 1; break;
    case DAV1D_PIXEL_LAYOUT_I422: out->layout = 2; break;
    default: out->layout = 3; break;
    }
    for (int i = 0; i < 3; i++) out->plane[i] = s->pic.data[i];
    out->stride[0] = s->pic.stride[0];
    out->stride[1] = out->stride[2] = s->pic.stride[1];
    out->pts = s->pic.m.timestamp;
    out->primaries = out->transfer = out->matrix = 2;
    out->range = 0;
    out->has_hdr = 0;
    if (s->pic.seq_hdr) {
        const Dav1dSequenceHeader *q = s->pic.seq_hdr;
        out->primaries = (uint8_t)q->pri;
        out->transfer = (uint8_t)q->trc;
        out->matrix = (uint8_t)q->mtrx;
        out->range = q->color_range ? 2 : 1;
    }
    for (int i = 0; i < 12; i++) out->hdr[i] = 0;
    if (s->pic.mastering_display) {
        const Dav1dMasteringDisplay *m = s->pic.mastering_display;
        /* 0.16 fixed point to 0.00002 units; 24.8 to cd/m2; 18.14 to 0.0001. */
        for (int c = 0; c < 3; c++) {
            out->hdr[c] = (uint16_t)(((uint64_t)m->primaries[c][0] * 50000 + 32768) >> 16);
            out->hdr[3 + c] = (uint16_t)(((uint64_t)m->primaries[c][1] * 50000 + 32768) >> 16);
        }
        out->hdr[6] = (uint16_t)(((uint64_t)m->white_point[0] * 50000 + 32768) >> 16);
        out->hdr[7] = (uint16_t)(((uint64_t)m->white_point[1] * 50000 + 32768) >> 16);
        uint64_t mx = m->max_luminance >> 8, mn = ((uint64_t)m->min_luminance * 10000) >> 14;
        out->hdr[8] = (uint16_t)(mx > 65535 ? 65535 : mx);
        out->hdr[9] = (uint16_t)(mn > 65535 ? 65535 : mn);
        out->has_hdr = 1;
    }
    if (s->pic.content_light) {
        out->hdr[10] = s->pic.content_light->max_content_light_level;
        out->hdr[11] = s->pic.content_light->max_frame_average_light_level;
        out->has_hdr = 1;
    }
    return ATR_GOT;
}

void atr_dav1d_flush(void *d) {
    st *s = d;
    if (!s) return;
    if (s->have_pic) { dav1d_picture_unref(&s->pic); s->have_pic = 0; }
    if (s->pending.sz) dav1d_data_unref(&s->pending);
    dav1d_flush(s->c);
}

void atr_dav1d_free(void *d) {
    st *s = d;
    if (!s) return;
    atr_dav1d_flush(s);
    dav1d_close(&s->c);
    free(s);
}
