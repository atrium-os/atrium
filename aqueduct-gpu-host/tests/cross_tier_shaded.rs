//! Cross-tier per-pipeline certification via the differential harness
//! (`docs/spec/energy-policy.md` §"Per-pipeline certification").
//!
//! The Tier-2 harness already runs a fragment shader three independent ways
//! (interpreter / Cranelift / bespoke) and asserts pixel agreement. This
//! adds a **fourth** runner — `MoltenVkShaderRunner`, which runs the FS on
//! Metal (Tier-3) — so `assert_shader_agrees` becomes a cross-tier check.
//!
//! Rung 1 (this file): a constant (no-input) fragment shader. The MoltenVK
//! runner renders it over a full-screen triangle and reads back the pixel;
//! it agrees with the interpreter oracle. Later rungs feed varyings /
//! uniforms / textures to Metal per invocation.
//!
//! Gated on a working MoltenVK loader; skips cleanly when absent. Run with
//! `DYLD_LIBRARY_PATH=/opt/homebrew/lib`.

use aqueduct_gpu::ids::{IdNamespace, ResourceId};
use aqueduct_gpu_host::{Backend, MoltenVkBackend};
use atrium_spv_tests::harness::{
    assert_shader_agrees, BackendError, InterpreterRunner, ShaderRunner,
};
use atrium_spv_tests::interpreter::{ShaderInputs, ShaderOutputs};
use atrium_spv_tests::pixels::ColorTolerance;

/// Runs a fragment shader on Tier-3 (MoltenVK / Metal) for the harness.
struct MoltenVkShaderRunner {
    backend: MoltenVkBackend,
    vs: Vec<u8>,
    next_id: std::cell::Cell<u32>,
}

impl std::fmt::Debug for MoltenVkShaderRunner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("moltenvk")
    }
}

impl MoltenVkShaderRunner {
    fn new() -> Option<Self> {
        Some(MoltenVkShaderRunner {
            backend: MoltenVkBackend::new().ok()?,
            vs: build_fullscreen_tri_vs(),
            next_id: std::cell::Cell::new(0x9000),
        })
    }
    fn fresh(&self) -> u32 {
        let v = self.next_id.get();
        self.next_id.set(v + 1);
        v
    }
}

impl ShaderRunner for MoltenVkShaderRunner {
    fn name(&self) -> &'static str {
        "moltenvk"
    }

    fn run(&self, fs_spirv: &[u8], inputs: &ShaderInputs) -> Result<ShaderOutputs, BackendError> {
        let varyings = &inputs.varyings_per_invocation;
        let has_push = inputs.push_constants.iter().any(|&b| b != 0);
        let has_ubo = !inputs.uniforms.is_empty() && inputs.textures.is_empty();
        let has_tex = !inputs.textures.is_empty();
        if (has_push || has_ubo || has_tex) && !varyings.is_empty() {
            return Err(BackendError::Unsupported(
                "moltenvk runner: combined uniform/texture + varyings not yet supported".into(),
            ));
        }

        let mut pixels = Vec::new();
        if has_tex {
            // Rung 4b: a sampled texture (combined image sampler) at
            // (set 0, binding 0). The FS samples at a constant UV.
            let t = &inputs.textures[0];
            if t.format != 0 {
                // 0 = Rgba8Unorm in atrium_spv_runtime::TexFormat.
                return Err(BackendError::Unsupported("moltenvk runner: RGBA8 textures only".into()));
            }
            let tex = aqueduct_gpu_host::TexBind {
                data: &t.data, width: t.width, height: t.height,
                linear: t.sampler.mag_filter == 1, // FilterMode::Linear == 1
            };
            pixels.push(self.render_pixel_tex(&self.vs, fs_spirv, tex)?);
        } else if has_ubo {
            // Rung 4a: a UBO bound at (set 0, binding 0) via descriptors.
            pixels.push(self.render_pixel_ubo(&self.vs, fs_spirv, &inputs.uniforms)?);
        } else if has_push {
            // Rung 3: feed the push-constant block to Metal (VERTEX|FRAGMENT)
            // via the plain VS; the FS reads it directly.
            pixels.push(self.render_pixel(&self.vs, fs_spirv, &inputs.push_constants)?);
        } else if varyings.is_empty() {
            // Rung 1: constant FS, plain full-screen-tri VS, no inputs.
            pixels.push(self.render_pixel(&self.vs, fs_spirv, &[])?);
        } else {
            // Rung 2: bake each invocation's varying into a per-invocation VS
            // as a Location-0 vec4 constant.
            for vb in varyings {
                if vb.len() != 16 {
                    return Err(BackendError::Unsupported(
                        "moltenvk runner rung 2: single vec4 varying only".into(),
                    ));
                }
                let le = |o: usize| f32::from_le_bytes([vb[o], vb[o + 1], vb[o + 2], vb[o + 3]]);
                let v = [le(0), le(4), le(8), le(12)];
                let vs = build_tri_vs_with_varying(v);
                pixels.push(self.render_pixel(&vs, fs_spirv, &[])?);
            }
        }
        Ok(ShaderOutputs { pixels })
    }
}

