/*
 * tremolo.c — the reference Lyra C node plugin.
 *
 * The canonical "hello world" of the node ABI: amplitude modulation by a
 * low-frequency oscillator. Mirrors the tremolo the in-tree lyra-effect process
 * runs, so it is a drop-in proof that a C .so and a Rust node are
 * interchangeable behind the same contract. Real-time clean: all state is
 * allocated in instantiate(), process() only does arithmetic.
 *
 * Build:  cc -shared -fPIC -I../include tremolo.c -o tremolo.so   (-lm if needed)
 */
#include "lyra_node.h"
#include <math.h>
#include <stdlib.h>

#define TWO_PI 6.28318530717958647692f

/* Control parameter ids. */
enum { PARAM_RATE_HZ = 0, PARAM_DEPTH = 1 };

typedef struct trem {
	float phase;   /* LFO phase, radians */
	float inc;     /* phase increment per frame */
	float depth;   /* 0..1 modulation depth */
	uint32_t sr;   /* sample rate */
} trem;

static void
trem_recompute_inc(trem *t, float rate_hz)
{
	t->inc = TWO_PI * rate_hz / (float)t->sr;
}

static lyra_node_handle
trem_instantiate(uint32_t sample_rate, uint32_t nchannels)
{
	(void)nchannels;
	trem *t = (trem *)calloc(1, sizeof(trem));
	if (t == NULL)
		return NULL;
	t->sr = sample_rate ? sample_rate : 48000u;
	t->depth = 0.5f;
	t->phase = 0.0f;
	trem_recompute_inc(t, 5.0f); /* default 5 Hz */
	return t;
}

static void
trem_destroy(lyra_node_handle h)
{
	free(h);
}

static void
trem_process(lyra_node_handle h, const lyra_run_ctx *ctx, const float *in,
    float *out)
{
	trem *t = (trem *)h;
	uint32_t ch = ctx->nchannels;
	for (uint32_t f = 0; f < ctx->nframes; f++) {
		/* unipolar LFO in 0..1; depth scales how deep it dips. */
		float lfo = 0.5f * (1.0f - cosf(t->phase));
		float g = 1.0f - t->depth * lfo;
		for (uint32_t c = 0; c < ch; c++)
			out[f * ch + c] = in[f * ch + c] * g;
		t->phase += t->inc;
		if (t->phase > TWO_PI)
			t->phase -= TWO_PI;
	}
}

static void
trem_set_param(lyra_node_handle h, uint32_t id, float value)
{
	trem *t = (trem *)h;
	switch (id) {
	case PARAM_RATE_HZ:
		trem_recompute_inc(t, value);
		break;
	case PARAM_DEPTH:
		t->depth = value < 0.0f ? 0.0f : (value > 1.0f ? 1.0f : value);
		break;
	default:
		break;
	}
}

static const lyra_node_desc DESC = {
	.abi_version = LYRA_NODE_ABI_VERSION,
	.name = "tremolo",
	.nchannels = 0, /* any */
	.latency_frames = 0,
	.instantiate = trem_instantiate,
	.destroy = trem_destroy,
	.process = trem_process,
	.set_param = trem_set_param,
};

const lyra_node_desc *
lyra_node_descriptor(void)
{
	return &DESC;
}
