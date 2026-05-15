/* Native reference for the `heavyvec` benchmark shader.
 *
 * Hand-written C of the vec4-accumulator loop
 * `build_heavyvec_spirv` emits: a single `vec4` carried
 * across the loop, updated `v = v*0.99 + bias` per
 * iteration. This is the shape that should finally expose
 * a gap to `clang -O2`: the four lanes are independent, so
 * an optimising compiler SIMD-packs them into one
 * `fmul.4s` + `fadd.4s` (or a fused `fmla.4s`) per
 * iteration, where the bespoke backend lane-walks four
 * scalar `fmul` + four scalar `fadd`. The vec-Phi support
 * (per-lane scalar decomposition + per-lane coalescing)
 * gets the bespoke path *correct* and call-overhead-free;
 * this bench measures how far that scalarised loop body
 * sits from native SIMD.
 *
 * Same AAPCS64 fragment ABI (docs/spec §4.1). bench_driver
 * compiles it `-ffp-contract=off` (same arithmetic as the
 * backends) and plain `-O2` (the FMA-fused ceiling).
 */

typedef unsigned int u32;

void atrium_fs_main(
    const unsigned char *in_varyings,
    const unsigned char *uniforms,
    const unsigned char *push_constants,
    float fc_x, float fc_y, float fc_z, float fc_w,
    u32 samples_mask,
    float *out_color,
    float *out_depth)
{
    (void)in_varyings; (void)uniforms;
    (void)fc_x; (void)fc_y; (void)fc_z; (void)fc_w;
    (void)samples_mask; (void)out_depth;

    int n = 0;
    if (push_constants) {
        n = (int)((u32)push_constants[0]
                | ((u32)push_constants[1] << 8)
                | ((u32)push_constants[2] << 16)
                | ((u32)push_constants[3] << 24));
    }

    float v[4]    = { 0.5f, 0.25f, 0.125f, 0.0625f };
    float bias[4] = { 0.001f, 0.002f, 0.003f, 0.004f };
    for (int i = 0; i < n; i++) {
        for (int k = 0; k < 4; k++) {
            v[k] = v[k] * 0.99f + bias[k];
        }
    }

    out_color[0] = v[0];
    out_color[1] = v[1];
    out_color[2] = v[2];
    out_color[3] = v[3];
}
