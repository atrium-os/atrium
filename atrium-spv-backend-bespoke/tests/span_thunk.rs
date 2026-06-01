//! P2.2b validation: the bespoke span thunk (`atrium_fs_main_span`)
//! must shade each masked lane identically to a per-lane
//! `atrium_fs_main` call, and leave masked-off lanes untouched.
//!
//! Maps the compiled `.afblob` into an executable region (the same
//! W^X dance the loader's jitmap uses) and calls both entries
//! directly.

use atrium_spv_backend_bespoke::{compile_blob, Target};
use atrium_spv_frontend::translate;
use atrium_spv_blob::ShaderBlob;

type FsMain = unsafe extern "C" fn(
    *const u8, *const u8, *const u8,   // varyings, uniforms, push
    f32, f32, f32, f32,                // frag_coord x/y/z/w
    u32,                               // samples_mask
    *mut f32, *mut f32,                // out_color, out_depth
    u32, u32,                          // front_facing, primitive_id
);
type FsSpanMain = unsafe extern "C" fn(
    *const u8, u32, *const u8, *const u8,        // varyings_soa, stride, uniforms, push
    *const f32, *const f32, *const f32, *const f32, // frag_x/y/z/w ptrs
    u64, u32,                                    // coverage_mask, samples_mask
    *mut f32, *mut f32,                          // out_color_soa, out_depth
    u32, u32, u32,                               // front_facing, primitive_id, lane_count
);

/// Constant-colour fragment shader (the gated span subset: no
/// varyings input, single Location=0 output, no textures/derivs).
fn build_const_fs(rgba: [f32; 4]) -> Vec<u8> {
    use rspirv::binary::Assemble;
    use rspirv::spirv::{
        AddressingModel, Capability, Decoration, ExecutionMode, ExecutionModel,
        FunctionControl, MemoryModel, StorageClass,
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
    b.decorate(out, Decoration::Location, vec![rspirv::dr::Operand::LiteralBit32(0)]);
    let main = b.begin_function(void, None, FunctionControl::NONE, void_fn).unwrap();
    b.begin_block(None).unwrap();
    b.store(out, color, None, vec![]).unwrap();
    b.ret().unwrap();
    b.end_function().unwrap();
    b.entry_point(ExecutionModel::Fragment, main, "main", vec![out]);
    b.execution_mode(main, ExecutionMode::OriginUpperLeft, vec![]);
    let words: Vec<u32> = b.module().assemble();
    let mut bytes = Vec::with_capacity(words.len() * 4);
    for w in words { bytes.extend_from_slice(&w.to_le_bytes()); }
    bytes
}

/// mmap `code` into an executable region (W^X), returning the base.
/// Mirrors the loader's jitmap::map_exec.
#[cfg(not(target_os = "macos"))]
unsafe fn map_exec(code: &[u8]) -> *mut u8 {
    let len = code.len();
    let raw = libc::mmap(std::ptr::null_mut(), len,
        libc::PROT_READ | libc::PROT_WRITE,
        libc::MAP_PRIVATE | libc::MAP_ANON, -1, 0);
    assert!(raw != libc::MAP_FAILED, "mmap RW failed");
    std::ptr::copy_nonoverlapping(code.as_ptr(), raw as *mut u8, len);
    assert!(libc::mprotect(raw, len, libc::PROT_READ | libc::PROT_EXEC) == 0,
        "mprotect RX failed");
    raw as *mut u8
}

#[cfg(target_os = "macos")]
unsafe fn map_exec(code: &[u8]) -> *mut u8 {
    extern "C" {
        fn pthread_jit_write_protect_np(enabled: libc::c_int);
        fn sys_icache_invalidate(start: *mut libc::c_void, len: libc::size_t);
    }
    let len = code.len();
    let raw = libc::mmap(std::ptr::null_mut(), len,
        libc::PROT_READ | libc::PROT_WRITE | libc::PROT_EXEC,
        libc::MAP_PRIVATE | libc::MAP_ANON | libc::MAP_JIT, -1, 0);
    assert!(raw != libc::MAP_FAILED, "mmap MAP_JIT failed");
    pthread_jit_write_protect_np(0);
    std::ptr::copy_nonoverlapping(code.as_ptr(), raw as *mut u8, len);
    pthread_jit_write_protect_np(1);
    sys_icache_invalidate(raw, len);
    raw as *mut u8
}

#[test]
fn span_thunk_matches_per_lane_fs_main() {
    let spv = build_const_fs([1.0, 0.2, 0.3, 1.0]);
    let module = translate(&spv).expect("frontend");
    let target = if cfg!(target_os = "macos") {
        Target::Aarch64Darwin
    } else { Target::Aarch64FreeBSD };
    let out = compile_blob(&module, target).expect("compile_blob");
    let blob = ShaderBlob::from_bytes(&out.blob).expect("parse blob");

    let fs_off = blob.entries.fs.expect("fs entry");
    let span_off = blob.entries.fs_span
        .expect("span entry MUST be emitted for a constant-colour FS");

    let base = unsafe { map_exec(&blob.code) };
    let fs_main: FsMain = unsafe {
        std::mem::transmute(base.add(fs_off as usize))
    };
    let fs_span: FsSpanMain = unsafe {
        std::mem::transmute(base.add(span_off as usize))
    };

    // Reference: call fs_main per lane for the 3 lanes.
    let mut ref_color = [0.0f32; 3 * 4];
    let mut depth = 0.0f32;
    for lane in 0..3 {
        unsafe {
            fs_main(std::ptr::null(), std::ptr::null(), std::ptr::null(),
                lane as f32 + 0.5, 4.5, 0.0, 1.0, 0,
                ref_color.as_mut_ptr().add(lane * 4), &mut depth, 1, 7);
        }
    }

    // Span call: lanes 0 and 2 active (mask = 0b101), lane 1 masked off.
    let fx = [0.5f32, 1.5, 2.5];
    let fy = [4.5f32; 3];
    let fz = [0.0f32; 3];
    let fw = [1.0f32; 3];
    let mut span_color = [-1.0f32; 3 * 4]; // sentinel: masked-off stays -1
    let mut span_depth = [0.0f32; 3];
    unsafe {
        fs_span(std::ptr::null(), 0, std::ptr::null(), std::ptr::null(),
            fx.as_ptr(), fy.as_ptr(), fz.as_ptr(), fw.as_ptr(),
            0b101, 0,
            span_color.as_mut_ptr(), span_depth.as_mut_ptr(),
            1, 7, 3);
    }

    // Active lanes 0,2 match the per-lane fs_main output.
    for lane in [0usize, 2] {
        for c in 0..4 {
            assert_eq!(span_color[lane * 4 + c], ref_color[lane * 4 + c],
                "lane {lane} channel {c}: span {} != fs_main {}",
                span_color[lane * 4 + c], ref_color[lane * 4 + c]);
        }
    }
    // Masked-off lane 1 left untouched (still the -1 sentinel).
    for c in 0..4 {
        assert_eq!(span_color[4 + c], -1.0,
            "masked-off lane 1 channel {c} was written: {}", span_color[4 + c]);
    }
}
