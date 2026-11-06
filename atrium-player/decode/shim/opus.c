#include <stdlib.h>
#include <string.h>
#include <opus/opus_multistream.h>
#include "shim.h"

typedef struct {
    OpusMSDecoder *d;
    int channels;
} st;

static unsigned le16(const uint8_t *p) { return p[0] | (unsigned)p[1] << 8; }

void *atr_opus_new(const uint8_t *h, size_t len, int *channels) {
    if (!h || len < 19 || memcmp(h, "OpusHead", 8) != 0 || (h[8] >> 4) != 0) return NULL;
    int ch = h[9], family = h[18];
    if (ch < 1 || ch > 8) return NULL;
    int streams, coupled;
    unsigned char map[8];
    if (family == 0) {
        if (ch > 2) return NULL;
        streams = 1;
        coupled = ch == 2;
        map[0] = 0;
        map[1] = 1;
    } else if (family == 1) {
        if (len < 21 + (size_t)ch) return NULL;
        streams = h[19];
        coupled = h[20];
        if (streams < 1 || coupled > streams || streams + coupled > 255) return NULL;
        memcpy(map, h + 21, ch);
        for (int i = 0; i < ch; i++)
            if (map[i] != 255 && map[i] >= streams + coupled) return NULL;
    } else {
        return NULL;
    }
    st *s = calloc(1, sizeof *s);
    if (!s) return NULL;
    int err;
    s->d = opus_multistream_decoder_create(48000, ch, streams, coupled, map, &err);
    if (!s->d || err != OPUS_OK) { free(s); return NULL; }
    /* Output gain (Q7.8 dB) is applied by the decoder. */
    opus_multistream_decoder_ctl(s->d, OPUS_SET_GAIN((opus_int16)le16(h + 16)));
    s->channels = ch;
    *channels = ch;
    return s;
}

int atr_opus_decode(void *d, const uint8_t *pkt, size_t len, float *out, int cap) {
    st *s = d;
    if (!s || !out || cap <= 0 || len > 0x7fffffff) return ATR_ERR;
    int n = opus_multistream_decode_float(s->d, pkt, (opus_int32)len, out, cap, 0);
    return n < 0 ? ATR_ERR : n;
}

void atr_opus_reset(void *d) {
    st *s = d;
    if (s) opus_multistream_decoder_ctl(s->d, OPUS_RESET_STATE);
}

void atr_opus_free(void *d) {
    st *s = d;
    if (!s) return;
    opus_multistream_decoder_destroy(s->d);
    free(s);
}
