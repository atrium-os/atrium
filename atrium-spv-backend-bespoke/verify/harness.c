/* In-VM verification harness: dlopen a bespoke-compiled
 * fragment shader .so, call atrium_fs_main per the
 * AAPCS64 fragment ABI, print the resulting RGBA.
 *
 * Usage:
 *   harness <so>                       -- no push-constants
 *   harness <so> <push-const>          -- f32  in push_constants[0..4]
 *   harness <so> <push-const> int      -- i32  in push_constants[0..4]
 *   harness <so> texsample             -- texture-sample mode: builds
 *                                         a 2x2 RGBW texture, a
 *                                         Nearest/Clamp sampler, packs
 *                                         the v1 descriptor table into
 *                                         uniforms, and exposes a
 *                                         minimal nearest+clamp+RGBA8
 *                                         `atrium_tex_sample_2d` for
 *                                         the shader to blr through.
 *
 * The optional push-const arg lets the if/else / loop /
 * arithmetic shaders be driven with a real input so we
 * exercise AAPCS64 control flow (b.cond, back-edge
 * relocation), fcmp/fcsel, and the W-reg integer pool on
 * the production target — not just a constant store.
 * The trailing `int` switches the push-const from f32 to
 * i32 (loop shaders take an i32 iteration count).
 *
 * Texture-sample mode is the on-target gate for the
 * texture/sampler arc (see RUNBOOK). The C-side
 * `atrium_tex_sample_2d` here is a deliberate slim port
 * of `atrium-spv-runtime` — nearest + clamp + RGBA8Unorm
 * only, enough for the host-side `texsample` shader
 * that samples a known texel and reports its colour.
 * The shader code emitted by either backend (Cranelift
 * compile() or bespoke compile()) `blr`s through a
 * function-pointer slot the harness writes into the
 * uniforms buffer's v1 prefix — so the shader is
 * reloc-free and the harness is the only thing that
 * needs to *be* the runtime. */
#include <dlfcn.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

typedef struct {
    const unsigned char *data;
    uint32_t width;
    uint32_t height;
    uint32_t stride_bytes;
    uint32_t format;        /* 0 = RGBA8Unorm */
} tex_desc_t;

typedef struct {
    uint32_t mag_filter;    /* 0 = Nearest, 1 = Linear */
    uint32_t min_filter;
    uint32_t wrap_s;        /* 0 = ClampToEdge */
    uint32_t wrap_t;
} samp_desc_t;

/* Slim host-side mirror of atrium_spv_runtime::sample_2d_impl.
 * RGBA8Unorm + Nearest + ClampToEdge only. The texsample
 * shader fixes those choices in its sampler descriptor. */
static void atrium_tex_sample_2d(
    const tex_desc_t *tex,
    const samp_desc_t *samp,
    float u, float v,
    float *out_rgba)
{
    (void)samp;  /* nearest+clamp only */
    float x = u * (float)tex->width  - 0.5f;
    float y = v * (float)tex->height - 0.5f;
    int xi = (int)(x < 0 ? x - 0.5f : x + 0.5f);
    int yi = (int)(y < 0 ? y - 0.5f : y + 0.5f);
    if (xi < 0) xi = 0;
    if ((uint32_t)xi >= tex->width)  xi = (int)tex->width  - 1;
    if (yi < 0) yi = 0;
    if ((uint32_t)yi >= tex->height) yi = (int)tex->height - 1;
    const unsigned char *p = tex->data
        + (size_t)yi * (size_t)tex->stride_bytes
        + (size_t)xi * 4;
    out_rgba[0] = (float)p[0] / 255.0f;
    out_rgba[1] = (float)p[1] / 255.0f;
    out_rgba[2] = (float)p[2] / 255.0f;
    out_rgba[3] = (float)p[3] / 255.0f;
}

/* Unused fetch — present so the v1 helper header slot at
 * uniforms[8..16] gets a valid pointer too. Not currently
 * called by any in-VM shader; here for ABI symmetry. */
