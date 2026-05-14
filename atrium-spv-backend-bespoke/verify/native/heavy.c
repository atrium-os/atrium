/* Native reference for the `heavy` benchmark shader.
 *
 * Hand-written C of the exact 2-accumulator contractive
 * loop `build_heavy_spirv` emits, so `clang -O2` can be
 * the bespoke backend's *perf bar* — the question is no
 * longer "is bespoke as good as Cranelift" (it is) but
 * "how far is it from what an optimising native compiler
 * produces" (spec §8.1: target hand-written-ARM64 perf).
 *
 * Same AAPCS64 fragment ABI as the codegen'd shaders
 * (docs/spec/tier2-renderer.md §4.1). Compiled two ways by
 * bench_driver:
 *   clang -O2 -ffp-contract=off  — same arithmetic as the
 *       backends (no FMA fusion); isolates pure
 *       scheduling / register-allocation quality.
 *   clang -O2                    — default `fast` FP
 *       contraction; the true native ceiling, but FMA
 *       changes results bit-for-bit (it skips the
 *       intermediate rounding) so it is NOT bit-identical
 *       to the interpreter oracle.
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

    /* Iteration count from the i32 push-constant — read
     * byte-by-byte, little-endian, to match the shader's
     * push-constant layout without alignment assumptions. */
    int n = 0;
    if (push_constants) {
        n = (int)((u32)push_constants[0]
                | ((u32)push_constants[1] << 8)
                | ((u32)push_constants[2] << 16)
                | ((u32)push_constants[3] << 24));
    }

    float a = 0.5f, b = 0.25f;
    for (int i = 0; i < n; i++) {
        float t1  = a * b;
        float a_n = a * 0.99f + b * 0.01f;
        float b_n = b * 0.99f + t1 * 0.01f;
        a = a_n;
        b = b_n;
    }

    out_color[0] = a;
    out_color[1] = b;
    out_color[2] = 0.0f;
    out_color[3] = 1.0f;
}