impl MoltenVkShaderRunner {
    /// Render `fs` with vertex shader `vs` (+ optional `push` constants) over
    /// a 2×2 target and read back the (uniform) pixel as RGBA f32.
    fn render_pixel(&self, vs: &[u8], fs: &[u8], push: &[u8]) -> Result<[f32; 4], BackendError> {
        const W: u32 = 2;
        const H: u32 = 2;
        let img = ResourceId::new(IdNamespace::IcdRuntime, self.fresh());
        let buf = ResourceId::new(IdNamespace::IcdRuntime, self.fresh());
        self.backend.image_created(img, W, H);
        self.backend.set_image_format(img, 37); // R8G8B8A8_UNORM
        self.backend.buffer_created(buf, (W * H * 4) as u64);
        self.backend
            .draw_and_copy_pc(img, buf, vs, fs, 3, [0, 0, 0, 255], push)
            .map_err(|e| BackendError::Unsupported(format!("moltenvk draw failed: {e:?}")))?;
        self.read_center(buf, W, H)
    }

    /// Render `fs` with a UBO bound at (set 0, binding 0).
    fn render_pixel_ubo(&self, vs: &[u8], fs: &[u8], ubo: &[u8]) -> Result<[f32; 4], BackendError> {
        const W: u32 = 2;
        const H: u32 = 2;
        let img = ResourceId::new(IdNamespace::IcdRuntime, self.fresh());
        let buf = ResourceId::new(IdNamespace::IcdRuntime, self.fresh());
        self.backend.image_created(img, W, H);
        self.backend.set_image_format(img, 37);
        self.backend.buffer_created(buf, (W * H * 4) as u64);
        self.backend
            .draw_and_copy_full(img, buf, vs, fs, 3, [0, 0, 0, 255], &[], Some(ubo), None, false)
            .map_err(|e| BackendError::Unsupported(format!("moltenvk ubo draw failed: {e:?}")))?;
        self.read_center(buf, W, H)
    }

    /// Render `fs` with a sampled texture bound at (set 0, binding 0).
    fn render_pixel_tex(
        &self, vs: &[u8], fs: &[u8], tex: aqueduct_gpu_host::TexBind,
    ) -> Result<[f32; 4], BackendError> {
        const W: u32 = 2;
        const H: u32 = 2;
        let img = ResourceId::new(IdNamespace::IcdRuntime, self.fresh());
        let buf = ResourceId::new(IdNamespace::IcdRuntime, self.fresh());
        self.backend.image_created(img, W, H);
        self.backend.set_image_format(img, 37);
        self.backend.buffer_created(buf, (W * H * 4) as u64);
        self.backend
            .draw_and_copy_full(img, buf, vs, fs, 3, [0, 0, 0, 255], &[], None, Some(tex), false)
            .map_err(|e| BackendError::Unsupported(format!("moltenvk tex draw failed: {e:?}")))?;
        self.read_center(buf, W, H)
    }

    fn read_center(&self, buf: ResourceId, w: u32, h: u32) -> Result<[f32; 4], BackendError> {
        let px = self
            .backend
            .buffer_read_bytes(buf, 0, (w * h * 4) as u64)
            .map_err(BackendError::Unsupported)?;
        Ok([px[0] as f32 / 255.0, px[1] as f32 / 255.0, px[2] as f32 / 255.0, px[3] as f32 / 255.0])
    }
}

#[test]
fn moltenvk_agrees_with_the_interpreter_oracle_on_a_constant_shader() {
    let Some(mvk) = MoltenVkShaderRunner::new() else {
        eprintln!("cross-tier shaded probe skipped (MoltenVK unavailable)");
        return;
    };
    let fs = build_constant_color_fs([0.7, 0.2, 0.5, 1.0]);
    let inputs = ShaderInputs::default();
    let runners: [&dyn ShaderRunner; 2] = [&InterpreterRunner, &mvk];
    // 8-bit UNORM readback quantises f32 → tolerate ~2 LSB.
    assert_shader_agrees(&fs, &inputs, ColorTolerance::AbsEpsilon { eps: 0.01 }, &runners);
}

