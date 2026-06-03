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
        // Rung 1: only no-input (constant) shaders. Inputs that need
        // per-invocation feeding to Metal come in later rungs.
        if !inputs.varyings_per_invocation.is_empty()
            || !inputs.uniforms.is_empty()
            || !inputs.textures.is_empty()
            || inputs.push_constants.iter().any(|&b| b != 0)
        {
            return Err(BackendError::Unsupported(
                "moltenvk runner rung 1: constant shaders only".into(),
            ));
        }

        const W: u32 = 2;
        const H: u32 = 2;
        let img = ResourceId::new(IdNamespace::IcdRuntime, self.fresh());
        let buf = ResourceId::new(IdNamespace::IcdRuntime, self.fresh());
        self.backend.image_created(img, W, H);
        self.backend.set_image_format(img, 37); // R8G8B8A8_UNORM
        self.backend.buffer_created(buf, (W * H * 4) as u64);

        self.backend
            .draw_and_copy(img, buf, &self.vs, fs_spirv, 3, [0, 0, 0, 255])
            .map_err(|e| BackendError::Unsupported(format!("moltenvk draw failed: {e:?}")))?;

        let px = self
            .backend
            .buffer_read_bytes(buf, 0, (W * H * 4) as u64)
            .map_err(BackendError::Unsupported)?;
        // Constant shader → every pixel identical; take pixel 0.
        let rgba = [
            px[0] as f32 / 255.0,
            px[1] as f32 / 255.0,
            px[2] as f32 / 255.0,
            px[3] as f32 / 255.0,
        ];
        Ok(ShaderOutputs { pixels: vec![rgba] })
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

// ── SPIR-V builders ──────────────────────────────────────────────────

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
