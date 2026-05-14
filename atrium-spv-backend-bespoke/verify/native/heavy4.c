/* Native reference for the `heavy4` benchmark shader.
 *
 * Hand-written C of the four-accumulator cross-coupled
 * loop `build_heavy4_spirv` emits. Where `heavy.c` is a
 * two-accumulator chain, this one gives an optimising
 * compiler four partly-independent FP chains to schedule
 * — more instruction-level parallelism to exploit, so it
 * is the better test of whether the bespoke backend's
 * single-pass scheduling keeps up with `clang -O2`.
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

    float a = 0.5f, b = 0.25f, c = 0.125f, d = 0.0625f;
    for (int i = 0; i < n; i++) {
        float t1  = a * b;
        float t2  = c * d;
        float a_n = a * 0.99f + t2 * 0.01f;
        float b_n = b * 0.99f + t1 * 0.01f;
        float c_n = c * 0.99f + a  * 0.005f;
        float d_n = d * 0.99f + b  * 0.005f;
        a = a_n;
        b = b_n;
        c = c_n;
        d = d_n;
    }

    out_color[0] = a;
    out_color[1] = b;
    out_color[2] = c;
    out_color[3] = d;
}