#[test]
fn moltenvk_agrees_with_the_interpreter_oracle_on_a_varying_shader() {
    // Rung 2: a passthrough FS that reads a Location-0 vec4 varying and
    // outputs it. The MoltenVK runner bakes each invocation's varying into
    // the VS; Metal's FS reads it back — and agrees with the interpreter.
    let Some(mvk) = MoltenVkShaderRunner::new() else {
        eprintln!("cross-tier shaded probe skipped (MoltenVK unavailable)");
        return;
    };
    let fs = build_passthrough_varying_fs();
    let colors = [[1.0f32, 0.0, 0.0, 1.0], [0.0, 1.0, 0.0, 1.0], [0.25, 0.5, 0.75, 1.0]];
    let inputs = ShaderInputs {
        varyings_per_invocation: colors
            .iter()
            .map(|c| c.iter().flat_map(|f| f.to_le_bytes()).collect())
            .collect(),
        ..ShaderInputs::default()
    };
    let runners: [&dyn ShaderRunner; 2] = [&InterpreterRunner, &mvk];
    assert_shader_agrees(&fs, &inputs, ColorTolerance::AbsEpsilon { eps: 0.01 }, &runners);
}

#[test]
fn moltenvk_agrees_with_the_interpreter_oracle_on_a_push_constant_shader() {
    // Rung 3: a uniform fed as a push constant. The FS reads a vec4 from the
    // push-constant block and outputs it; the runner pushes it to Metal
    // (VERTEX|FRAGMENT). Agrees with the interpreter oracle.
    let Some(mvk) = MoltenVkShaderRunner::new() else {
        eprintln!("cross-tier shaded probe skipped (MoltenVK unavailable)");
        return;
    };
    let fs = build_push_constant_fs();
    let mut pc = [0u8; 128];
    for (i, f) in [0.3f32, 0.6, 0.9, 1.0].iter().enumerate() {
        pc[i * 4..i * 4 + 4].copy_from_slice(&f.to_le_bytes());
    }
    let inputs = ShaderInputs { push_constants: pc, ..ShaderInputs::default() };
    let runners: [&dyn ShaderRunner; 2] = [&InterpreterRunner, &mvk];
    assert_shader_agrees(&fs, &inputs, ColorTolerance::AbsEpsilon { eps: 0.01 }, &runners);
}

#[test]
fn moltenvk_agrees_with_the_interpreter_oracle_on_a_ubo_shader() {
    // Rung 4a: a uniform delivered as a UBO at (set 0, binding 0) — the
    // first descriptor-backed cross-tier check. The runner creates a uniform
    // buffer + descriptor set on Metal; the FS reads the block and outputs
    // it. Agrees with the interpreter oracle.
    let Some(mvk) = MoltenVkShaderRunner::new() else {
        eprintln!("cross-tier shaded probe skipped (MoltenVK unavailable)");
        return;
    };
    let fs = build_ubo_fs();
    let color = [0.2f32, 0.4, 0.8, 1.0];
    let uniforms: Vec<u8> = color.iter().flat_map(|f| f.to_le_bytes()).collect();
    let inputs = ShaderInputs { uniforms, ..ShaderInputs::default() };
    let runners: [&dyn ShaderRunner; 2] = [&InterpreterRunner, &mvk];
    assert_shader_agrees(&fs, &inputs, ColorTolerance::AbsEpsilon { eps: 0.01 }, &runners);
}

#[test]
fn moltenvk_agrees_with_the_interpreter_oracle_on_a_texture_shader() {
    // Rung 4b: a sampled texture. A 2×2 RGBA checkerboard sampled at the
    // centre (0.5, 0.5) with bilinear/clamp → the four-texel mean. Tests
    // that Metal's sampler convention matches the interpreter's — the real
    // unknown of this rung.
    let Some(mvk) = MoltenVkShaderRunner::new() else {
        eprintln!("cross-tier shaded probe skipped (MoltenVK unavailable)");
        return;
    };
    let fs = build_sample_centre_fs();
    let pixels: Vec<u8> = vec![
        255, 0, 0, 255, 0, 255, 0, 255, // red, green
        0, 0, 255, 255, 255, 255, 255, 255, // blue, white
    ];
    let texture = atrium_spv_tests::interpreter::TextureBinding {
        set: 0, binding: 0, data: pixels, width: 2, height: 2, stride_bytes: 8,
        format: 0, // Rgba8Unorm
        sampler: atrium_spv_runtime::SamplerDesc {
            mag_filter: 1, min_filter: 1, // Linear
            wrap_s: 0, wrap_t: 0,         // ClampToEdge
            compare_enable: 0, compare_op: 0,
        },
    };
    let inputs = ShaderInputs { textures: vec![texture], ..ShaderInputs::default() };
    let runners: [&dyn ShaderRunner; 2] = [&InterpreterRunner, &mvk];
    // Sampling rounds through 8-bit + bilinear → a slightly looser epsilon.
    assert_shader_agrees(&fs, &inputs, ColorTolerance::AbsEpsilon { eps: 0.02 }, &runners);
}

