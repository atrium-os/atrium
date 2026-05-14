/* In-VM verification harness: dlopen a bespoke-compiled
 * fragment shader .so, call atrium_fs_main per the
 * AAPCS64 fragment ABI, print the resulting RGBA.
 *
 * Usage:
 *   harness <so>                  -- no push-constants
 *   harness <so> <f32-push-const> -- one f32 in push_constants[0..4]
 *
 * The optional second arg lets the if/else / arithmetic
 * shaders be driven with a real input so we exercise
 * AAPCS64 control flow (b.cond, branch relocation) +
 * fcmp/fcsel on the production target, not just a
 * constant store. */
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
        fprintf(stderr, "usage: %s <so> [f32-push-const]\n", argv[0]);
        return 2;
    }
    void *h = dlopen(argv[1], RTLD_NOW);
    if (!h) { fprintf(stderr, "dlopen: %s\n", dlerror()); return 3; }
    fs_main_t fs = (fs_main_t)dlsym(h, "atrium_fs_main");
    if (!fs) { fprintf(stderr, "dlsym: %s\n", dlerror()); return 4; }

    unsigned char pc[16] = {0};
    const unsigned char *pc_ptr = NULL;
    if (argc >= 3) {
        float v = (float)atof(argv[2]);
        memcpy(pc, &v, sizeof v);
        pc_ptr = pc;
    }

    float out[4] = {0,0,0,0};
    float depth = 0;
    fs(NULL, NULL, pc_ptr, 0.f, 0.f, 0.f, 0.f, 0u, out, &depth);
    printf("%g %g %g %g\n", out[0], out[1], out[2], out[3]);
    dlclose(h);
    return 0;
}
