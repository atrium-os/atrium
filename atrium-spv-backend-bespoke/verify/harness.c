/* In-VM verification harness: dlopen the bespoke-compiled
 * fragment shader .so, call atrium_fs_main per the
 * AAPCS64 fragment ABI, print the resulting RGBA.
 * argv[1] = path to the .so. */
#include <dlfcn.h>
#include <stdio.h>
#include <stdlib.h>

typedef void (*fs_main_t)(
    const unsigned char *in_varyings,
    const unsigned char *uniforms,
    const unsigned char *push_constants,
    float fc_x, float fc_y, float fc_z, float fc_w,
    unsigned int samples_mask,
    float *out_color,
    float *out_depth);

int main(int argc, char **argv) {
    if (argc < 2) { fprintf(stderr, "usage: %s <so>\n", argv[0]); return 2; }
    void *h = dlopen(argv[1], RTLD_NOW);
    if (!h) { fprintf(stderr, "dlopen: %s\n", dlerror()); return 3; }
    fs_main_t fs = (fs_main_t)dlsym(h, "atrium_fs_main");
    if (!fs) { fprintf(stderr, "dlsym: %s\n", dlerror()); return 4; }
    float out[4] = {0,0,0,0};
    float depth = 0;
    fs(NULL, NULL, NULL, 0.f, 0.f, 0.f, 0.f, 0u, out, &depth);
    printf("%g %g %g %g\n", out[0], out[1], out[2], out[3]);
    dlclose(h);
    return 0;
}
