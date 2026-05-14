/* In-VM verification harness: dlopen a bespoke-compiled
 * fragment shader .so, call atrium_fs_main per the
 * AAPCS64 fragment ABI, print the resulting RGBA.
 *
 * Usage:
 *   harness <so>                     -- no push-constants
 *   harness <so> <push-const>        -- f32  in push_constants[0..4]
 *   harness <so> <push-const> int    -- i32  in push_constants[0..4]
 *
 * The optional push-const arg lets the if/else / loop /
 * arithmetic shaders be driven with a real input so we
 * exercise AAPCS64 control flow (b.cond, back-edge
 * relocation), fcmp/fcsel, and the W-reg integer pool on
 * the production target — not just a constant store.
 * The trailing `int` switches the push-const from f32 to
 * i32 (loop shaders take an i32 iteration count). */
#include <dlfcn.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

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
        fprintf(stderr, "usage: %s <so> [push-const] [int]\n", argv[0]);
        return 2;
    }
    void *h = dlopen(argv[1], RTLD_NOW);
    if (!h) { fprintf(stderr, "dlopen: %s\n", dlerror()); return 3; }
    fs_main_t fs = (fs_main_t)dlsym(h, "atrium_fs_main");
    if (!fs) { fprintf(stderr, "dlsym: %s\n", dlerror()); return 4; }

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