/// Non-panicking shaded certifier: run `fs` through the interpreter oracle
/// and MoltenVK for `inputs`, compare per-channel, and return a
/// `Certification` to seed the runtime registry. (`assert_shader_agrees`
/// panics — this is its verdict-returning sibling for the offline certifier.)
fn shaded_certify(
    mvk: &MoltenVkShaderRunner, fs: &[u8], inputs: &ShaderInputs, eps: f32,
) -> aqueduct_gpu_host::Certification {
    use aqueduct_gpu_host::Certification;
    let (a, b) = match (InterpreterRunner.run(fs, inputs), mvk.run(fs, inputs)) {
        (Ok(a), Ok(b)) => (a, b),
        _ => return Certification::Failed { max_channel_diff: 255 },
    };
    if a.pixels.len() != b.pixels.len() || a.pixels.is_empty() {
        return Certification::Failed { max_channel_diff: 255 };
    }
    let mut maxd = 0.0f32;
    for (pa, pb) in a.pixels.iter().zip(&b.pixels) {
        for c in 0..4 {
            maxd = maxd.max((pa[c] - pb[c]).abs());
        }
    }
    if maxd <= eps {
        Certification::Certified
    } else {
        Certification::Failed { max_channel_diff: (maxd * 255.0) as u8 }
    }
}

#[test]
fn offline_shaded_cert_seeds_the_routing_registry() {
    // The full path: certify a shaded pipeline cross-tier (interpreter ↔
    // MoltenVK), feed the verdict to the router's registry, and watch the
    // surface using that pipeline become migration-eligible.
    use aqueduct_gpu::frame::FrameBuilder;
    use aqueduct_gpu::opcodes::FrameOp;
    use aqueduct_gpu_host::{
        Backend, Certification, CpuProfile, DeviceProfile, GpuPowerModel, RouteMode,
        RoutingBackend, StubBackend,
    };
    use std::sync::Arc;

    let Some(mvk) = MoltenVkShaderRunner::new() else {
        eprintln!("offline shaded cert skipped (MoltenVK unavailable)");
        return;
    };

    // Offline certification of a varying shader.
    let fs = build_passthrough_varying_fs();
    let colors = [[1.0f32, 0.0, 0.0, 1.0], [0.0, 1.0, 0.0, 1.0]];
    let inputs = ShaderInputs {
        varyings_per_invocation: colors
            .iter()
            .map(|c| c.iter().flat_map(|f| f.to_le_bytes()).collect())
            .collect(),
        ..ShaderInputs::default()
    };
    let cert = shaded_certify(&mvk, &fs, &inputs, 0.01);
    assert_eq!(cert, Certification::Certified, "the shaded pipeline is tier-equivalent");

    // Seed the router's registry with the offline verdict.
    let rb = RoutingBackend::new(
        Arc::new(StubBackend::new()), Arc::new(StubBackend::new()),
        DeviceProfile::uma_apple_m4_max(), CpuProfile::apple_m4_max(),
        GpuPowerModel::apple_m4_max(), RouteMode::Perf);
    let img = ResourceId::new(IdNamespace::IcdRuntime, 1);
    let pipe = ResourceId::new(IdNamespace::IcdRuntime, 2);
    rb.image_created(img, 64, 64);
    rb.pipeline_created(pipe, &build_fullscreen_tri_vs(), &fs);
    rb.certify(pipe, cert);

    // A frame using the (now-certified) pipeline is scored, not pinned.
    let mut fb = FrameBuilder::new(1024);
    let mut brp = img.raw().to_le_bytes().to_vec();
    brp.extend_from_slice(&[0, 0, 0, 255]);
    brp.extend_from_slice(&0u32.to_le_bytes());
    fb.push(FrameOp::BeginRenderPass, &brp).unwrap();
    fb.push(FrameOp::BindPipeline, &pipe.raw().to_le_bytes()).unwrap();
    let mut draw = 3u32.to_le_bytes().to_vec();
    draw.extend_from_slice(&1u32.to_le_bytes());
    draw.extend_from_slice(&[0u8; 8]);
    fb.push(FrameOp::Draw, &draw).unwrap();
    rb.submit_frame(ResourceId::new(IdNamespace::IcdRuntime, 9), 1, fb.as_bytes());
    assert_eq!(rb.decision_stats(), (1, 0),
        "the certified pipeline's surface is eligible → scored, not pinned");
}

// ── SPIR-V builders ──────────────────────────────────────────────────

