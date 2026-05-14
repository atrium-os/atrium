/* Native reference for the `loop` benchmark shader.
 *
 * Hand-written C of the counted integer sum loop
 * `build_loop_spirv` emits: `acc = 0; for i in 0..n: acc
 * += i;` then `out = (acc == 10) ? white : black`.
 *
 * This one is here for a *different* reason than `heavy` /
 * `heavy4`. Those test scalar-FP codegen quality. This
 * tests something the backends don't do at all:
 * loop-idiom recognition. `clang -O2` recognises the
 * counted sum and replaces the whole loop with the closed
 * form `n*(n-1)/2` — O(1). The bespoke backend and
 * Cranelift both emit the literal O(n) loop. So the gap
 * `bench_driver` shows here is not codegen quality; it's
 * "an optimising compiler does strength reduction / idiom
 * recognition and our single-pass backend does not." An
 * honest data point about where the *real* distance to a
 * full optimiser still lies.
 *
 * Same AAPCS64 fragment ABI (docs/spec §4.1).
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

    int acc = 0;
    for (int i = 0; i < n; i++) {
        acc += i;
    }

    float lum = (acc == 10) ? 1.0f : 0.0f;
    out_color[0] = lum;
    out_color[1] = lum;
    out_color[2] = lum;
    out_color[3] = 1.0f;
}
