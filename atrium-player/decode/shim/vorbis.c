#include <stdlib.h>
#include <string.h>
#include <vorbis/codec.h>
#include "shim.h"

typedef struct {
    vorbis_info vi;
    vorbis_comment vc;
    vorbis_dsp_state vd;
    vorbis_block vb;
    int ready;
    ogg_int64_t packetno;
} st;

static int packet(st *s, const uint8_t *p, size_t len, int bos, ogg_packet *op) {
    if (len > 0x7fffffff) return -1;
    memset(op, 0, sizeof *op);
    op->packet = (unsigned char *)p; /* libvorbis only reads it */
    op->bytes = (long)len;
    op->b_o_s = bos;
    op->packetno = s->packetno++;
    return 0;
}

static void teardown(st *s) {
    if (s->ready) {
        vorbis_block_clear(&s->vb);
        vorbis_dsp_clear(&s->vd);
    }
    vorbis_comment_clear(&s->vc);
    vorbis_info_clear(&s->vi);
}

void *atr_vorbis_new(const uint8_t *h, size_t len, int *rate, int *channels) {
    if (!h) return NULL;
    st *s = calloc(1, sizeof *s);
    if (!s) return NULL;
    vorbis_info_init(&s->vi);
    vorbis_comment_init(&s->vc);
    size_t at = 0;
    for (int i = 0; i < 3; i++) {
        if (len - at < 4) goto fail;
        uint32_t n = h[at] | (uint32_t)h[at + 1] << 8 | (uint32_t)h[at + 2] << 16 | (uint32_t)h[at + 3] << 24;
        at += 4;
        if (n > len - at) goto fail;
        ogg_packet op;
        if (packet(s, h + at, n, i == 0, &op) < 0) goto fail;
        if (vorbis_synthesis_headerin(&s->vi, &s->vc, &op) != 0) goto fail;
        at += n;
    }
    if (s->vi.channels < 1 || s->vi.channels > 8 || s->vi.rate < 1 || s->vi.rate > 768000) goto fail;
    if (vorbis_synthesis_init(&s->vd, &s->vi) != 0) goto fail;
    if (vorbis_block_init(&s->vd, &s->vb) != 0) { vorbis_dsp_clear(&s->vd); goto fail; }
    s->ready = 1;
    *rate = (int)s->vi.rate;
    *channels = s->vi.channels;
    return s;
fail:
    teardown(s);
    free(s);
    return NULL;
}

int atr_vorbis_decode(void *d, const uint8_t *pkt, size_t len, float *out, int cap) {
    st *s = d;
    if (!s || !s->ready || !pkt || !out || cap <= 0) return ATR_ERR;
    ogg_packet op;
    if (packet(s, pkt, len, 0, &op) < 0) return ATR_ERR;
    if (vorbis_synthesis(&s->vb, &op) != 0) return ATR_ERR;
    if (vorbis_synthesis_blockin(&s->vd, &s->vb) != 0) return ATR_ERR;
    float **pcm;
    int total = 0, n, ch = s->vi.channels;
    while ((n = vorbis_synthesis_pcmout(&s->vd, &pcm)) > 0) {
        if (n > cap - total) n = cap - total;
        for (int i = 0; i < n; i++)
            for (int c = 0; c < ch; c++) out[(size_t)(total + i) * ch + c] = pcm[c][i];
        vorbis_synthesis_read(&s->vd, n);
        total += n;
        if (total == cap) break;
    }
    return total;
}

void atr_vorbis_reset(void *d) {
    st *s = d;
    if (s && s->ready) vorbis_synthesis_restart(&s->vd);
}

void atr_vorbis_free(void *d) {
    st *s = d;
    if (!s) return;
    teardown(s);
    free(s);
}