/// A fragment shader that samples a `sampler2D` at (set 0, binding 0) at the
/// constant UV (0.5, 0.5) and writes the result to Location-0 output.
fn build_sample_centre_fs() -> Vec<u8> {
    use rspirv::binary::Assemble;
    use rspirv::dr::Operand;
    use rspirv::spirv::{
        AddressingModel, Capability, Decoration, Dim, ExecutionMode, ExecutionModel,
        FunctionControl, ImageFormat, MemoryModel, StorageClass,
    };
    let mut b = rspirv::dr::Builder::new();
    b.set_version(1, 0);
    b.capability(Capability::Shader);
    b.memory_model(AddressingModel::Logical, MemoryModel::GLSL450);
    let void = b.type_void();
    let f32t = b.type_float(32, None);
    let vec2 = b.type_vector(f32t, 2);
    let vec4 = b.type_vector(f32t, 4);
    let void_fn = b.type_function(void, vec![]);
    let image = b.type_image(f32t, Dim::Dim2D, 0, 0, 0, 1, ImageFormat::Unknown, None);
    let sampled = b.type_sampled_image(image);
    let ptr_uc = b.type_pointer(None, StorageClass::UniformConstant, sampled);
    let ptr_out = b.type_pointer(None, StorageClass::Output, vec4);
    let tex = b.variable(ptr_uc, None, StorageClass::UniformConstant, None);
    b.decorate(tex, Decoration::DescriptorSet, vec![Operand::LiteralBit32(0)]);
    b.decorate(tex, Decoration::Binding, vec![Operand::LiteralBit32(0)]);
    let out = b.variable(ptr_out, None, StorageClass::Output, None);
    b.decorate(out, Decoration::Location, vec![Operand::LiteralBit32(0)]);
    let ch = b.constant_bit32(f32t, 0.5f32.to_bits());
    let uv = b.constant_composite(vec2, vec![ch, ch]);
    let main = b.begin_function(void, None, FunctionControl::NONE, void_fn).unwrap();
    b.begin_block(None).unwrap();
    let s = b.load(sampled, None, tex, None, vec![]).unwrap();
    let px = b.image_sample_implicit_lod(vec4, None, s, uv, None, vec![]).unwrap();
    b.store(out, px, None, vec![]).unwrap();
    b.ret().unwrap();
    b.end_function().unwrap();
    b.entry_point(ExecutionModel::Fragment, main, "main", vec![tex, out]);
    b.execution_mode(main, ExecutionMode::OriginUpperLeft, vec![]);
    let words: Vec<u32> = b.module().assemble();
    words.iter().flat_map(|w| w.to_le_bytes()).collect()
}

/// A fragment shader that reads a vec4 from a UBO `struct { vec4 }` at
/// (set 0, binding 0, member 0) and writes it to Location-0 output.
fn build_ubo_fs() -> Vec<u8> {
    use rspirv::binary::Assemble;
    use rspirv::dr::Operand;
    use rspirv::spirv::{
        AddressingModel, Capability, Decoration, ExecutionMode, ExecutionModel, FunctionControl,
        MemoryModel, StorageClass,
    };
    let mut b = rspirv::dr::Builder::new();
    b.set_version(1, 0);
    b.capability(Capability::Shader);
    b.memory_model(AddressingModel::Logical, MemoryModel::GLSL450);
    let void = b.type_void();
    let f32t = b.type_float(32, None);
    let i32t = b.type_int(32, 1);
    let vec4 = b.type_vector(f32t, 4);
    let void_fn = b.type_function(void, vec![]);
    let blk = b.type_struct(vec![vec4]);
    b.member_decorate(blk, 0, Decoration::Offset, vec![Operand::LiteralBit32(0)]);
    b.decorate(blk, Decoration::Block, vec![]);
    let ptr_ub_blk = b.type_pointer(None, StorageClass::Uniform, blk);
    let ptr_ub_v4 = b.type_pointer(None, StorageClass::Uniform, vec4);
    let ptr_out = b.type_pointer(None, StorageClass::Output, vec4);
    let ub = b.variable(ptr_ub_blk, None, StorageClass::Uniform, None);
    b.decorate(ub, Decoration::DescriptorSet, vec![Operand::LiteralBit32(0)]);
    b.decorate(ub, Decoration::Binding, vec![Operand::LiteralBit32(0)]);
    let out = b.variable(ptr_out, None, StorageClass::Output, None);
    b.decorate(out, Decoration::Location, vec![Operand::LiteralBit32(0)]);
    let c0i = b.constant_bit32(i32t, 0);
    let main = b.begin_function(void, None, FunctionControl::NONE, void_fn).unwrap();
    b.begin_block(None).unwrap();
    let ac = b.access_chain(ptr_ub_v4, None, ub, vec![c0i]).unwrap();
    let val = b.load(vec4, None, ac, None, vec![]).unwrap();
    b.store(out, val, None, vec![]).unwrap();
    b.ret().unwrap();
    b.end_function().unwrap();
    b.entry_point(ExecutionModel::Fragment, main, "main", vec![ub, out]);
    b.execution_mode(main, ExecutionMode::OriginUpperLeft, vec![]);
    let words: Vec<u32> = b.module().assemble();
    words.iter().flat_map(|w| w.to_le_bytes()).collect()
}

