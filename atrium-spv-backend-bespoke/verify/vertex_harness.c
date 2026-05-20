/* In-VM verification harness for VERTEX shaders.
 *
 * Mirror of the fragment harness, retargeted at
 * `atrium_vs_main` — dlopen a bespoke-compiled vertex
 * shader .so, call it per the AAPCS64 vertex ABI, print
 * the resulting gl_Position as four f32s.
 *
 * Usage:
 *   vertex_harness <so> <x> <y> <z>
 *     -- passthrough: vec3 attribute (x, y, z) at
 *        location 0; null uniforms.
 *   vertex_harness <so> <x> <y> <z> <m00> <m01> ... <m33>
 *     -- MVP: same vec3 attribute, plus a column-major
 *        mat4 uniform at (set=0, binding=0) packed into
 *        a 64-byte uniform buffer (16 floats in column-
 *        major order, same shape the bespoke / cranelift
 *        emit_freebsd_obj `vertex_mvp` writes).
 *
 * Output: gl_Position printed via %.9g %.9g %.9g %.9g.
 *
 * The shader ABI (docs/spec/tier2-renderer.md §4.1):
 *   atrium_vs_main(
 *     in_attributes,    // X0
 *     in_attr_strides,  // X1
 *     uniforms,         // X2
 *     push_constants,   // X3
 *     vertex_index,     // W4
 *     instance_index,   // W5
 *     out_position,     // X6 (vec4)
 *     out_varyings,     // X7
 *     out_clip_distance // X8
 *   ) */
#include <dlfcn.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

typedef void (*vs_main_t)(
    const unsigned char *in_attributes,
    const unsigned char *in_attr_strides,
    const unsigned char *uniforms,
    const unsigned char *push_constants,
    unsigned int vertex_index,
    unsigned int instance_index,
    float *out_position,
    unsigned char *out_varyings,
    float *out_clip_distance);

int main(int argc, char **argv) {
    if (argc != 5 && argc != 5 + 16) {
        fprintf(stderr,
            "usage: %s <so> <x> <y> <z> [<m00> ... <m33>]\n"
            "       4 floats = passthrough; 4+16 = MVP with uniform mat4\n",
            argv[0]);
        return 2;
    }
    void *h = dlopen(argv[1], RTLD_NOW);
    if (!h) { fprintf(stderr, "dlopen: %s\n", dlerror()); return 3; }
    vs_main_t vs = (vs_main_t)dlsym(h, "atrium_vs_main");
    if (!vs) { fprintf(stderr, "dlsym: %s\n", dlerror()); return 4; }

    float xyz[3] = {
        (float)atof(argv[2]),
        (float)atof(argv[3]),
        (float)atof(argv[4]),
    };
    unsigned char attr[12];
    memcpy(attr, xyz, sizeof attr);

    /* Optional uniform block: 64-byte mat4 in column-major
     * order.  When present, argv[5..20] are 16 f32 lanes
     * matching the layout the host packer (pack_mat4 in
     * the differential tests + the example) lays down. */
    unsigned char *ubo_ptr = NULL;
    unsigned char ubo_buf[64];
    if (argc == 5 + 16) {
        float mvp[16];
        for (int i = 0; i < 16; i++)
            mvp[i] = (float)atof(argv[5 + i]);
        memcpy(ubo_buf, mvp, sizeof ubo_buf);
        ubo_ptr = ubo_buf;
    }

    float pos[4] = {0, 0, 0, 0};
    unsigned char varyings[256] = {0};
    float clip[8] = {0};
    vs(attr, NULL, ubo_ptr, NULL, 0u, 0u, pos, varyings, clip);

    /* Same `%.9g` rule as the fragment harness — exact
     * round-trip for power-of-two-derived f32s. */
    printf("%.9g %.9g %.9g %.9g\n", pos[0], pos[1], pos[2], pos[3]);
    dlclose(h);
    return 0;
}
