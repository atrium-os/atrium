/*
 * lyra_node.h — the Lyra C node ABI (the cross-language plugin contract).
 *
 * Lyra's first ambition (pro-native co-equal): third-party DSP is a first-class
 * graph node. C is the lingua franca of audio plugins (LADSPA/LV2/VST/CLAP all
 * present a C ABI), so this is the contract a plugin implements. It is small on
 * purpose — one exported symbol, a flat descriptor of function pointers, no
 * framework.
 *
 * How it composes with Lyra's other two ambitions:
 *  - capability-jailed nodes (ambition 3): an UNTRUSTED plugin is not dlopen'd
 *    into lyrad. A tiny `lyra-host` shim runs inside a Portcullis jail, dlopens
 *    the .so there, and bridges it to the audio ring (ring.rs). A crash or
 *    runaway in process() kills only the jail; lyrad bypasses it (the L3 demo).
 *    TRUSTED plugins (signed, core DSP) may be loaded in-process for latency.
 *  - the deadline lane (the thesis): the hosting process is a CBS lane entity;
 *    process() runs inside that reservation. `latency_frames` feeds PDC
 *    (lyra_pdc) so parallel chains stay phase-coherent.
 *
 * REAL-TIME CONTRACT for process(): no malloc/free, no locks, no syscalls, no
 * unbounded loops. Allocate in instantiate(); only touch the passed buffers and
 * the instance handle in process(). Breaking this is what the jail contains.
 */
#ifndef LYRA_NODE_H
#define LYRA_NODE_H

#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

#define LYRA_NODE_ABI_VERSION 1u

/* Per-buffer run context the host fills in before each process() call. */
typedef struct lyra_run_ctx {
	uint32_t sample_rate; /* Hz */
	uint32_t nframes;     /* frames in this buffer */
	uint32_t nchannels;   /* interleaved channels (matches instantiate) */
	uint32_t _reserved;   /* keep 8-byte alignment for frame_pos */
	uint64_t frame_pos;   /* sample-accurate transport position of in[0] */
} lyra_run_ctx;

/* Opaque handle to plugin-allocated per-instance state. */
typedef void *lyra_node_handle;

/*
 * The descriptor every plugin exports. A NULL function pointer means "not
 * provided" for the optional ones (set_param); the lifecycle + process ones are
 * mandatory.
 */
typedef struct lyra_node_desc {
	uint32_t abi_version;    /* MUST equal LYRA_NODE_ABI_VERSION */
	const char *name;        /* stable identifier, e.g. "tremolo" */
	uint32_t nchannels;      /* channels processed, or 0 = accept any */
	uint32_t latency_frames; /* introduced latency, for PDC */

	/* lifecycle (mandatory) */
	lyra_node_handle (*instantiate)(uint32_t sample_rate, uint32_t nchannels);
	void (*destroy)(lyra_node_handle);

	/* the hot path (mandatory): process ctx->nframes * ctx->nchannels
	 * interleaved floats from `in` to `out` (may alias for in-place). */
	void (*process)(lyra_node_handle, const lyra_run_ctx *,
	    const float *in, float *out);

	/* set a control parameter by id (optional; may be NULL) */
	void (*set_param)(lyra_node_handle, uint32_t id, float value);
} lyra_node_desc;

/*
 * The single symbol every Lyra plugin defines. Returns a pointer to a static
 * descriptor (lives for the life of the shared object).
 */
const lyra_node_desc *lyra_node_descriptor(void);

#ifdef __cplusplus
}
#endif

#endif /* LYRA_NODE_H */