static void atrium_tex_fetch_2d(
    const tex_desc_t *tex,
    int32_t x, int32_t y, int32_t lod,
    float *out_rgba)
{
    (void)lod;
    if (x < 0) x = 0;
    if ((uint32_t)x >= tex->width)  x = (int32_t)tex->width  - 1;
    if (y < 0) y = 0;
    if ((uint32_t)y >= tex->height) y = (int32_t)tex->height - 1;
    const unsigned char *p = tex->data
        + (size_t)y * (size_t)tex->stride_bytes
        + (size_t)x * 4;
    out_rgba[0] = (float)p[0] / 255.0f;
    out_rgba[1] = (float)p[1] / 255.0f;
    out_rgba[2] = (float)p[2] / 255.0f;
    out_rgba[3] = (float)p[3] / 255.0f;
}

typedef void (*fs_main_t)(
    const unsigned char *in_varyings,
    const unsigned char *uniforms,
    const unsigned char *push_constants,
    float fc_x, float fc_y, float fc_z, float fc_w,
    unsigned int samples_mask,
    float *out_color,
    float *out_depth);

int main(int argc, char **argv) {
    if (argc < 2) {
        fprintf(stderr, "usage: %s <so> [push-const|texsample] [int]\n", argv[0]);
        return 2;
    }
    void *h = dlopen(argv[1], RTLD_NOW);
    if (!h) { fprintf(stderr, "dlopen: %s\n", dlerror()); return 3; }
    fs_main_t fs = (fs_main_t)dlsym(h, "atrium_fs_main");
    if (!fs) { fprintf(stderr, "dlsym: %s\n", dlerror()); return 4; }

    /* Texture-sample mode: build a 2x2 RGBW + a nearest/
     * clamp sampler + the v1 uniforms-buffer prefix
     * (helper pointers at [0..16], descriptor slot 0 at
     * [16..32]). Then invoke the shader with that
     * uniforms ptr. */
    if (argc >= 3 && strcmp(argv[2], "texsample") == 0) {
        static const unsigned char pixels[16] = {
            255,   0,   0, 255,   /* (0,0) red */
              0, 255,   0, 255,   /* (1,0) green */
              0,   0, 255, 255,   /* (0,1) blue */
            255, 255, 255, 255,   /* (1,1) white */
        };
        tex_desc_t  tex_desc  = { pixels, 2, 2, 8, 0 };
        samp_desc_t samp_desc = { 0, 0, 0, 0 };  /* Nearest/Clamp */

        unsigned char uniforms[32] = {0};
        uintptr_t  sample_addr = (uintptr_t)&atrium_tex_sample_2d;
        uintptr_t  fetch_addr  = (uintptr_t)&atrium_tex_fetch_2d;
        uintptr_t  tex_addr    = (uintptr_t)&tex_desc;
        uintptr_t  samp_addr   = (uintptr_t)&samp_desc;
        memcpy(&uniforms[ 0], &sample_addr, sizeof sample_addr);
        memcpy(&uniforms[ 8], &fetch_addr,  sizeof fetch_addr);
        memcpy(&uniforms[16], &tex_addr,    sizeof tex_addr);
        memcpy(&uniforms[24], &samp_addr,   sizeof samp_addr);

        float out[4] = {0,0,0,0};
        float depth = 0;
        fs(NULL, uniforms, NULL, 0.f, 0.f, 0.f, 0.f, 0u, out, &depth);
        printf("%.9g %.9g %.9g %.9g\n", out[0], out[1], out[2], out[3]);
        dlclose(h);
        return 0;
    }

    unsigned char pc[16] = {0};
    const unsigned char *pc_ptr = NULL;
    if (argc >= 3) {
        int as_int = (argc >= 4 && strcmp(argv[3], "int") == 0);
        if (as_int) {
            int iv = atoi(argv[2]);
            memcpy(pc, &iv, sizeof iv);
        } else {
            float fv = (float)atof(argv[2]);
            memcpy(pc, &fv, sizeof fv);
        }
        pc_ptr = pc;
    }

    float out[4] = {0,0,0,0};
    float depth = 0;
    fs(NULL, NULL, pc_ptr, 0.f, 0.f, 0.f, 0.f, 0u, out, &depth);
    /* %.9g, not %g: the default %g caps at 6 significant
     * digits, so exact values like 0.97265625 print as
     * "0.972656" and spuriously diverge from the host's
     * Rust `{}` (shortest round-trip) expected string. All
     * verification shaders use power-of-two-derived exact
     * f32 values, so 9 significant digits reproduces them
     * exactly and matches Rust's formatting. */
    printf("%.9g %.9g %.9g %.9g\n", out[0], out[1], out[2], out[3]);
    dlclose(h);
    return 0;
}