/// A fragment shader that reads a vec4 from a push-constant block (member 0)
/// and writes it to Location-0 output.
fn build_push_constant_fs() -> Vec<u8> {
    use rspirv::binary::Assemble;
    use rspirv::dr::Operand;
    use rspirv::spirv::{
        AddressingModel, Capability, Decoration, ExecutionMode, ExecutionModel, FunctionControl,
        MemoryModel, StorageClass,
    };
    let mut b = rspirv::dr::Builder::new();
    b.set_version(1, 0);
    b.capability(Capability::Shader);
    b.memory_model(AddressingModel::Logical, MemoryModel::GLSL450);
    let void = b.type_void();
    let f32t = b.type_float(32, None);
    let i32t = b.type_int(32, 1);
    let vec4 = b.type_vector(f32t, 4);
    let void_fn = b.type_function(void, vec![]);
    let blk = b.type_struct(vec![vec4]);
    b.decorate(blk, Decoration::Block, vec![]);
    b.member_decorate(blk, 0, Decoration::Offset, vec![Operand::LiteralBit32(0)]);
    let ptr_pc_blk = b.type_pointer(None, StorageClass::PushConstant, blk);
    let pc = b.variable(ptr_pc_blk, None, StorageClass::PushConstant, None);
    let ptr_pc_v4 = b.type_pointer(None, StorageClass::PushConstant, vec4);
    let ptr_out = b.type_pointer(None, StorageClass::Output, vec4);
    let out = b.variable(ptr_out, None, StorageClass::Output, None);
    b.decorate(out, Decoration::Location, vec![Operand::LiteralBit32(0)]);
    let c0i = b.constant_bit32(i32t, 0);
    let main = b.begin_function(void, None, FunctionControl::NONE, void_fn).unwrap();
    b.begin_block(None).unwrap();
    let ac = b.access_chain(ptr_pc_v4, None, pc, vec![c0i]).unwrap();
    let val = b.load(vec4, None, ac, None, vec![]).unwrap();
    b.store(out, val, None, vec![]).unwrap();
    b.ret().unwrap();
    b.end_function().unwrap();
    b.entry_point(ExecutionModel::Fragment, main, "main", vec![out]);
    b.execution_mode(main, ExecutionMode::OriginUpperLeft, vec![]);
    let words: Vec<u32> = b.module().assemble();
    words.iter().flat_map(|w| w.to_le_bytes()).collect()
}

fn build_constant_color_fs(rgba: [f32; 4]) -> Vec<u8> {
    use rspirv::binary::Assemble;
    use rspirv::spirv::{
        AddressingModel, Capability, ExecutionMode, ExecutionModel, FunctionControl, MemoryModel,
        StorageClass,
    };
    let mut b = rspirv::dr::Builder::new();
    b.set_version(1, 0);
    b.capability(Capability::Shader);
    b.memory_model(AddressingModel::Logical, MemoryModel::GLSL450);
    let void = b.type_void();
    let f32_ty = b.type_float(32, None);
    let vec4 = b.type_vector(f32_ty, 4);
    let void_fn = b.type_function(void, vec![]);
    let ptr_out = b.type_pointer(None, StorageClass::Output, vec4);
    let cs: Vec<_> = rgba.iter().map(|x| b.constant_bit32(f32_ty, x.to_bits())).collect();
    let color = b.constant_composite(vec4, cs);
    let out = b.variable(ptr_out, None, StorageClass::Output, None);
    b.decorate(out, rspirv::spirv::Decoration::Location, vec![rspirv::dr::Operand::LiteralBit32(0)]);
    let main = b.begin_function(void, None, FunctionControl::NONE, void_fn).unwrap();
    b.begin_block(None).unwrap();
    b.store(out, color, None, vec![]).unwrap();
    b.ret().unwrap();
    b.end_function().unwrap();
    b.entry_point(ExecutionModel::Fragment, main, "main", vec![out]);
    b.execution_mode(main, ExecutionMode::OriginUpperLeft, vec![]);
    let words: Vec<u32> = b.module().assemble();
    words.iter().flat_map(|w| w.to_le_bytes()).collect()
}

/// A fragment shader that reads a Location-0 vec4 input varying and writes
/// it to Location-0 output (passthrough) — the harness's standard varying
/// probe.
fn build_passthrough_varying_fs() -> Vec<u8> {
    use rspirv::binary::Assemble;
    use rspirv::spirv::{
        AddressingModel, Capability, Decoration, ExecutionMode, ExecutionModel, FunctionControl,
        MemoryModel, StorageClass,
    };
    let mut b = rspirv::dr::Builder::new();
    b.set_version(1, 0);
    b.capability(Capability::Shader);
    b.memory_model(AddressingModel::Logical, MemoryModel::GLSL450);
    let void = b.type_void();
    let f32t = b.type_float(32, None);
    let vec4 = b.type_vector(f32t, 4);
    let void_fn = b.type_function(void, vec![]);
    let ptr_in = b.type_pointer(None, StorageClass::Input, vec4);
    let ptr_out = b.type_pointer(None, StorageClass::Output, vec4);
    let in_color = b.variable(ptr_in, None, StorageClass::Input, None);
    b.decorate(in_color, Decoration::Location, vec![rspirv::dr::Operand::LiteralBit32(0)]);
    let out_color = b.variable(ptr_out, None, StorageClass::Output, None);
    b.decorate(out_color, Decoration::Location, vec![rspirv::dr::Operand::LiteralBit32(0)]);
    let main = b.begin_function(void, None, FunctionControl::NONE, void_fn).unwrap();
    b.begin_block(None).unwrap();
    let c = b.load(vec4, None, in_color, None, vec![]).unwrap();
    b.store(out_color, c, None, vec![]).unwrap();
    b.ret().unwrap();
    b.end_function().unwrap();
    b.entry_point(ExecutionModel::Fragment, main, "main", vec![in_color, out_color]);
    b.execution_mode(main, ExecutionMode::OriginUpperLeft, vec![]);
    let words: Vec<u32> = b.module().assemble();
    words.iter().flat_map(|w| w.to_le_bytes()).collect()
}

/// A full-screen-triangle VS that also emits a constant Location-0 vec4
/// varying (`v`) at every vertex — so the FS sees exactly `v` (interpolating
/// a constant is a no-op), the trick that feeds a known varying to Metal.
fn build_tri_vs_with_varying(v: [f32; 4]) -> Vec<u8> {
    use rspirv::binary::Assemble;
    use rspirv::dr::Operand;
    use rspirv::spirv::{
        AddressingModel, BuiltIn, Capability, Decoration, ExecutionModel, FunctionControl,
        MemoryModel, StorageClass,
    };
    let mut b = rspirv::dr::Builder::new();
    b.set_version(1, 0);
    b.capability(Capability::Shader);
    b.memory_model(AddressingModel::Logical, MemoryModel::GLSL450);
    let void = b.type_void();
    let f32t = b.type_float(32, None);
    let i32t = b.type_int(32, 1);
    let v4 = b.type_vector(f32t, 4);
    let void_fn = b.type_function(void, vec![]);
    let per_vertex = b.type_struct(vec![v4]);
    b.member_decorate(per_vertex, 0, Decoration::BuiltIn, vec![Operand::BuiltIn(BuiltIn::Position)]);
    b.member_decorate(per_vertex, 0, Decoration::Offset, vec![Operand::LiteralBit32(0)]);
    b.decorate(per_vertex, Decoration::Block, vec![]);
    let ptr_pv = b.type_pointer(None, StorageClass::Output, per_vertex);
    let ptr_out_v4 = b.type_pointer(None, StorageClass::Output, v4);
    let ptr_in_i32 = b.type_pointer(None, StorageClass::Input, i32t);
    let in_idx = b.variable(ptr_in_i32, None, StorageClass::Input, None);
    b.decorate(in_idx, Decoration::BuiltIn, vec![Operand::BuiltIn(BuiltIn::VertexIndex)]);
    let pv_var = b.variable(ptr_pv, None, StorageClass::Output, None);
    // The varying output at Location 0.
    let out_var = b.variable(ptr_out_v4, None, StorageClass::Output, None);
    b.decorate(out_var, Decoration::Location, vec![Operand::LiteralBit32(0)]);
    let vc: Vec<_> = v.iter().map(|x| b.constant_bit32(f32t, x.to_bits())).collect();
    let vconst = b.constant_composite(v4, vc);
    let c0i = b.constant_bit32(i32t, 0);
    let c1i = b.constant_bit32(i32t, 1);
    let c2i = b.constant_bit32(i32t, 2);
    let c2f = b.constant_bit32(f32t, 2.0f32.to_bits());
    let c1f = b.constant_bit32(f32t, 1.0f32.to_bits());
    let c0f = b.constant_bit32(f32t, 0.0f32.to_bits());
    let main = b.begin_function(void, None, FunctionControl::NONE, void_fn).unwrap();
    b.begin_block(None).unwrap();
    let idx = b.load(i32t, None, in_idx, None, vec![]).unwrap();
    let sh = b.shift_left_logical(i32t, None, idx, c1i).unwrap();
    let xb = b.bitwise_and(i32t, None, sh, c2i).unwrap();
    let yb = b.bitwise_and(i32t, None, idx, c2i).unwrap();
    let xf = b.convert_s_to_f(f32t, None, xb).unwrap();
    let yf = b.convert_s_to_f(f32t, None, yb).unwrap();
    let xm = b.f_mul(f32t, None, xf, c2f).unwrap();
    let x = b.f_sub(f32t, None, xm, c1f).unwrap();
    let ym = b.f_mul(f32t, None, yf, c2f).unwrap();
    let y = b.f_sub(f32t, None, ym, c1f).unwrap();
    let pos = b.composite_construct(v4, None, vec![x, y, c0f, c1f]).unwrap();
    let dst = b.access_chain(ptr_out_v4, None, pv_var, vec![c0i]).unwrap();
    b.store(dst, pos, None, vec![]).unwrap();
    b.store(out_var, vconst, None, vec![]).unwrap();
    b.ret().unwrap();
    b.end_function().unwrap();
    b.entry_point(ExecutionModel::Vertex, main, "main", vec![in_idx, pv_var, out_var]);
    let words: Vec<u32> = b.module().assemble();
    words.iter().flat_map(|w| w.to_le_bytes()).collect()
}

fn build_fullscreen_tri_vs() -> Vec<u8> {
    use rspirv::binary::Assemble;
    use rspirv::dr::Operand;
    use rspirv::spirv::{
        AddressingModel, BuiltIn, Capability, Decoration, ExecutionModel, FunctionControl,
        MemoryModel, StorageClass,
    };
    let mut b = rspirv::dr::Builder::new();
    b.set_version(1, 0);
    b.capability(Capability::Shader);
    b.memory_model(AddressingModel::Logical, MemoryModel::GLSL450);
    let void = b.type_void();
    let f32t = b.type_float(32, None);
    let i32t = b.type_int(32, 1);
    let v4 = b.type_vector(f32t, 4);
    let void_fn = b.type_function(void, vec![]);
    let per_vertex = b.type_struct(vec![v4]);
    b.member_decorate(per_vertex, 0, Decoration::BuiltIn, vec![Operand::BuiltIn(BuiltIn::Position)]);
    b.member_decorate(per_vertex, 0, Decoration::Offset, vec![Operand::LiteralBit32(0)]);
    b.decorate(per_vertex, Decoration::Block, vec![]);
    let ptr_pv = b.type_pointer(None, StorageClass::Output, per_vertex);
    let ptr_out_v4 = b.type_pointer(None, StorageClass::Output, v4);
    let ptr_in_i32 = b.type_pointer(None, StorageClass::Input, i32t);
    let in_idx = b.variable(ptr_in_i32, None, StorageClass::Input, None);
    b.decorate(in_idx, Decoration::BuiltIn, vec![Operand::BuiltIn(BuiltIn::VertexIndex)]);
    let pv_var = b.variable(ptr_pv, None, StorageClass::Output, None);
    let c0i = b.constant_bit32(i32t, 0);
    let c1i = b.constant_bit32(i32t, 1);
    let c2i = b.constant_bit32(i32t, 2);
    let c2f = b.constant_bit32(f32t, 2.0f32.to_bits());
    let c1f = b.constant_bit32(f32t, 1.0f32.to_bits());
    let c0f = b.constant_bit32(f32t, 0.0f32.to_bits());
    let main = b.begin_function(void, None, FunctionControl::NONE, void_fn).unwrap();
    b.begin_block(None).unwrap();
    let idx = b.load(i32t, None, in_idx, None, vec![]).unwrap();
    let sh = b.shift_left_logical(i32t, None, idx, c1i).unwrap();
    let xb = b.bitwise_and(i32t, None, sh, c2i).unwrap();
    let yb = b.bitwise_and(i32t, None, idx, c2i).unwrap();
    let xf = b.convert_s_to_f(f32t, None, xb).unwrap();
    let yf = b.convert_s_to_f(f32t, None, yb).unwrap();
    let xm = b.f_mul(f32t, None, xf, c2f).unwrap();
    let x = b.f_sub(f32t, None, xm, c1f).unwrap();
    let ym = b.f_mul(f32t, None, yf, c2f).unwrap();
    let y = b.f_sub(f32t, None, ym, c1f).unwrap();
    let pos = b.composite_construct(v4, None, vec![x, y, c0f, c1f]).unwrap();
    let dst = b.access_chain(ptr_out_v4, None, pv_var, vec![c0i]).unwrap();
    b.store(dst, pos, None, vec![]).unwrap();
    b.ret().unwrap();
    b.end_function().unwrap();
    b.entry_point(ExecutionModel::Vertex, main, "main", vec![in_idx, pv_var]);
    let words: Vec<u32> = b.module().assemble();
    words.iter().flat_map(|w| w.to_le_bytes()).collect()
}
